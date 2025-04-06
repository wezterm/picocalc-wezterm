#![no_std]
#![no_main]

use core::cell::RefCell;
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_rp::block::ImageDef;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::pio::{self};
use embassy_rp::spi::Spi;
use embassy_rp::{bind_interrupts, spi, usb};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Delay, Timer};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9488Rgb565;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation};
use panic_halt as _;

#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: ImageDef = ImageDef::secure_exe();

// Program metadata for `picotool info`.
// This isn't needed, but it's recomended to have these minimal entries.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"Blinky Example"),
    embassy_rp::binary_info::rp_program_description!(
        c"This example tests the RP Pico on board LED, connected to gpio 25"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
});

mod task {
    use cyw43_pio::PioSpi;
    use embassy_rp::gpio::Output;
    use embassy_rp::peripherals::{DMA_CH0, PIO0, USB};
    use embassy_rp::usb;
    use embassy_usb::UsbDevice;

    #[embassy_executor::task]
    pub async fn cyw43(
        runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
    ) -> ! {
        runner.run().await
    }

    #[embassy_executor::task]
    pub async fn log(driver: usb::Driver<'static, USB>) {
        embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
    }

    #[embassy_executor::task]
    pub async fn usb(mut usb: UsbDevice<'static, usb::Driver<'static, USB>>) -> ! {
        usb.run().await
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let miso = p.PIN_12;
    let mosi = p.PIN_11;
    let sclk = p.PIN_10;
    let dcx = p.PIN_14;
    let display_cs = p.PIN_13;
    let rst = p.PIN_15;

    // create SPI
    let mut display_config = spi::Config::default();
    display_config.frequency = 6_000_000;
    display_config.phase = spi::Phase::CaptureOnSecondTransition;
    display_config.polarity = spi::Polarity::IdleHigh;

    let spi = Spi::new_blocking(p.SPI1, sclk, mosi, miso, display_config.clone());
    let spi_bus: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(spi));

    let display_spi = SpiDeviceWithConfig::new(
        &spi_bus,
        Output::new(display_cs, Level::High),
        display_config,
    );

    let dcx = Output::new(dcx, Level::Low);
    let rst = Output::new(rst, Level::Low);
    // dcx: 0 = command, 1 = data

    // Enable LCD backlight
    //let bl = p.PIN_13;
    //let _bl = Output::new(bl, Level::High);
    // Note: backlight is controlled via I2C to the keyboard

    // display interface abstraction from SPI and DC
    let mut buffer = [0_u8; 320 * 3];
    let di = SpiInterface::new(display_spi, dcx, &mut buffer);

    // Define the display from the display interface and initialize it
    let mut display = Builder::new(ILI9488Rgb565, di)
        .color_order(ColorOrder::Bgr)
        .display_size(320, 320)
        .reset_pin(rst)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().flip_horizontal())
        .init(&mut Delay)
        .unwrap();
    display.clear(Rgb565::BLACK).unwrap();

    let style = MonoTextStyle::new(&FONT_10X20, Rgb565::GREEN);
    Text::new("WezTerm", Point::new(0, 20), style)
        .draw(&mut display)
        .unwrap();

    loop {
        Timer::after_secs(1).await;
    }
}

/*
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    use core::str;
    use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
    use embassy_rp::pio::Pio;
    use static_cell::StaticCell;
    let p = embassy_rp::init(Default::default());

    let fw = include_bytes!("../embassy/cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../embassy/cyw43-firmware/43439A0_clm.bin");

    // USB background task:
    spawner
        .spawn(task::log(usb::Driver::new(p.USB, Irqs)))
        .unwrap();

    // Wireless background task:
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (_net_device, mut control, runner) = {
        let state = STATE.init(cyw43::State::new());
        let wireless_enable = Output::new(p.PIN_23, Level::Low);
        let wireless_spi = {
            let cs = Output::new(p.PIN_25, Level::High);
            let mut pio = Pio::new(p.PIO0, Irqs);
            PioSpi::new(
                &mut pio.common,
                pio.sm0,
                RM2_CLOCK_DIVIDER,
                pio.irq0,
                cs,
                p.PIN_24,
                p.PIN_29,
                p.DMA_CH0,
            )
        };
        cyw43::new(state, wireless_enable, wireless_spi, fw).await
    };
    spawner.spawn(task::cyw43(runner)).unwrap();

    control.init(clm).await;

    loop {
        // Scan WiFi networks:
        let mut scanner = control.scan(Default::default()).await;
        while let Some(bss) = scanner.next().await {
            if let Ok(ssid_str) = str::from_utf8(&bss.ssid) {
                log::info!("scanned {} == {:?}", ssid_str, bss.bssid);
            }
        }
    }
}
*/
