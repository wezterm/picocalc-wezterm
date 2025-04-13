use crate::Irqs;
use embassy_rp::peripherals::TRNG;
use embassy_rp::trng::Trng;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::once_lock::OnceLock;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_core::RngCore;
use static_cell::StaticCell;
use sunset::random::{CryptoRngProvider, EphemeralSecret, SigningKey};

static RNG: OnceLock<Mutex<CriticalSectionRawMutex, Trng<TRNG>>> = OnceLock::new();

pub fn init_rng(trng: TRNG) {
    if RNG
        .init(Mutex::new(Trng::new(
            trng,
            Irqs,
            embassy_rp::trng::Config::default(),
        )))
        .is_err()
    {
        panic!("failed to init Trng");
    }

    // Stash a reference for sunset to use
    static SUNSET_RNG: StaticCell<RngProvider> = StaticCell::new();
    let rng = SUNSET_RNG.init_with(|| RngProvider);
    unsafe {
        sunset::random::assign_rng(rng);
    }
}

/// Our Rng type. It internally manages mutual exclusion around
/// the underlying TRNG hardware.
pub struct WezTermRng;
impl rand_core::RngCore for WezTermRng {
    fn next_u32(&mut self) -> u32 {
        RNG.try_get().unwrap().try_lock().unwrap().next_u32()
    }
    fn next_u64(&mut self) -> u64 {
        RNG.try_get().unwrap().try_lock().unwrap().next_u64()
    }
    fn fill_bytes(&mut self, buf: &mut [u8]) {
        rand_core::RngCore::fill_bytes(&mut *RNG.try_get().unwrap().try_lock().unwrap(), buf)
    }
    fn try_fill_bytes(&mut self, buf: &mut [u8]) -> Result<(), rand_core::Error> {
        RNG.try_get()
            .unwrap()
            .try_lock()
            .unwrap()
            .try_fill_bytes(buf)
    }
}

struct RngProvider;
impl RngProvider {
    fn make_rng(&self) -> Result<ChaCha20Rng, sunset::Error> {
        let mut rng = WezTermRng;
        ChaCha20Rng::from_rng(&mut rng)
            .map_err(|_| sunset::Error::msg("failed to init ChaCha20Rng"))
    }
}
impl CryptoRngProvider for RngProvider {
    fn fill_random(&self, buf: &mut [u8]) -> Result<(), sunset::Error> {
        let mut rng = self.make_rng()?;
        rng.fill_bytes(buf);
        Ok(())
    }

    fn make_ephemeral_secret(&self) -> Result<EphemeralSecret, sunset::Error> {
        let mut rng = self.make_rng()?;
        Ok(EphemeralSecret::random_from_rng(&mut rng))
    }

    fn make_signing_key(&self) -> Result<SigningKey, sunset::Error> {
        let mut rng = self.make_rng()?;
        Ok(SigningKey::generate(&mut rng))
    }
}
