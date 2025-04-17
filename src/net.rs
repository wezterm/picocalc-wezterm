use crate::config::CONFIG;
use crate::rng::WezTermRng;
use crate::screen::SCREEN;
use crate::{Irqs, SCREEN_HEIGHT, SCREEN_WIDTH, mk_static, static_bytes};
use core::fmt::Write;
use cyw43::Control;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_net::dns::{DnsQueryType, DnsSocket};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Stack};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use embedded_io_async::Read;
use rand_core::RngCore;
use static_cell::StaticCell;
use sunset::{CliEvent, SessionCommand};
use sunset_embassy::{ChanInOut, ProgressHolder, SSHClient};

#[embassy_executor::task]
pub async fn run_cyw43(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_runner(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

pub async fn setup_wifi(
    spawner: &Spawner,
    pin_23: embassy_rp::peripherals::PIN_23,
    pin_24: embassy_rp::peripherals::PIN_24,
    pin_25: embassy_rp::peripherals::PIN_25,
    pin_29: embassy_rp::peripherals::PIN_29,
    pio_0: embassy_rp::peripherals::PIO0,
    dma_ch0: embassy_rp::peripherals::DMA_CH0,
) -> Control<'static> {
    let fw = include_bytes!("../embassy/cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../embassy/cyw43-firmware/43439A0_clm.bin");

    // Wireless background task:
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, mut control, runner) = {
        let state = STATE.init(cyw43::State::new());
        let wireless_enable = Output::new(pin_23, Level::Low);
        let wireless_spi = {
            let cs = Output::new(pin_25, Level::High);
            let mut pio = Pio::new(pio_0, Irqs);
            PioSpi::new(
                &mut pio.common,
                pio.sm0,
                RM2_CLOCK_DIVIDER,
                pio.irq0,
                cs,
                pin_24,
                pin_29,
                dma_ch0,
            )
        };
        cyw43::new(state, wireless_enable, wireless_spi, fw).await
    };

    spawner.must_spawn(run_cyw43(runner));
    control.init(clm).await;
    use embassy_net::StackResources;
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();

    let config = embassy_net::Config::dhcpv4(Default::default());
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        WezTermRng.next_u64(),
    );
    spawner.must_spawn(net_runner(runner));

    control
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;

    let (ssid, wifi_pw) = {
        let config = CONFIG.get().lock().await;
        (config.ssid.clone(), config.wifi_pw.clone())
    };
    print!("Connecting to \u{1b}[1m{ssid}\u{1b}[0m...\r\n");
    loop {
        match control
            .join(&ssid, cyw43::JoinOptions::new(wifi_pw.as_bytes()))
            .await
        {
            Ok(_) => break,
            Err(err) => {
                log::error!("join failed with status={}", err.status);
                print!("Failed with status {}\r\n", err.status);
            }
        }
    }

    log::info!("waiting for TCP to be up...");
    stack.wait_config_up().await;
    log::info!("Stack is up!");
    if let Some(v4) = stack.config_v4() {
        log::info!("{v4:?}");
        print!("IP Address {}\r\n", v4.address);
    }

    spawner.must_spawn(crate::time::time_sync(stack));
    // spawner.must_spawn(ssh_session_task(stack));

    control
}

#[embassy_executor::task]
async fn ssh_channel_task(mut channel: ChanInOut<'static, 'static>) {
    /*
    let winch = {
        let screen = SCREEN.get().lock().await;
        let rows = screen.height;
        let cols = screen.width;
        sunset::packets::WinChange {
            rows: rows.into(),
            cols: cols.into(),
            width: SCREEN_WIDTH as u32,
            height: SCREEN_HEIGHT as u32,
        }
    };
    log::info!("sending window size {winch:?}");
    if let Err(err) = channel.term_window_change(winch).await {
        log::error!("winch failed: {err:?}");
    }

    log::info!("sending uname command");
    if let Err(err) = channel.write_all("uname -a\r\n".as_bytes()).await {
        log::error!("error sending command: {err:?}");
    }
    */

    log::info!("ssh_channel_task waiting for output");
    loop {
        let mut buf = [0u8; 1024];
        match channel.read(&mut buf).await {
            Ok(n) => {
                if n == 0 {
                    log::warn!("EOF on ssh channel");
                    break;
                }
                match core::str::from_utf8(&buf[0..n]) {
                    Ok(s) => {
                        log::info!("{s}");
                        write!(SCREEN.get().lock().await, "{s}").ok();
                    }
                    Err(err) => {
                        log::error!("failed utf8: {err:?}");
                    }
                }
            }
            Err(err) => {
                log::error!("failed read: {err:?}");
                break;
            }
        }
    }

    loop {
        Timer::after(Duration::from_millis(1000)).await;
    }
}

