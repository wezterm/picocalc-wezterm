use crate::fixed_str::FixedString;
use crate::storage::STORAGE;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use embedded_sdmmc::VolumeIdx;
use heapless::FnvIndexMap;

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

const CONFIG_FILENAME: &str = "WEZTERM.CFG";

pub static CONFIG: LazyLock<Mutex<CriticalSectionRawMutex, Configuration>> =
    LazyLock::new(|| Mutex::new(Configuration::default()));

#[derive(Debug, Default)]
pub struct Configuration {
    cache: FnvIndexMap<StrKey, StrValue, 32>,
    dirty: bool,
    loaded: bool,
}

pub type StrKey = FixedString<32>;
pub type StrValue = FixedString<128>;

#[derive(Debug)]
pub enum ConfigError {
    NoSdCard,
    SdError(embedded_sdmmc::Error<embedded_sdmmc::SdCardError>),
    ParseError,
    IoError,
}

impl From<embedded_sdmmc::Error<embedded_sdmmc::SdCardError>> for ConfigError {
    fn from(err: embedded_sdmmc::Error<embedded_sdmmc::SdCardError>) -> Self {
        Self::SdError(err)
    }
}

impl Configuration {
    async fn load_from_sd(&mut self) -> Result<(), ConfigError> {
        let mut storage = STORAGE.get().lock().await;
        let Some(mgr) = storage.vol_mgr() else {
            return Err(ConfigError::NoSdCard);
        };

        let mut vol = mgr.open_volume(VolumeIdx(0))?;
        let mut root_dir = vol.open_root_dir()?;

        let buffer_result: Result<Vec<u8>, ConfigError> = {
            match root_dir.open_file_in_dir(CONFIG_FILENAME, embedded_sdmmc::Mode::ReadOnly) {
                Ok(mut file) => {
                    let mut buffer = Vec::new();

                    loop {
                        let mut chunk = [0u8; 512];
                        match file.read(&mut chunk) {
                            Ok(0) => break, // EOF
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                            Err(e) => {
                                file.close()?;
                                return Err(e.into());
                            }
                        }
                    }

                    file.close()?;
                    Ok(buffer)
                }
                Err(embedded_sdmmc::Error::NotFound) => Ok(Vec::new()),
                Err(e) => Err(e.into()),
            }
        };

        root_dir.close()?;

        match buffer_result {
            Ok(buffer) => {
                if buffer.is_empty() {
                    self.cache.clear();
                    self.dirty = false;
                    self.loaded = true;
                    return Ok(());
                }

                self.cache.clear();
                let content = String::from_utf8_lossy(&buffer);
                for line in content.lines() {
                    if let Some((key, value)) = line.split_once('=') {
                        if let (Ok(k), Ok(v)) = (key.trim().try_into(), value.trim().try_into()) {
                            let _ = self.cache.insert(k, v);
                        }
                    }
                }

                self.dirty = false;
                self.loaded = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn save_to_sd(&mut self) -> Result<(), ConfigError> {
        if !self.dirty {
            return Ok(());
        }

        let mut storage = STORAGE.get().lock().await;
        let Some(mgr) = storage.vol_mgr() else {
            return Err(ConfigError::NoSdCard);
        };

        let mut vol = mgr.open_volume(VolumeIdx(0))?;
        let mut root_dir = vol.open_root_dir()?;

        let mut content = String::new();
        for (key, value) in &self.cache {
            content.push_str(key.as_str());
            content.push('=');
            content.push_str(value.as_str());
            content.push('\n');
        }

        let data = content.as_bytes();

        let _ = root_dir.delete_file_in_dir(CONFIG_FILENAME);

        let write_result: Result<(), ConfigError> = {
            match root_dir.open_file_in_dir(
                CONFIG_FILENAME,
                embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
            ) {
                Ok(mut file) => {
                    let result = match file.write(data) {
                        Ok(_) => Ok(()),
                        Err(e) => Err(e.into()),
                    };
                    file.close()?;
                    result
                }
                Err(e) => Err(e.into()),
            }
        };

        root_dir.close()?;

        if write_result.is_ok() {
            self.dirty = false;
        }

        write_result
    }

    pub async fn fetch(&mut self, key: &str) -> Result<Option<StrValue>, ConfigError> {
        if !self.loaded {
            let _ = self.load_from_sd().await;
        }

        let key: StrKey = key.try_into().map_err(|_| ConfigError::ParseError)?;
        Ok(self.cache.get(&key).cloned())
    }

    pub async fn remove(&mut self, key: &str) -> Result<(), ConfigError> {
        if !self.loaded {
            let _ = self.load_from_sd().await;
        }

        let key: StrKey = key.try_into().map_err(|_| ConfigError::ParseError)?;
        if self.cache.remove(&key).is_some() {
            self.dirty = true;
            self.save_to_sd().await?;
        }
        Ok(())
    }

    pub async fn store(&mut self, key: &str, value: StrValue) -> Result<(), ConfigError> {
        if !self.loaded {
            let _ = self.load_from_sd().await;
        }

        let key: StrKey = key.try_into().map_err(|_| ConfigError::ParseError)?;
        self.cache.insert(key, value).ok();
        self.dirty = true;
        self.save_to_sd().await?;
        Ok(())
    }

    pub async fn format(&mut self) -> Result<(), ConfigError> {
        self.cache.clear();
        self.dirty = true;
        self.loaded = true;
        self.save_to_sd().await
    }

    pub async fn get_all(&mut self) -> Result<FnvIndexMap<StrKey, StrValue, 32>, ConfigError> {
        if !self.loaded {
            let _ = self.load_from_sd().await;
        }

        Ok(self.cache.clone())
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
