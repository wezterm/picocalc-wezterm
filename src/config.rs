use embassy_rp::flash::{Async, ERASE_SIZE, Error as FlashError, Flash as RpFlash, WRITE_SIZE};
use embassy_rp::peripherals::{DMA_CH3, FLASH};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use embedded_io::ErrorKind;
use heapless::String;
use postcard::{Deserializer, serialize_with_flavor};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const PICO2_FLASH_SIZE: usize = 4 * 1024 * 1024;
pub const CONFIG_SIZE: u32 = 4096;
pub const CONFIG_BASE: u32 = PICO2_FLASH_SIZE as u32 - CONFIG_SIZE;

pub static CONFIG: LazyLock<Mutex<CriticalSectionRawMutex, Configuration>> =
    LazyLock::new(|| Mutex::new(Configuration::default()));

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Configuration {
    pub ssid: String<32>,
    pub wifi_pw: String<32>,
    pub ssh_pw: String<32>,
}

impl Configuration {
    pub fn load_in_place(&mut self, flash: &mut Flash) -> Result<(), postcard::Error> {
        let config = Self::load(flash)?;
        *self = config;
        Ok(())
    }

    pub fn load(flash: &mut Flash) -> Result<Self, postcard::Error> {
        flash.deserialize::<Self, 128>(CONFIG_BASE)
    }

    pub fn save(&self, flash: &mut Flash) -> Result<(), postcard::Error> {
        flash.serialize::<Self, 128>(CONFIG_BASE, self)
    }
}

pub struct Flash {
    flash: RpFlash<'static, FLASH, Async, PICO2_FLASH_SIZE>,
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

    pub fn deserialize<T: DeserializeOwned, const BUF_SIZE: usize>(
        &mut self,
        offset: u32,
    ) -> Result<T, postcard::Error> {
        use postcard::de_flavors::crc::CrcModifier;
        use postcard::de_flavors::io::eio::EIOReader;
        let mut buf = [0u8; BUF_SIZE];
        let reader = EIOReader::new(self.cursor(offset), &mut buf);
        let crc = crc::Crc::<u16>::new(&crc::CRC_16_USB);
        let digest = crc.digest();
        let mut deserializer = Deserializer::from_flavor(CrcModifier::new(reader, digest));
        T::deserialize(&mut deserializer)
    }

    pub fn serialize<T: Serialize, const BUF_SIZE: usize>(
        &mut self,
        offset: u32,
        value: &T,
    ) -> Result<(), postcard::Error> {
        use postcard::ser_flavors::Slice;
        use postcard::ser_flavors::crc::CrcModifier;

        let mut buf = [0u8; BUF_SIZE];
        let crc = crc::Crc::<u16>::new(&crc::CRC_16_USB);
        let digest = crc.digest();

        let serialized_slice =
            serialize_with_flavor(value, CrcModifier::new(Slice::new(&mut buf), digest))?;

        log::info!(
            "Serialized as {} bytes in {serialized_slice:x?}",
            serialized_slice.len()
        );

        assert!(
            serialized_slice.len() <= CONFIG_SIZE as usize,
            "{} is larger than CONFIG_SIZE {CONFIG_SIZE}",
            serialized_slice.len()
        );

        if let Err(err) = self.flash.blocking_erase(offset, offset + CONFIG_SIZE) {
            log::error!("Flash blocking erase of {CONFIG_SIZE} bytes @ {offset} failed: {err:?}",);
            return Err(postcard::Error::SerdeSerCustom);
        }

        match self.flash.blocking_write(offset, serialized_slice) {
            Err(err) => {
                log::error!(
                    "Flash blocking write of {} bytes @ {offset} failed: {err:?}",
                    serialized_slice.len()
                );
                Err(postcard::Error::SerdeSerCustom)
            }
            Ok(()) => Ok(()),
        }
    }

    pub fn cursor(&mut self, offset: u32) -> Cursor {
        Cursor {
            offset,
            flash: self,
        }
    }
}

pub struct Cursor<'a> {
    flash: &'a mut Flash,
    offset: u32,
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

impl<'a> embedded_io::ErrorType for Cursor<'a> {
    type Error = EmbeddedFlashError;
}

impl<'a> embedded_io::Read for Cursor<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, EmbeddedFlashError> {
        self.flash.flash.blocking_read(self.offset, buf)?;
        self.offset += buf.len() as u32;
        Ok(buf.len())
    }
}
