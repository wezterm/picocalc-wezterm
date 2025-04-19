#![feature(impl_trait_in_assoc_type)]
#![no_std]
#![no_main]

use crate::config::{CONFIG, Flash};
use crate::heap::HEAP;
use crate::psram::init_psram;
use crate::screen::SCREEN;
use crate::storage::init_storage;
use core::cell::RefCell;
use core::fmt::Write as _;
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_rp::block::ImageDef;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIO0, PIO1, SPI1, TRNG, UART0, UART1, USB};
use embassy_rp::spi::Spi;
use embassy_rp::uart::BufferedInterruptHandler;
use embassy_rp::watchdog::Watchdog;
use embassy_rp::{bind_interrupts, spi, usb};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Delay, Duration, Ticker, Timer};
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9488Rgb565;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation};
use panic_persist as _;
use static_cell::StaticCell;

macro_rules! print {
    ($($args:tt)+) => {
        {
            use crate::screen::SCREEN;
            use core::fmt::Write;
            use crate::process::current_proc;
            let proc = current_proc();
            {
                let mut screen = SCREEN.get().lock().await;
                // Erase whatever prompt may have been printed
                proc.un_prompt(&mut screen);
                // write our text
                write!(screen, $($args)+).ok();
            }
            // Get the shell to render its prompt again
            proc.render().await;
        }
    }
}

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
mod fixed_str;
mod heap;
mod keyboard;
mod logging;
mod net;
mod process;
mod psram;
mod rng;
mod screen;
mod storage;
mod time;

const SCREEN_HEIGHT: u16 = 320;
const SCREEN_WIDTH: u16 = 320;
const MAX_SPI_FREQ: u32 = 62_500_000;

#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: ImageDef = ImageDef::secure_exe();

#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"WezTerm"),
    embassy_rp::binary_info::rp_program_description!(c"Hardware WezTerm"),
    embassy_rp::binary_info::env!(
        embassy_rp::binary_info::consts::TAG_RASPBERRY_PI,
        embassy_rp::binary_info::consts::ID_RP_PROGRAM_VERSION_STRING,
        "WEZTERM_CI_TAG"
    ),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
    I2C1_IRQ => embassy_rp::i2c::InterruptHandler<embassy_rp::peripherals::I2C1>;
    UART0_IRQ => BufferedInterruptHandler<UART0>;
    UART1_IRQ => BufferedInterruptHandler<UART1>;
    TRNG_IRQ => embassy_rp::trng::InterruptHandler<TRNG>;
});

#[embassy_executor::task]
async fn watchdog_task(mut watchdog: Watchdog) {
    if let Some(reason) = watchdog.reset_reason() {
        log::error!("Watchdog reset reason: {reason:?}");
    }

    watchdog.start(Duration::from_secs(3));

    let mut ticker = Ticker::every(Duration::from_secs(2));
    loop {
        watchdog.feed();
        ticker.next().await;
    }
}

/// Returns the amount of RAM available to use as stack space.
/// This gives a sense of the amount of free memory in the system.
/// It is not a directly useful metric.
/// The calculation here relies on the flip-link memory layout
/// and assumes that the .data and .bss have been re-arranged
/// to sit on top of the stack space.
fn get_max_usable_stack() -> usize {
    unsafe extern "C" {
        /// flip-link assigns this to be exactly the stack
        /// size from the ORIGIN(RAM). It is the top of the
        /// stack space, and stack grows down towards zero.
        static mut _stack_start: u8;
    }

    let start_ptr = &raw mut _stack_start as *mut u8 as usize;
    start_ptr - 0x20000000 /* where RAM starts in memory.x */
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    crate::heap::init_heap();

    crate::logging::setup_logging(
        &spawner,
        p.PIN_0,
        p.PIN_1,
        p.UART0,
        p.PIN_8,
        p.PIN_9,
        p.UART1,
        usb::Driver::new(p.USB, Irqs),
    )
    .await;

    print!("\u{1b}[35mWezTerm {}\u{1b}[0m\r\n", env!("WEZTERM_CI_TAG"));

    if let Some(msg) = panic_persist::get_panic_message_utf8() {
        // Give serial a chance to be ready to capture this info
        Timer::after(Duration::from_millis(100)).await;
        log::error!("prior panic: {msg}");
        let mut screen = SCREEN.get().lock().await;
        write!(screen, "\u{1f}[1mPanic: ").ok();
        for chunk in msg.lines() {
            write!(screen, "{chunk}\r\n").ok();
        }
        write!(screen, "\u{1f}[0m").ok();
        Timer::after(Duration::from_secs(5)).await;
    }
    spawner.must_spawn(watchdog_task(Watchdog::new(p.WATCHDOG)));
    crate::rng::init_rng(p.TRNG);

    let mut i2c_config = embassy_rp::i2c::Config::default();
    i2c_config.frequency = 400_000;
    let scl = p.PIN_7;
    let sda = p.PIN_6;
    let i2c_bus = embassy_rp::i2c::I2c::new_async(p.I2C1, scl, sda, Irqs, i2c_config);

    let miso = p.PIN_12;
    let mosi = p.PIN_11;
    let sclk = p.PIN_10;
    let dcx = p.PIN_14;
    let display_cs = p.PIN_13;
    let rst = p.PIN_15;

    // create SPI
    let mut display_config = spi::Config::default();
    display_config.frequency = MAX_SPI_FREQ;
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
        .reset_pin(rst)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().flip_horizontal())
        .init(&mut Delay)
        .unwrap();
    spawner.must_spawn(crate::screen::screen_painter(display));
    spawner.must_spawn(crate::keyboard::keyboard_reader(i2c_bus));

    let flash = Flash::new(p.FLASH, p.DMA_CH3);
    CONFIG.get().lock().await.assign_flash(flash);

    let psram = init_psram(
        p.PIO1, p.PIN_21, p.PIN_2, p.PIN_3, p.PIN_20, p.DMA_CH1, p.DMA_CH2,
    )
    .await;

    {
        print!(
            "RAM {} avail of 512KiB. PSRAM {}\r\n",
            byte_size(get_max_usable_stack()),
            byte_size(psram.size),
        );
        if psram.size == 0 {
            // This can happen if you power on the pico without first
            // powering up the picocalc carrier board
            print!("\u{1b}[1mExternal PSRAM was NOT found!\u{1b}[0m\r\n");
        }
        print!(
            "Heap {} used, {} free\r\n",
            byte_size(HEAP.used()),
            byte_size(HEAP.free()),
        );
    }

    init_storage(
        &spawner, p.PIN_16, p.PIN_17, p.PIN_18, p.PIN_19, p.PIN_22, p.SPI0,
    )
    .await;

    crate::net::setup_wifi(
        &spawner, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_29, p.PIO0, p.DMA_CH0,
    )
    .await;

    let mut ticker = Ticker::every(Duration::from_secs(3600));
    loop {
        ticker.next().await;
    }
}

#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[macro_export]
macro_rules! static_bytes {
    ($n:expr) => {
        mk_static!([u8; $n], [0u8; $n])
    };
}

pub fn byte_size<V: humansize::ToF64 + humansize::Unsigned>(
    n: V,
) -> humansize::SizeFormatter<V, humansize::FormatSizeOptions> {
    humansize::SizeFormatter::new(
        n,
        humansize::FormatSizeOptions::from(humansize::BINARY).space_after_value(true),
    )
}
