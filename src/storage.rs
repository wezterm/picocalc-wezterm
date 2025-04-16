use crate::byte_size;
use crate::time::WezTermTimeSource;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use embassy_embedded_hal::SetConfig;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIN_16, PIN_17, PIN_18, PIN_19, PIN_22, SPI0};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use embassy_time::{Delay, Duration, Timer};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use embedded_sdmmc::{DirEntry, SdCard, VolumeIdx, VolumeManager};

extern crate alloc;

// Max number of open dirs, files, volumes
const MAX_DIRS: usize = 4;
const MAX_FILES: usize = 4;
const MAX_VOLUMES: usize = 1;

pub static STORAGE: LazyLock<Mutex<CriticalSectionRawMutex, Storage>> =
    LazyLock::new(|| Mutex::new(Storage::PendingInit));

type CardType = SdCard<
    ExclusiveDevice<
        embassy_rp::spi::Spi<'static, SPI0, embassy_rp::spi::Blocking>,
        Output<'static>,
        NoDelay,
    >,
    Delay,
>;
type VolMgr = VolumeManager<CardType, WezTermTimeSource, MAX_DIRS, MAX_FILES, MAX_VOLUMES>;

#[derive(Default)]
pub enum Storage {
    #[default]
    PendingInit,
    NotPlugged(CardType),
    Loaded(VolMgr),
    Unplugged(VolMgr),
}

impl Storage {
    fn mark_loaded(&mut self, vol_mgr: VolMgr) {
        *self = Self::Loaded(vol_mgr);
    }

    fn take(&mut self) -> Self {
        core::mem::take(self)
    }

    fn take_sdcard(&mut self) -> CardType {
        match core::mem::replace(self, Self::PendingInit) {
            Self::NotPlugged(card) => card,
            _ => unreachable!(),
        }
    }

    /// Returns the VolMgr only if the card is initialized
    pub fn vol_mgr(&mut self) -> Option<&mut VolMgr> {
        match self {
            Self::Loaded(vol_mgr) => Some(vol_mgr),
            _ => None,
        }
    }
}

async fn check_card(sd_detect: &Input<'_>) {
    let sd_is_present = sd_detect.get_level() == Level::Low;
    let mut storage = STORAGE.get().lock().await;
    match &mut *storage {
        Storage::PendingInit => {}
        Storage::NotPlugged(sdcard) => {
            if !sd_is_present {
                sdcard.mark_card_uninit();
                return;
            }
            match sdcard.num_bytes() {
                Ok(size) => {
                    log::info!("SD Card size is {size} bytes");
                    // Now that the card is initialized, the SPI clock can go faster
                    let mut config = embassy_rp::spi::Config::default();
                    config.frequency = 16_000_000;
                    sdcard
                        .spi(|dev| SetConfig::set_config(dev.bus_mut(), &config))
                        .ok();

                    // Now let's look for volumes (also known as partitions) on our block device.
                    // To do this we need a Volume Manager. It will take ownership of the block device.
                    let sdcard = storage.take_sdcard();
                    let volume_mgr = VolMgr::new(sdcard, WezTermTimeSource());

                    storage.mark_loaded(volume_mgr);
                }
                Err(err) => {
                    print!("\u{1b}[1mSD Card error: {err:?}\u{1b}[0m\r\n",);
                }
            }
        }
        Storage::Loaded(_) | Storage::Unplugged(_) => {
            let mut volmgr = match storage.take() {
                Storage::Loaded(volmgr) | Storage::Unplugged(volmgr) => volmgr,
                _ => unreachable!(),
            };
            if !sd_is_present {
                log::info!("SD Card unplugged");
                volmgr.device().mark_card_uninit();
                *storage = Storage::Unplugged(volmgr);
            } else {
                match volmgr.device().num_bytes() {
                    Ok(size) => {
                        *storage = Storage::Loaded(volmgr);
                        log::info!("SD Card size is {size} bytes");
                    }
                    Err(err) => {
                        *storage = Storage::Unplugged(volmgr);
                        print!("\u{1b}[1mSD Card error: {err:?}\u{1b}[0m\r\n",);
                    }
                }
            }
        }
    }
}

