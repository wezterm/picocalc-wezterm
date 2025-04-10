#![no_std]
#![no_main]

use crate::keyboard::{Key, KeyBoardState, KeyState, Modifiers, set_lcd_backlight};
use crate::psram::init_psram;
use core::cell::RefCell;
use core::fmt::Write as _;
use core::str;
use cyw43::Control;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_rp::block::ImageDef;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIO0, PIO1, SPI1, USB};
use embassy_rp::pio::Pio;
use embassy_rp::spi::Spi;
use embassy_rp::watchdog::Watchdog;
use embassy_rp::{bind_interrupts, spi, usb};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
use embassy_time::{Delay, Duration, Instant, Ticker, Timer};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use heapless::{FnvIndexSet, String};
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9488Rgb565;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation};
//use panic_halt as _;
use panic_persist as _;
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

mod keyboard;
mod psram;

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
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
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

#[embassy_executor::task]
async fn watchdog_task(mut watchdog: Watchdog) {
    if let Some(reason) = watchdog.reset_reason() {
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

    // USB background task:
    spawner.must_spawn(task::log(usb::Driver::new(p.USB, Irqs)));

    SCREEN.get().lock().await.print("WezTerm\r\n");
    if let Some(msg) = panic_persist::get_panic_message_utf8() {
        log::error!("prior panic: {msg}");
        write!(SCREEN.get().lock().await, "Panic: {msg}\r\n").ok();
    }
    spawner.must_spawn(watchdog_task(Watchdog::new(p.WATCHDOG)));

    /*
    let psram_size = detect_psram(&embassy_rp::pac::QMI);
    write!(SCREEN.get().lock().await, "psram: {psram_size:x}\r\n").ok();
    */

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

    let keyboard = KeyBoardState::default();
    spawner.must_spawn(keyboard_reader(keyboard, i2c_bus));

    Timer::after(Duration::from_millis(100)).await;

    let wifi_control = setup_wifi(
        &spawner, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_29, p.PIO0, p.DMA_CH0,
    )
    .await;
    spawner.must_spawn(wifi_scanner(wifi_control));

    Timer::after(Duration::from_secs(15)).await;

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
async fn keyboard_reader(
    mut keyboard: KeyBoardState,
    mut i2c_bus: embassy_rp::i2c::I2c<
        'static,
        embassy_rp::peripherals::I2C1,
        embassy_rp::i2c::Async,
    >,
) {
    let mut kbd_ticker = Ticker::every(Duration::from_millis(50));
    loop {
        kbd_ticker.next().await;
        if let Some(key) = keyboard.process(&mut i2c_bus).await {
            log::info!("key == {key:?}");
            if key.state == KeyState::Pressed {
                // See rp2350 datasheet section 5.4.8.24. reboot
                const NO_RETURN_ON_SUCCESS: u32 = 0x100;
                const REBOOT_TYPE_NORMAL: u32 = 0;
                const REBOOT_TYPE_BOOTSEL: u32 = 2;
                match key.key {
                    Key::F5 if key.modifiers == Modifiers::CTRL => {
                        //embassy_rp::reset_to_usb_boot(0, 0); // for rp2040
                        embassy_rp::rom_data::reboot(
                            REBOOT_TYPE_BOOTSEL | NO_RETURN_ON_SUCCESS,
                            100,
                            0,
                            0,
                        );
                        loop {}
                    }
                    Key::F1 if key.modifiers == Modifiers::CTRL => {
                        //embassy_rp::reset_to_usb_boot(0, 0); // for rp2040
                        // See rp2350 datasheet section 5.4.8.24. reboot
                        embassy_rp::rom_data::reboot(
                            REBOOT_TYPE_NORMAL | NO_RETURN_ON_SUCCESS,
                            100,
                            0,
                            0,
                        );
                        loop {}
                    }
                    Key::Enter => {
                        SCREEN.get().lock().await.print("\r\n");
                    }
                    Key::Char(c) => {
                        SCREEN.get().lock().await.print_char(c);
                    }
                    _ => {}
                }
            }
        }
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

static SCREEN: LazyLock<AsyncMutex<CriticalSectionRawMutex, ScreenModel>> =
    LazyLock::new(|| AsyncMutex::new(ScreenModel::default()));

type WifiSet = FnvIndexSet<String<32>, 16>;
static NETWORKS: LazyLock<AsyncMutex<CriticalSectionRawMutex, WifiSet>> =
    LazyLock::new(|| AsyncMutex::new(WifiSet::new()));

#[embassy_executor::task]
async fn wifi_scanner(mut control: Control<'static>) {
    let mut scanner = control.scan(Default::default()).await;

    while let Some(bss) = scanner.next().await {
        if bss.ssid_len == 0 {
            continue;
        }
        if let Ok(ssid_str) = str::from_utf8(&bss.ssid[0..bss.ssid_len as usize]) {
            if let Ok(ssid) = String::try_from(ssid_str) {
                if let Ok(true) = NETWORKS.get().lock().await.insert(ssid) {
                    log::info!("wifi: {ssid_str} = {:x?}", bss.bssid);
                    write!(SCREEN.get().lock().await, "wifi: {ssid_str}\r\n",).ok();
                }
            }
        }
    }
}

async fn setup_wifi(
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
    let (_net_device, mut control, runner) = {
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
    control
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

impl core::fmt::Write for ScreenModel {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.print(s);
        Ok(())
    }
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
        //let font = & embedded_graphics::mono_font::ascii::FONT_10X20;
        let font = &embedded_graphics::mono_font::ascii::FONT_5X8;
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
