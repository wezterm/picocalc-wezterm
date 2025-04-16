use chrono::{DateTime, Datelike, Timelike, Utc};
use core::net::{IpAddr, SocketAddr};
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use sntpc::{NtpContext, NtpResult, NtpTimestampGenerator, get_time};

// This module keeps track of the wall clock time.
// The rp2350 has an AON time source that can be used
// to reliably keep track of the real time, but
// which has no persistence once power is removed.
//
// That means that the embassy_time::Instant type has
// reliable intervals, and that we just need to associate
// a given Instant with the current real time.
//
// What we do here is spawn a task to periodically
// poll an NTP server to determine the current date/time
// for a given Instant.
//
// That allows us to provide a UnixTime type and associated
// UnixTime::now() method to return the current unix time.

/// This type is used to expose the current time to the
/// embedded_sdmmc crate
pub struct WezTermTimeSource();

impl embedded_sdmmc::TimeSource for WezTermTimeSource {
    fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
        let now = UnixTime::now();
        let chrono = now.as_chrono();
        let date = chrono.date_naive();
        let time = chrono.time();
        embedded_sdmmc::Timestamp {
            year_since_1970: (date.year() - 1970) as u8,
            zero_indexed_month: date.month0() as u8,
            zero_indexed_day: date.day0() as u8,
            hours: time.hour() as u8,
            minutes: time.minute() as u8,
            seconds: time.second() as u8,
        }
    }
}

/// Represents a time relative to the Unix Epoch
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct UnixTime {
    pub seconds: u64,
    pub useconds: u32,
}

impl UnixTime {
    /// Returns the current unix time.
    pub fn now() -> Self {
        match TIME.get().try_lock() {
            Ok(time) => {
                let elapsed = time.instant.elapsed();

                let mut seconds = time.unix.seconds.saturating_add(elapsed.as_secs() as u64);
                let remainder = elapsed - Duration::from_secs(elapsed.as_secs());
                let mut useconds = time
                    .unix
                    .useconds
                    .saturating_add(remainder.as_micros() as u32);
                while useconds > 1_000_000 {
                    seconds += 1;
                    useconds -= 1_000_000;
                }

                UnixTime { seconds, useconds }
            }
            Err(_) => Self::default(),
        }
    }

    /// Convert the time into a chrono type for more convenient
    /// manipulation by humans
    pub fn as_chrono(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.seconds as i64, self.useconds * 1000)
            .expect("failed to map UnixTime to chrono")
    }
}

pub struct Rfc3339(pub DateTime<Utc>);

impl core::fmt::Display for Rfc3339 {
    fn fmt(&self, w: &mut core::fmt::Formatter) -> core::fmt::Result {
        let date = self.0.date_naive();
        let year = date.year();
        if (0..=9999).contains(&year) {
            write!(w, "{year:04}")?;
        } else {
            // ISO 8601 requires the explicit sign for out-of-range years
            write!(w, "{year:+05}")?;
        }

        let (hour, min, mut sec) = {
            let time = self.0.time();
            (time.hour(), time.minute(), time.second())
        };

        if self.0.nanosecond() >= 1_000_000_000 {
            sec += 1;
        }

        write!(
            w,
            "-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            date.month() as u8,
            date.day() as u8,
            hour as u8,
            min as u8,
            sec as u8
        )
    }
}

/// Tracks "The Time" as we know it
struct TheTime {
    unix: UnixTime,
    instant: Instant,
}

impl TheTime {
    pub fn new() -> Self {
        Self {
            unix: UnixTime::default(),
            instant: Instant::now(),
        }
    }

    pub fn update_from_ntp(&mut self, now: Instant, ntp: NtpResult) {
        self.instant = now;
        self.unix.seconds = ntp.sec() as u64;
        self.unix.useconds = ntp.sec_fraction() * 1_000_000 / u32::MAX;
    }
}

static TIME: LazyLock<Mutex<CriticalSectionRawMutex, TheTime>> =
    LazyLock::new(|| Mutex::new(TheTime::new()));

/// Enables sntpc to get our idea of the current time
#[derive(Copy, Clone, Default)]
struct Timestamp {
    now: UnixTime,
}

impl NtpTimestampGenerator for Timestamp {
    fn init(&mut self) {
        self.now = UnixTime::now();
    }

    fn timestamp_sec(&self) -> u64 {
        self.now.seconds
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        self.now.useconds
    }
}

#[embassy_executor::task]
pub async fn time_sync(stack: Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY; 8];
    let mut rx_buffer = [0; 512];
    let mut tx_meta = [PacketMetadata::EMPTY; 8];
    let mut tx_buffer = [0; 512];

    const NTP_SERVER: &str = "pool.ntp.org";

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(123).expect("failed to bind port 123!?");

    let context = NtpContext::new(Timestamp::default());

    let mut first = true;

    loop {
        let ntp_addrs = match stack.dns_query(NTP_SERVER, DnsQueryType::A).await {
            Ok(ntp_addrs) => ntp_addrs,
            Err(err) => {
                log::error!("dns_query {NTP_SERVER} failed: {err:?}");
                Timer::after(Duration::from_secs(15)).await;
                continue;
            }
        };

        if ntp_addrs.is_empty() {
            log::error!("{NTP_SERVER} resolved to no addresses!");
            Timer::after(Duration::from_secs(15)).await;
            continue;
        }

        let mut sync_interval = Duration::from_secs(15);

        for _ in 0..120 {
            let mut updated = false;
            for &addr in &ntp_addrs {
                let addr: IpAddr = addr.into();
                let result = get_time(SocketAddr::from((addr, 123)), &socket, context).await;

                match result {
                    Ok(time) => {
                        let now = Instant::now();
                        TIME.get().lock().await.update_from_ntp(now, time);

                        let now_ts = UnixTime::now();
                        let rfc3339 = Rfc3339(now_ts.as_chrono());

                        let offset = Duration::from_micros(time.offset.abs() as u64);
                        if first {
                            first = false;
                            print!("The time is {rfc3339}\r\n");
                        }

                        log::info!("{rfc3339} drift={}us", offset.as_micros());

                        if offset < Duration::from_secs(1) {
                            // While we have good sync, we can poll less frequently
                            sync_interval = (sync_interval * 2).min(Duration::from_secs(1024));
                        } else {
                            sync_interval = Duration::from_secs(15);
                        }
                        updated = true;
                        break;
                    }
                    Err(err) => {
                        log::error!("Error getting time: {err:?}");
                    }
                }
            }

            if !updated {
                // Try again a bit sooner if we repeatedly experience
                // connectivity issues
                sync_interval = (sync_interval / 2).max(Duration::from_secs(15));
            }
            log::info!("Next time sync in {}", sync_interval.as_secs());
            Timer::after(sync_interval).await;
        }
    }
}

pub async fn time_command(_args: &[&str]) {
    let now_ts = UnixTime::now();
    let rfc3339 = Rfc3339(now_ts.as_chrono());
    print!("The time is {rfc3339}\r\n");
}
