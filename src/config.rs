use crate::fixed_str::FixedString;
use embassy_rp::flash::{
    Async, ERASE_SIZE, Error as FlashError, Flash as RpFlash, PAGE_SIZE, WRITE_SIZE,
};
use embassy_rp::peripherals::{DMA_CH3, FLASH};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use embedded_io::ErrorKind;
use heapless::FnvIndexMap;
use sequential_storage::cache::NoCache;
use sequential_storage::erase_all;
use sequential_storage::map::{fetch_all_items, fetch_item, remove_item, store_item};

const PICO2_FLASH_SIZE: usize = 4 * 1024 * 1024;
pub const CONFIG_SIZE: u32 = ERASE_SIZE as u32 * 2;
pub const CONFIG_BASE: u32 = PICO2_FLASH_SIZE as u32 - CONFIG_SIZE;
const SCRATCH_SIZE: usize = PAGE_SIZE * 2;

pub static CONFIG: LazyLock<Mutex<CriticalSectionRawMutex, Configuration>> =
    LazyLock::new(|| Mutex::new(Configuration::default()));

#[derive(Debug, Default)]
pub struct Configuration {
    flash: Option<Flash>,
}

pub type StrKey = FixedString<32>;
pub type StrValue = FixedString<128>;

impl Configuration {
    pub fn assign_flash(&mut self, flash: Flash) {
        self.flash.replace(flash);
    }

    pub async fn fetch(
        &mut self,
        key: &str,
    ) -> Result<Option<StrValue>, sequential_storage::Error<embassy_rp::flash::Error>> {
        match &mut self.flash {
            Some(flash) => {
                let key: StrKey = key.try_into()?;
                let mut buf = [0u8; SCRATCH_SIZE];
                fetch_item(
                    &mut flash.flash,
                    CONFIG_BASE..CONFIG_BASE + CONFIG_SIZE,
                    &mut NoCache::new(),
                    &mut buf,
                    &key,
                )
                .await
            }
            None => {
                todo!();
            }
        }
    }

    pub async fn remove(
        &mut self,
        key: &str,
    ) -> Result<(), sequential_storage::Error<embassy_rp::flash::Error>> {
        match &mut self.flash {
            Some(flash) => {
                let key: StrKey = key.try_into()?;
                let mut buf = [0u8; SCRATCH_SIZE];
                remove_item(
                    &mut flash.flash,
                    CONFIG_BASE..CONFIG_BASE + CONFIG_SIZE,
                    &mut NoCache::new(),
                    &mut buf,
                    &key,
                )
                .await
            }
            None => {
                todo!();
            }
        }
    }

    pub async fn store(
        &mut self,
        key: &str,
        value: StrValue,
    ) -> Result<(), sequential_storage::Error<embassy_rp::flash::Error>> {
        match &mut self.flash {
            Some(flash) => {
                let key: StrKey = key.try_into()?;
                let mut buf = [0u8; SCRATCH_SIZE];
                store_item(
                    &mut flash.flash,
                    CONFIG_BASE..CONFIG_BASE + CONFIG_SIZE,
                    &mut NoCache::new(),
                    &mut buf,
                    &key,
                    &value,
                )
                .await
            }
            None => {
                todo!();
            }
        }
    }

    pub async fn format(
        &mut self,
    ) -> Result<(), sequential_storage::Error<embassy_rp::flash::Error>> {
        match &mut self.flash {
            Some(flash) => {
                erase_all(&mut flash.flash, CONFIG_BASE..CONFIG_BASE + CONFIG_SIZE).await
            }
            None => {
                todo!();
            }
        }
    }

    pub async fn get_all(
        &mut self,
    ) -> Result<
        FnvIndexMap<StrKey, StrValue, 32>,
        sequential_storage::Error<embassy_rp::flash::Error>,
    > {
        match &mut self.flash {
            Some(flash) => {
                let mut buf = [0u8; SCRATCH_SIZE];
                let mut cache = NoCache::new();
                let mut iter = fetch_all_items::<StrKey, _, _>(
                    &mut flash.flash,
                    CONFIG_BASE..CONFIG_BASE + CONFIG_SIZE,
                    &mut cache,
                    &mut buf,
                )
                .await?;

                let mut map = FnvIndexMap::new();

                while let Some((key, value)) = iter.next::<StrKey, StrValue>(&mut buf).await? {
                    if let Err((k, v)) = map.insert(key, value) {
                        print!("Configuration::get_all: too many keys. Ignoring {k} -> {v}\r\n");
                    }
                }

                Ok(map)
            }
            None => {
                todo!();
            }
        }
    }
}

pub struct Flash {
    flash: RpFlash<'static, FLASH, Async, PICO2_FLASH_SIZE>,
}

impl core::fmt::Debug for Flash {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        fmt.debug_struct("Flash").finish()
    }
}

impl Flash {
    pub fn new(flash: FLASH, dma: DMA_CH3) -> Self {
        let flash = embassy_rp::flash::Flash::new(flash, dma);
        log::info!(
            "flash capacity={}, write={}, erase={}",
            flash.capacity(),
            WRITE_SIZE,
            ERASE_SIZE
        );
        Self { flash }
    }

    #[allow(unused)]
    pub async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), FlashError> {
        self.flash.read(offset, bytes).await
    }
}

#[derive(Debug)]
pub struct EmbeddedFlashError(FlashError);

impl From<FlashError> for EmbeddedFlashError {
    fn from(err: FlashError) -> Self {
        Self(err)
    }
}

impl embedded_io::Error for EmbeddedFlashError {
    fn kind(&self) -> ErrorKind {
        match self.0 {
            FlashError::OutOfBounds => ErrorKind::InvalidInput,
            FlashError::Unaligned => ErrorKind::InvalidInput,
            FlashError::InvalidCore => ErrorKind::InvalidInput,
            FlashError::Other => ErrorKind::Other,
        }
    }
}

pub async fn config_command(args: &[&str]) {
    match args {
        ["config", "format"] => {
            let mut config = CONFIG.get().lock().await;
            let result = config.format().await;
            print!("{result:?}");
        }
        ["config", "list"] => {
            let mut config = CONFIG.get().lock().await;
            match config.get_all().await {
                Ok(map) => {
                    for (k, v) in &map {
                        print!("{k}={v}\r\n");
                    }
                }
                Err(err) => {
                    print!("{err:?}\r\n");
                }
            }
        }
        ["config", "get", key] => {
            let mut config = CONFIG.get().lock().await;
            let value = config.fetch(key).await;
            print!("{value:?}\r\n");
        }
        ["config", "rm", key] => {
            let mut config = CONFIG.get().lock().await;
            let result = config.remove(key).await;
            print!("{result:?}\r\n");
        }
        ["config", "set", key, value] => {
            let value: StrValue = match (*value).try_into() {
                Ok(v) => v,
                Err(err) => {
                    print!("value `{value}`: {err:?}\r\n");
                    return;
                }
            };
            let mut config = CONFIG.get().lock().await;
            match config.store(key, value).await {
                Ok(()) => {
                    print!("OK\r\n");
                }
                Err(err) => {
                    print!("{err:?}\r\n");
                }
            }
        }
        _ => {
            print!("invalid arguments\r\n");
        }
    }
}
