#![no_std]
#![no_main]

use crate::keyboard::{Key, KeyBoardState, KeyState, set_lcd_backlight};
use core::cell::RefCell;
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_rp::block::ImageDef;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIO0, SPI1, USB};
use embassy_rp::pio::{self};
use embassy_rp::spi::Spi;
use embassy_rp::{bind_interrupts, spi, usb};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Delay, Timer};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9488Rgb565;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation};
use mipidsi::{Builder};
use panic_halt as _;

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

mod keyboard;

const SCREEN_HEIGHT: u16 = 320;
const SCREEN_WIDTH: u16 = 320;

#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: ImageDef = ImageDef::secure_exe();

// Program metadata for `picotool info`.
// This isn't needed, but it's recomended to have these minimal entries.
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
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    I2C1_IRQ => embassy_rp::i2c::InterruptHandler<embassy_rp::peripherals::I2C1>;
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
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // USB background task:
    spawner
        .spawn(task::log(usb::Driver::new(p.USB, Irqs)))
        .unwrap();

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
    display_config.frequency = 40_000_000;
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
    set_lcd_backlight(&mut i2c_bus, 0x80).await;

    // display interface abstraction from SPI and DC
    let mut buffer = [0_u8; 320 * 3];
    let di = SpiInterface::new(display_spi, dcx, &mut buffer);

    // Define the display from the display interface and initialize it
    let mut display = Builder::new(ILI9488Rgb565, di)
        .color_order(ColorOrder::Bgr)
        .display_size(SCREEN_WIDTH, SCREEN_HEIGHT)
        .reset_pin(rst)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().flip_horizontal())
        .init(&mut Delay)
        .unwrap();
    display.clear(Rgb565::BLACK).unwrap();

    let mut screen_model = ScreenModel::default();
    screen_model.print("WezTerm\r\n");

    let mut keyboard = KeyBoardState::default();
    loop {
        screen_model.update_display(&mut display);

        if let Some(key) = keyboard.process(&mut i2c_bus).await {
            log::info!("key == {key:?}");
            if key.state == KeyState::Pressed {
                match key.key {
                    Key::Enter => {
                        screen_model.print("\r\n");
                    }
                    Key::Char(c) => {
                        screen_model.print_char(c);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[derive(Copy, Clone)]
struct Line {
    pub ascii: [u8; 80],
}

impl Default for Line {
    fn default() -> Line {
        Line { ascii: [0x20; 80] }
    }
}

struct ScreenModel {
    pub lines: [Line; 60],
    pub x: u8,
    pub y: u8,
    pub width: u8,
    pub height: u8,
    pub font: &'static MonoFont<'static>,
}

impl ScreenModel {
    pub fn print_char(&mut self, c: char) {
        match c {
            '\r' => {
                self.x = 0;
            }
            '\n' => {
                self.y += 1;
                // FIXME: scroll
            }
            _ => {
                let ascii = if c.is_ascii() {
                    c as u32 as u8
                } else {
                    0x20 // space
                };
                self.lines[self.y as usize].ascii[self.x as usize] = ascii;
                self.x += 1;
                if self.x >= self.width {
                    self.y += 1;
                    self.x = 0;
                    // FIXME: scroll
                }
            }
        }
    }

    pub fn print(&mut self, text: &str) {
        for c in text.chars() {
            self.print_char(c);
        }
    }

    pub fn update_display(&self, display: &mut PicoCalcDisplay) {
        let style = MonoTextStyle::new(self.font, Rgb565::GREEN);

        for y in 0..self.height as usize {
            let slice = &self.lines[y].ascii[0..self.width as usize];
            let Ok(text) = core::str::from_utf8(slice) else {
                continue;
            };

            Text::new(
                text,
                Point::new(
                    0,
                    (y * self.font.character_size.height as usize + self.font.baseline as usize)
                        as i32,
                ),
                style,
            )
            .draw(display)
            .unwrap();
        }
    }
}

impl Default for ScreenModel {
    fn default() -> ScreenModel {
        let font = &FONT_10X20;
        ScreenModel {
            x: 0,
            y: 0,
            width: ((SCREEN_WIDTH as u32) / (font.character_size.width + font.character_spacing))
                as u8,
            height: ((SCREEN_HEIGHT as u32) / font.character_size.height) as u8,
            font,

            lines: [Line::default(); 60],
        }
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
