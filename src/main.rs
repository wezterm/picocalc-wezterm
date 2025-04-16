#![feature(impl_trait_in_assoc_type)]
#![no_std]
#![no_main]

use crate::config::{CONFIG, Flash};
use crate::heap::HEAP;
use crate::keyboard::set_lcd_backlight;
use crate::psram::init_psram;
use crate::rng::WezTermRng;
use crate::screen::SCREEN;
use crate::storage::init_storage;
use core::cell::RefCell;
use core::fmt::Write as _;
use cyw43::Control;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_net::dns::{DnsQueryType, DnsSocket};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Stack};
use embassy_rp::block::ImageDef;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIO0, PIO1, SPI1, TRNG, UART0, USB};
use embassy_rp::pio::Pio;
use embassy_rp::spi::Spi;
use embassy_rp::uart::BufferedInterruptHandler;
use embassy_rp::watchdog::Watchdog;
use embassy_rp::{bind_interrupts, spi, usb};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Delay, Duration, Ticker, Timer};
use embedded_io_async::Read;
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9488Rgb565;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation};
use panic_persist as _;
use rand_core::RngCore;
use static_cell::StaticCell;
use sunset::{CliEvent, SessionCommand};
use sunset_embassy::{ChanInOut, ProgressHolder, SSHClient};

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
mod heap;
mod keyboard;
mod logging;
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
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
    I2C1_IRQ => embassy_rp::i2c::InterruptHandler<embassy_rp::peripherals::I2C1>;
    UART0_IRQ => BufferedInterruptHandler<UART0>;
    TRNG_IRQ => embassy_rp::trng::InterruptHandler<TRNG>;
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
        usb::Driver::new(p.USB, Irqs),
    )
    .await;

    print!(
        "\u{1b}[35mWezTerm {}\u{1b}[0m\r\n",
        env!("CARGO_PKG_VERSION")
    );

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
    }
    spawner.must_spawn(watchdog_task(Watchdog::new(p.WATCHDOG)));
    crate::rng::init_rng(p.TRNG);

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
        .reset_pin(rst)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().flip_horizontal())
        .init(&mut Delay)
        .unwrap();
    spawner.must_spawn(crate::screen::screen_painter(display));

    spawner.must_spawn(crate::keyboard::keyboard_reader(i2c_bus));

    Timer::after(Duration::from_millis(100)).await;

    let mut flash = Flash::new(p.FLASH, p.DMA_CH3);
    {
        let mut config = CONFIG.get().lock().await;
        match config.load_in_place(&mut flash) {
            Ok(()) => {
                log::info!("Loaded configuration: {config:#?}");
            }
            Err(err) => {
                log::error!("Failed to load config: {err:?}");
                config.ssid.push_str("SECRET").ok();
                config.wifi_pw.push_str("SECRET").ok();
                config.ssh_pw.push_str("SECRET").ok();
                if false {
                    // To bootstrap the config
                    match config.save(&mut flash) {
                        Ok(()) => {
                            log::info!("Wrote configuration!");
                        }
                        Err(err) => {
                            log::error!("Failed to write config: {err:?}");

                            print!("BORK: {err:?}");

                            let mut ticker = Ticker::every(Duration::from_millis(5000));
                            loop {
                                ticker.next().await;
                            }
                        }
                    }
                }
            }
        }
    };

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

    let wifi_control = setup_wifi(
        &spawner, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_29, p.PIO0, p.DMA_CH0,
    )
    .await;
    // spawner.must_spawn(wifi_scanner(wifi_control));

    let mut ticker = Ticker::every(Duration::from_millis(100));
    loop {
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

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

macro_rules! static_bytes {
    ($n:expr) => {
        mk_static!([u8; $n], [0u8; $n])
    };
}

#[embassy_executor::task]
async fn ssh_channel_task(mut channel: ChanInOut<'static, 'static>) {
    /*
    let winch = {
        let screen = SCREEN.get().lock().await;
        let rows = screen.height;
        let cols = screen.width;
        sunset::packets::WinChange {
            rows: rows.into(),
            cols: cols.into(),
            width: SCREEN_WIDTH as u32,
            height: SCREEN_HEIGHT as u32,
        }
    };
    log::info!("sending window size {winch:?}");
    if let Err(err) = channel.term_window_change(winch).await {
        log::error!("winch failed: {err:?}");
    }

    log::info!("sending uname command");
    if let Err(err) = channel.write_all("uname -a\r\n".as_bytes()).await {
        log::error!("error sending command: {err:?}");
    }
    */

    log::info!("ssh_channel_task waiting for output");
    loop {
        let mut buf = [0u8; 1024];
        match channel.read(&mut buf).await {
            Ok(n) => {
                if n == 0 {
                    log::warn!("EOF on ssh channel");
                    break;
                }
                match core::str::from_utf8(&buf[0..n]) {
                    Ok(s) => {
                        log::info!("{s}");
                        write!(SCREEN.get().lock().await, "{s}").ok();
                    }
                    Err(err) => {
                        log::error!("failed utf8: {err:?}");
                    }
                }
            }
            Err(err) => {
                log::error!("failed read: {err:?}");
                break;
            }
        }
    }

    loop {
        Timer::after(Duration::from_millis(1000)).await;
    }
}

#[embassy_executor::task]
async fn ssh_session_task(stack: Stack<'static>) {
    let dns_client = DnsSocket::new(stack);

    let host = "foo.lan";

    write!(SCREEN.get().lock().await, "$ ssh {host}\r\n").ok();

    match dns_client.query(host, DnsQueryType::A).await {
        Ok(addrs) => {
            log::info!("{host} -> {addrs:?}");
            let mut tcp_socket = TcpSocket::new(stack, static_bytes!(8192), static_bytes!(8192));

            match tcp_socket
                .connect(IpEndpoint {
                    addr: addrs[0],
                    port: 22,
                })
                .await
            {
                Ok(()) => {
                    use embassy_futures::join::join;
                    log::info!("Connected to port 22!");
                    write!(
                        SCREEN.get().lock().await,
                        "Connected to {host} {}:22\r\n",
                        addrs[0]
                    )
                    .ok();
                    let (mut read, mut write) = tcp_socket.split();
                    let ssh_client = mk_static!(
                        SSHClient,
                        SSHClient::new(static_bytes!(8192), static_bytes!(8192))
                            .expect("SSHClient::new")
                    );

                    let session_authd_chan =
                        embassy_sync::channel::Channel::<NoopRawMutex, (), 1>::new();
                    let wait_for_auth = session_authd_chan.receiver();

                    let spawn_session_future = async {
                        let _ = wait_for_auth.receive().await;

                        log::info!("try open pty");
                        let channel = ssh_client.open_session_pty().await.expect("openpty failed");
                        log::info!("pty opened, spawn client task");
                        Spawner::for_current_executor()
                            .await
                            .must_spawn(ssh_channel_task(channel));
                    };

                    let runner = ssh_client.run(&mut read, &mut write);
                    let mut progress = ProgressHolder::new();
                    let ssh_pw = CONFIG.get().lock().await.ssh_pw.clone();
                    let ssh_ticker = async {
                        loop {
                            match ssh_client.progress(&mut progress).await {
                                Ok(event) => match event {
                                    CliEvent::Hostkey(k) => {
                                        log::info!("host key {:?}", k.hostkey());
                                        k.accept().expect("accept hostkey");
                                    }
                                    CliEvent::Banner(b) => {
                                        if let Ok(b) = b.banner() {
                                            log::info!("banner: {b}");
                                        }
                                    }
                                    CliEvent::Username(req) => {
                                        req.username("wez").expect("set user");
                                    }
                                    CliEvent::Password(req) => {
                                        req.password(&ssh_pw).expect("set pw");
                                    }
                                    CliEvent::Pubkey(req) => {
                                        req.skip().expect("skip pubkey");
                                    }
                                    CliEvent::AgentSign(req) => {
                                        req.skip().expect("skip agentsign");
                                    }
                                    CliEvent::Authenticated => {
                                        log::info!("Authenticated!");
                                        session_authd_chan.sender().send(()).await;
                                    }
                                    CliEvent::SessionOpened(mut s) => {
                                        log::info!("session opened channel {}", s.channel());

                                        use heapless::{String, Vec};

                                        let mut term = String::<32>::new();
                                        let _ = term.push_str("xterm").unwrap();

                                        let pty = {
                                            let screen = SCREEN.get().lock().await;
                                            let rows = screen.height;
                                            let cols = screen.width;

                                            sunset::Pty {
                                                term,
                                                rows: rows.into(),
                                                cols: cols.into(),
                                                width: SCREEN_WIDTH as u32,
                                                height: SCREEN_HEIGHT as u32,
                                                modes: Vec::new(),
                                            }
                                        };

                                        log::info!("requesting pty {pty:?}");
                                        if let Err(err) = s.pty(pty) {
                                            log::error!("requesting pty failed {err:?}");
                                        }
                                        log::info!("setting command");
                                        let command = "uname -a";
                                        write!(SCREEN.get().lock().await, "execute {command}\r\n")
                                            .ok();
                                        if let Err(err) = s.cmd(&SessionCommand::Exec(command)) {
                                            log::error!("command failed: {err:?}");
                                        }
                                        log::info!("SessionOpened completed");
                                    }
                                    CliEvent::SessionExit(x) => {
                                        log::info!("session exit with {x:?}");
                                    }
                                    CliEvent::Defunct => {
                                        log::error!("ssh session terminated");
                                        break;
                                    }
                                },
                                Err(err) => {
                                    log::error!("ssh progress error: {err:?}");
                                    break;
                                }
                            }
                        }

                        Ok::<(), ()>(())
                    };

                    let res = join(runner, join(ssh_ticker, spawn_session_future)).await;
                    log::info!("ssh result is {res:?}");
                }
                Err(err) => {
                    log::error!("failed to connect to port 22: {err:?}");
                }
            }
        }
        Err(err) => {
            log::error!("failed foo.lan: {err:?}");
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

    let config = embassy_net::Config::dhcpv4(Default::default());
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        WezTermRng.next_u64(),
    );
    spawner.must_spawn(net_task(runner));

    control
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;

    let (ssid, wifi_pw) = {
        let config = CONFIG.get().lock().await;
        (config.ssid.clone(), config.wifi_pw.clone())
    };
    print!("Connecting to \u{1b}[1m{ssid}\u{1b}[0m...\r\n");
    loop {
        match control
            .join(&ssid, cyw43::JoinOptions::new(wifi_pw.as_bytes()))
            .await
        {
            Ok(_) => break,
            Err(err) => {
                log::error!("join failed with status={}", err.status);
                print!("Failed with status {}\r\n", err.status);
            }
        }
    }

    log::info!("waiting for TCP to be up...");
    stack.wait_config_up().await;
    log::info!("Stack is up!");
    if let Some(v4) = stack.config_v4() {
        log::info!("{v4:?}");
        print!("IP Address {}\r\n", v4.address);
    }

    spawner.must_spawn(crate::time::time_sync(stack));
    // spawner.must_spawn(ssh_session_task(stack));

    control
}

pub fn byte_size<V: humansize::ToF64 + humansize::Unsigned>(
    n: V,
) -> humansize::SizeFormatter<V, humansize::FormatSizeOptions> {
    humansize::SizeFormatter::new(
        n,
        humansize::FormatSizeOptions::from(humansize::BINARY).space_after_value(true),
    )
}