#[embassy_executor::task]
async fn ssh_session_task(stack: Stack<'static>) {
    let dns_client = DnsSocket::new(stack);

    let host = "foo.lan";

    write!(SCREEN.get().lock().await, "$ ssh {host}\r\n").ok();

    match dns_client.query(host, DnsQueryType::A).await {
        Ok(addrs) => {
            log::info!("{host} -> {addrs:?}");
            let mut tcp_socket = TcpSocket::new(stack, static_bytes!(8192), static_bytes!(8192));

            match tcp_socket
                .connect(IpEndpoint {
                    addr: addrs[0],
                    port: 22,
                })
                .await
            {
                Ok(()) => {
                    use embassy_futures::join::join;
                    log::info!("Connected to port 22!");
                    write!(
                        SCREEN.get().lock().await,
                        "Connected to {host} {}:22\r\n",
                        addrs[0]
                    )
                    .ok();
                    let (mut read, mut write) = tcp_socket.split();
                    let ssh_client = mk_static!(
                        SSHClient,
                        SSHClient::new(static_bytes!(8192), static_bytes!(8192))
                            .expect("SSHClient::new")
                    );

                    let session_authd_chan =
                        embassy_sync::channel::Channel::<NoopRawMutex, (), 1>::new();
                    let wait_for_auth = session_authd_chan.receiver();

                    let spawn_session_future = async {
                        let _ = wait_for_auth.receive().await;

                        log::info!("try open pty");
                        let channel = ssh_client.open_session_pty().await.expect("openpty failed");
                        log::info!("pty opened, spawn client task");
                        Spawner::for_current_executor()
                            .await
                            .must_spawn(ssh_channel_task(channel));
                    };

                    let runner = ssh_client.run(&mut read, &mut write);
                    let mut progress = ProgressHolder::new();
                    let ssh_pw = CONFIG.get().lock().await.ssh_pw.clone();
                    let ssh_ticker = async {
                        loop {
                            match ssh_client.progress(&mut progress).await {
                                Ok(event) => match event {
                                    CliEvent::Hostkey(k) => {
                                        log::info!("host key {:?}", k.hostkey());
                                        k.accept().expect("accept hostkey");
                                    }
                                    CliEvent::Banner(b) => {
                                        if let Ok(b) = b.banner() {
                                            log::info!("banner: {b}");
                                        }
                                    }
                                    CliEvent::Username(req) => {
                                        req.username("wez").expect("set user");
                                    }
                                    CliEvent::Password(req) => {
                                        req.password(&ssh_pw).expect("set pw");
                                    }
                                    CliEvent::Pubkey(req) => {
                                        req.skip().expect("skip pubkey");
                                    }
                                    CliEvent::AgentSign(req) => {
                                        req.skip().expect("skip agentsign");
                                    }
                                    CliEvent::Authenticated => {
                                        log::info!("Authenticated!");
                                        session_authd_chan.sender().send(()).await;
                                    }
                                    CliEvent::SessionOpened(mut s) => {
                                        log::info!("session opened channel {}", s.channel());

                                        use heapless::{String, Vec};

                                        let mut term = String::<32>::new();
                                        let _ = term.push_str("xterm").unwrap();

                                        let pty = {
                                            let screen = SCREEN.get().lock().await;
                                            let rows = screen.height;
                                            let cols = screen.width;

                                            sunset::Pty {
                                                term,
                                                rows: rows.into(),
                                                cols: cols.into(),
                                                width: SCREEN_WIDTH as u32,
                                                height: SCREEN_HEIGHT as u32,
                                                modes: Vec::new(),
                                            }
                                        };

                                        log::info!("requesting pty {pty:?}");
                                        if let Err(err) = s.pty(pty) {
                                            log::error!("requesting pty failed {err:?}");
                                        }
                                        log::info!("setting command");
                                        let command = "uname -a";
                                        write!(SCREEN.get().lock().await, "execute {command}\r\n")
                                            .ok();
                                        if let Err(err) = s.cmd(&SessionCommand::Exec(command)) {
                                            log::error!("command failed: {err:?}");
                                        }
                                        log::info!("SessionOpened completed");
                                    }
                                    CliEvent::SessionExit(x) => {
                                        log::info!("session exit with {x:?}");
                                    }
                                    CliEvent::Defunct => {
                                        log::error!("ssh session terminated");
                                        break;
                                    }
                                },
                                Err(err) => {
                                    log::error!("ssh progress error: {err:?}");
                                    break;
                                }
                            }
                        }

                        Ok::<(), ()>(())
                    };

                    let res = join(runner, join(ssh_ticker, spawn_session_future)).await;
                    log::info!("ssh result is {res:?}");
                }
                Err(err) => {
                    log::error!("failed to connect to port 22: {err:?}");
                }
            }
        }
        Err(err) => {
            log::error!("failed foo.lan: {err:?}");
        }
    }
}

/*
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use heapless::{FnvIndexSet, String};
type WifiSet = FnvIndexSet<String<32>, 16>;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
static NETWORKS: LazyLock<AsyncMutex<CriticalSectionRawMutex, WifiSet>> =
    LazyLock::new(|| AsyncMutex::new(WifiSet::new()));

#[embassy_executor::task]
async fn wifi_scanner(mut control: Control<'static>) {
    let mut scanner = control.scan(Default::default()).await;

    while let Some(bss) = scanner.next().await {
        if bss.ssid_len == 0 {
            continue;
        }
        if let Ok(ssid_str) = core::str::from_utf8(&bss.ssid[0..bss.ssid_len as usize]) {
            if let Ok(ssid) = String::try_from(ssid_str) {
                if let Ok(true) = NETWORKS.get().lock().await.insert(ssid) {
                    log::info!("wifi: {ssid_str} = {:x?}", bss.bssid);
                    write!(SCREEN.get().lock().await, "wifi: {ssid_str}\r\n",).ok();
                }
            }
        }
    }
}
*/
