#![no_std]
#![no_main]

use crate::config::{Configuration, Flash};
use crate::keyboard::set_lcd_backlight;
use crate::psram::init_psram;
use crate::screen::SCREEN;
use core::cell::RefCell;
use core::fmt::Write as _;
use cyw43::Control;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_embedded_hal::SetConfig;
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_rp::block::ImageDef;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIO0, PIO1, SPI1, UART0, USB};
use embassy_rp::pio::Pio;
use embassy_rp::spi::Spi;
use embassy_rp::uart::BufferedInterruptHandler;
use embassy_rp::watchdog::Watchdog;
use embassy_rp::{bind_interrupts, spi, usb};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Delay, Duration, Instant, Ticker, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_sdmmc::sdcard::SdCard;
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9488Rgb565;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation};
use panic_persist as _;
use rand::RngCore;
use static_cell::StaticCell;

type PicoCalcDisplay<'a> = mipidsi::Display<
    SpiInterface<
        'a,
        embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig<
            'a,
            NoopRawMutex,
            embassy_rp::spi::Spi<'a, SPI1, embassy_rp::spi::Blocking>,
            Output<'a>,
        >,
        Output<'a>,
    >,
    ILI9488Rgb565,
    Output<'a>,
>;

mod config;
mod keyboard;
mod logging;
mod psram;
mod screen;

const SCREEN_HEIGHT: u16 = 320;
const SCREEN_WIDTH: u16 = 320;

#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: ImageDef = ImageDef::secure_exe();

#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"WezTerm"),
    embassy_rp::binary_info::rp_program_description!(c"Hardware WezTerm"),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
    I2C1_IRQ => embassy_rp::i2c::InterruptHandler<embassy_rp::peripherals::I2C1>;
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

mod task {
    use cyw43_pio::PioSpi;
    use embassy_rp::gpio::Output;
    use embassy_rp::peripherals::{DMA_CH0, PIO0};

    #[embassy_executor::task]
    pub async fn cyw43(
        runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
    ) -> ! {
        runner.run().await
    }
}

#[embassy_executor::task]
async fn watchdog_task(mut watchdog: Watchdog) {
    if let Some(reason) = watchdog.reset_reason() {
        log::error!("Watchdog reset reason: {reason:?}");
        write!(SCREEN.get().lock().await, "Reset reason: {reason:?}\r\n").ok();
    }

    watchdog.start(Duration::from_secs(3));

    let mut ticker = Ticker::every(Duration::from_secs(2));
    loop {
        watchdog.feed();
        ticker.next().await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    crate::logging::setup_logging(
        &spawner,
        p.PIN_0,
        p.PIN_1,
        p.UART0,
        usb::Driver::new(p.USB, Irqs),
    )
    .await;

    SCREEN.get().lock().await.print("WezTerm\r\n");
    if let Some(msg) = panic_persist::get_panic_message_utf8() {
        log::error!("prior panic: {msg}");
        write!(SCREEN.get().lock().await, "Panic: {msg}\r\n").ok();
    }
    spawner.must_spawn(watchdog_task(Watchdog::new(p.WATCHDOG)));

    let mut i2c_config = embassy_rp::i2c::Config::default();
    i2c_config.frequency = 400_000;
    let scl = p.PIN_7;
    let sda = p.PIN_6;
    let mut i2c_bus = embassy_rp::i2c::I2c::new_async(p.I2C1, scl, sda, Irqs, i2c_config);

    let miso = p.PIN_12;
    let mosi = p.PIN_11;
    let sclk = p.PIN_10;
    let dcx = p.PIN_14;
    let display_cs = p.PIN_13;
    let rst = p.PIN_15;

    // create SPI
    let mut display_config = spi::Config::default();
    display_config.frequency = 50_000_000;
    display_config.phase = spi::Phase::CaptureOnSecondTransition;
    display_config.polarity = spi::Polarity::IdleHigh;

    static DISPLAY_SPI_BUS: StaticCell<
        Mutex<NoopRawMutex, RefCell<Spi<SPI1, embassy_rp::spi::Blocking>>>,
    > = StaticCell::new();
    let spi = Spi::new_blocking(p.SPI1, sclk, mosi, miso, display_config.clone());

    let display_spi = SpiDeviceWithConfig::new(
        DISPLAY_SPI_BUS.init_with(|| Mutex::new(RefCell::new(spi))),
        Output::new(display_cs, Level::High),
        display_config,
    );

    let dcx = Output::new(dcx, Level::Low);
    let rst = Output::new(rst, Level::Low);
    // dcx: 0 = command, 1 = data

    // Enable LCD backlight
    set_lcd_backlight(&mut i2c_bus, 0x80).await;

    // display interface abstraction from SPI and DC
    const DISPLAY_BUFFER_SIZE: usize = 320 * 3 * 320;
    static DISPLAY_BUFFER: StaticCell<[u8; DISPLAY_BUFFER_SIZE]> = StaticCell::new();
    let di = SpiInterface::new(
        display_spi,
        dcx,
        DISPLAY_BUFFER.init_with(|| [0u8; DISPLAY_BUFFER_SIZE]),
    );

    // Define the display from the display interface and initialize it
    let display = Builder::new(ILI9488Rgb565, di)
        .color_order(ColorOrder::Bgr)
        .display_size(SCREEN_WIDTH, SCREEN_HEIGHT)
        .reset_pin(rst)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().flip_horizontal())
        .init(&mut Delay)
        .unwrap();
    spawner.must_spawn(screen_painter(display));

    spawner.must_spawn(crate::keyboard::keyboard_reader(i2c_bus));

    Timer::after(Duration::from_millis(100)).await;

    let mut flash = Flash::new(p.FLASH, p.DMA_CH3);
    let wezterm_config = {
        match Configuration::load(&mut flash) {
            Ok(config) => {
                log::info!("Loaded configuration: {config:#?}");
                config
            }
            Err(err) => {
                log::error!("Failed to load config: {err:?}");
                let mut config = Configuration::default();
                config.ssid.push_str("SECRET").ok();
                config.wifi_pw.push_str("SECRET").ok();
                if false {
                    // To bootstrap the config
                    match config.save(&mut flash) {
                        Ok(()) => {
                            log::info!("Wrote configuration!");
                        }
                        Err(err) => {
                            log::error!("Failed to write config: {err:?}");
                        }
                    }
                }
                config
            }
        }
    };

    let wifi_control = setup_wifi(
        &spawner,
        p.PIN_23,
        p.PIN_24,
        p.PIN_25,
        p.PIN_29,
        p.PIO0,
        p.DMA_CH0,
        &wezterm_config,
    )
    .await;
    // spawner.must_spawn(wifi_scanner(wifi_control));

    {
        struct DummyTimesource();

        impl embedded_sdmmc::TimeSource for DummyTimesource {
            fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
                embedded_sdmmc::Timestamp {
                    year_since_1970: 0,
                    zero_indexed_month: 0,
                    zero_indexed_day: 0,
                    hours: 0,
                    minutes: 0,
                    seconds: 0,
                }
            }
        }

        let mut config = embassy_rp::spi::Config::default();
        // SPI clock needs to be running at <= 400kHz during initialization
        config.frequency = 400_000;
        let spi = embassy_rp::spi::Spi::new_blocking(p.SPI0, p.PIN_18, p.PIN_19, p.PIN_16, config);
        let cs = Output::new(p.PIN_17, Level::High);
        let spi_dev = ExclusiveDevice::new_no_delay(spi, cs).unwrap();

        let sdcard = SdCard::new(spi_dev, embassy_time::Delay);
        log::info!("Card size is {:?} bytes", sdcard.num_bytes());

        // Now that the card is initialized, the SPI clock can go faster
        let mut config = spi::Config::default();
        config.frequency = 16_000_000;
        sdcard
            .spi(|dev| SetConfig::set_config(dev.bus_mut(), &config))
            .ok();

        // Now let's look for volumes (also known as partitions) on our block device.
        // To do this we need a Volume Manager. It will take ownership of the block device.
        let mut volume_mgr = embedded_sdmmc::VolumeManager::new(sdcard, DummyTimesource());

        // Try and access Volume 0 (i.e. the first partition).
        // The volume object holds information about the filesystem on that volume.
        if let Ok(mut volume0) = volume_mgr.open_volume(embedded_sdmmc::VolumeIdx(0)) {
            log::info!("Volume 0: {:?}", volume0);

            // Open the root directory (mutably borrows from the volume).
            let mut root_dir = volume0.open_root_dir().unwrap();
            root_dir
                .iterate_dir(|entry| {
                    log::info!("entry - {}", entry.name);
                })
                .ok();
        }
    }

    Timer::after(Duration::from_secs(10)).await;

    let psram = init_psram(
        p.PIO1, p.PIN_21, p.PIN_2, p.PIN_3, p.PIN_20, p.DMA_CH1, p.DMA_CH2,
    )
    .await;

    let mut ticker = Ticker::every(Duration::from_millis(100));
    loop {
        ticker.next().await;
    }
}