pub async fn init_storage(
    spawner: &Spawner,
    rx: PIN_16,
    cs: PIN_17,
    sck: PIN_18,
    tx: PIN_19,
    sd_detect: PIN_22,
    spi0: SPI0,
) {
    let mut config = embassy_rp::spi::Config::default();
    // SPI clock needs to be running at <= 400kHz during initialization
    config.frequency = 400_000;
    let sd_detect = Input::new(sd_detect, Pull::Up);

    let spi = embassy_rp::spi::Spi::new_blocking(spi0, sck, tx, rx, config);
    let cs = Output::new(cs, Level::High);
    let spi_dev = ExclusiveDevice::new_no_delay(spi, cs).unwrap();
    let sdcard = SdCard::new(spi_dev, Delay);

    *STORAGE.get().lock().await = Storage::NotPlugged(sdcard);

    check_card(&sd_detect).await;
    spawner.must_spawn(sdcard_hot_plug(sd_detect));

    let mut storage = STORAGE.get().lock().await;
    match &mut *storage {
        Storage::PendingInit | Storage::NotPlugged(_) | Storage::Unplugged(_) => {
            print!("No SD card is present\r\n");
        }
        Storage::Loaded(volmgr) => match volmgr.device().num_bytes() {
            Ok(size) => {
                let mut volumes = String::new();
                for idx in 0..5 {
                    if let Ok(vol) = volmgr.open_volume(embedded_sdmmc::VolumeIdx(idx)) {
                        log::info!("Volume {idx}: {vol:?}");
                        if !volumes.is_empty() {
                            volumes.push(' ');
                        }
                        write!(volumes, "vol{idx}").ok();
                    } else {
                        break;
                    }
                }
                print!("SD card {}, {volumes}\r\n", byte_size(size));
            }
            Err(err) => {
                print!("\u{1b}[1mSD Card error: {err:?}\u{1b}[0m\r\n",);
            }
        },
    }
}

#[embassy_executor::task]
async fn sdcard_hot_plug(mut sd_detect: Input<'static>) {
    loop {
        sd_detect.wait_for_any_edge().await;
        // Give it a moment to de-bounce
        Timer::after(Duration::from_millis(1000)).await;

        // now see whether it is in or out
        check_card(&sd_detect).await;
    }
}

pub async fn ls_command(args: &[&str]) {
    log::debug!("invoked ls with {args:?}\r\n");
    let mut storage = STORAGE.get().lock().await;
    let Some(mgr) = storage.vol_mgr() else {
        print!("No SD card is present\r\n");
        return;
    };

    let mut vol = match mgr.open_volume(VolumeIdx(0)) {
        Ok(vol) => vol,
        Err(err) => {
            print!("Failed to open vol0: {err:?}\r\n");
            return;
        }
    };

    let mut dir = match vol.open_root_dir() {
        Ok(dir) => dir,
        Err(err) => {
            print!("Failed to open root dir on vol0: {err:?}\r\n");
            return;
        }
    };

    let path = args.get(1).copied().unwrap_or("");
    let (dirs, entry_name) = match path.rsplit_once('/') {
        Some((dirs, entry_name)) => (Some(dirs), entry_name),
        None => (None, path),
    };

    if let Some(dirs) = dirs {
        for comp in dirs.split('/') {
            match dir.change_dir(comp) {
                Ok(()) => {}
                Err(err) => {
                    print!("Failed to open {comp} in {dirs}: {err:?}\r\n");
                    return;
                }
            }
        }
    }

    async fn print_entry(entry: &DirEntry) {
        let mut attrs = String::new();
        write!(attrs, "{:?}", entry.attributes).ok();
        let mut size = String::new();
        write!(size, "{}", byte_size(entry.size)).ok();
        let (size, unit) = size.split_once(' ').unwrap_or((&size, ""));
        let mut name = String::new();
        write!(name, "{}", entry.name).ok();

        print!("{attrs:<3} {size:>7} {unit:<3} {name}\r\n");
    }

    if !entry_name.is_empty() {
        match dir.find_directory_entry(entry_name) {
            Ok(entry) => {
                if entry.attributes.is_directory() {
                    dir.change_dir(entry_name).ok();
                } else {
                    print_entry(&entry).await;
                    return;
                }
            }
            Err(err) => {
                print!("Failed to find {entry_name} in {dirs:?}: {err:?}\r\n");
                return;
            }
        }
    }

    // Just iterate the directory
    let mut dirs = Vec::new();
    dir.iterate_dir(|entry| {
        dirs.push(entry.clone());
    })
    .ok();
    dirs.sort_by(|a, b| a.name.base_name().cmp(b.name.base_name()));
    for entry in dirs {
        print_entry(&entry).await;
    }
}