#[embassy_executor::task]
async fn screen_painter(mut display: PicoCalcDisplay<'static>) {
    // Display update takes ~128ms @ 40_000_000
    let mut ticker = Ticker::every(Duration::from_millis(200));
    display.clear(Rgb565::BLACK).unwrap();
    let mut last = Instant::now();
    loop {
        log::trace!("slept {}ms", last.elapsed().as_millis());
        last = Instant::now();
        SCREEN.get().lock().await.update_display(&mut display);
        log::trace!("paint took {}ms", last.elapsed().as_millis());
        last = Instant::now();
        ticker.next().await;
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

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

async fn setup_wifi(
    spawner: &Spawner,
    pin_23: embassy_rp::peripherals::PIN_23,
    pin_24: embassy_rp::peripherals::PIN_24,
    pin_25: embassy_rp::peripherals::PIN_25,
    pin_29: embassy_rp::peripherals::PIN_29,
    pio_0: embassy_rp::peripherals::PIO0,
    dma_ch0: embassy_rp::peripherals::DMA_CH0,
    wezterm_config: &Configuration,
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

    spawner.must_spawn(task::cyw43(runner));
    control.init(clm).await;
    use embassy_net::StackResources;
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();

    let mut rng = RoscRng;
    let seed = rng.next_u64();

    let config = embassy_net::Config::dhcpv4(Default::default());
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.must_spawn(net_task(runner));

    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    write!(
        SCREEN.get().lock().await,
        "Connecting to {}...\r\n",
        wezterm_config.ssid,
    )
    .ok();
    loop {
        match control
            .join(
                &wezterm_config.ssid,
                cyw43::JoinOptions::new(wezterm_config.wifi_pw.as_bytes()),
            )
            .await
        {
            Ok(_) => break,
            Err(err) => {
                log::error!("join failed with status={}", err.status);
                write!(
                    SCREEN.get().lock().await,
                    "Failed with status {}\r\n",
                    err.status
                )
                .ok();
            }
        }
    }

    log::info!("waiting for TCP to be up...");
    stack.wait_config_up().await;
    log::info!("Stack is up!");
    if let Some(v4) = stack.config_v4() {
        log::info!("{v4:?}");
        write!(SCREEN.get().lock().await, "IP Address {}\r\n", v4.address).ok();
    }

    control
}
