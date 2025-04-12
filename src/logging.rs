use crate::{Irqs, SCREEN};
use core::fmt::Write as _;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::peripherals::{PIN_0, PIN_1, UART0, USB};
use embassy_rp::uart::{BufferedUart, BufferedUartRx, BufferedUartTx, Config as UartConfig};
use embassy_rp::usb;
use embassy_sync::pipe::Pipe;
use embassy_usb_logger::UsbLogger;
use embedded_io_async::{Read, Write as _};
use log::{LevelFilter, Metadata, Record};
use static_cell::StaticCell;

// This module logs to both UART0 and to a USB CDC endpoint.
// The former is routed via the host picocalc board and a CH340C
// USB to serial chip.
// The latter is an explicit and direct connection to us.

type CS = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

pub async fn setup_logging(
    spawner: &Spawner,
    tx_pin: PIN_0,
    rx_pin: PIN_1,
    uart: UART0,
    usb: usb::Driver<'static, USB>,
) {
    static TX_BUF: StaticCell<[u8; 16]> = StaticCell::new();
    let tx_buf = &mut TX_BUF.init([0; 16])[..];
    static RX_BUF: StaticCell<[u8; 16]> = StaticCell::new();
    let rx_buf = &mut RX_BUF.init([0; 16])[..];
    let uart = BufferedUart::new(
        uart,
        Irqs,
        tx_pin,
        rx_pin,
        tx_buf,
        rx_buf,
        UartConfig::default(),
    );
    let (mut tx, rx) = uart.split();

    let _ = tx
        .write_all(b"\r\n\r\n *** WezTerm picocalc starting up ***\r\n\r\n")
        .await;

    spawner.must_spawn(log(tx, usb));
    spawner.must_spawn(uart_reader(rx));
}

type UsbLog = UsbLogger<1024, embassy_usb_logger::DummyHandler>;

struct Logger {
    usb_logger: UsbLog,
    pipe: Pipe<CS, 1024>,
}

impl Logger {
    /// Take data from the pipe, which is populated by the `log` crate,
    /// and feed it into the uart.
    async fn run_uart(&self, mut uart: BufferedUartTx<'static, UART0>) {
        loop {
            let mut buf = [0u8; 1024];
            let len = self.pipe.read(&mut buf).await;
            let _ = uart.write_all(&buf[0..len]).await;
        }
    }
}

impl log::Log for Logger {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    /// Logs to both usb and the serial connection
    fn log(&self, record: &Record<'_>) {
        self.usb_logger.log(record);
        let _ = write!(Writer(&self.pipe), "{}\n", record.args());
    }
    fn flush(&self) {
        self.usb_logger.flush();
    }
}

pub struct Writer<'d, const N: usize>(&'d Pipe<CS, N>);

impl<'d, const N: usize> Writer<'d, N> {
    fn write_slice(&mut self, b: &[u8]) {
        // The Pipe is implemented in such way that we cannot
        // write across the wraparound discontinuity.
        if let Ok(n) = self.0.try_write(b) {
            if n < b.len() {
                // We wrote some data but not all, attempt again
                // as the reason might be a wraparound in the
                // ring buffer, which resolves on second attempt.
                let _ = self.0.try_write(&b[n..]);
            }
        }
    }
}

// Lifted from
// <https://github.com/embassy-rs/embassy/blob/6919732666bdcb4b2a4d26be348c87e4ca70280b/embassy-usb-logger/src/lib.rs#L191-L209>
// Used here under its MIT license.
impl<'d, const N: usize> core::fmt::Write for Writer<'d, N> {
    fn write_str(&mut self, s: &str) -> Result<(), core::fmt::Error> {
        // We need to translate \n to \r\n for serial to be happiest
        let b = s.as_bytes();

        for chunk in b.split_inclusive(|&c| c == b'\n') {
            let (stripped, emit_crlf) = match chunk.strip_suffix(b"\n") {
                Some(s) => (s, true),
                None => (chunk, false),
            };

            self.write_slice(stripped);

            if emit_crlf {
                self.write_slice(b"\r\n");
            }
        }
        Ok(())
    }
}

#[embassy_executor::task]
pub async fn log(uart: BufferedUartTx<'static, UART0>, driver: usb::Driver<'static, USB>) {
    static LOGGER: Logger = Logger {
        usb_logger: UsbLog::new(),
        pipe: Pipe::new(),
    };

    unsafe {
        let _ = log::set_logger_racy(&LOGGER).map(|()| log::set_max_level_racy(LevelFilter::Info));
    }

    let _ = join(
        LOGGER
            .usb_logger
            .run(&mut embassy_usb_logger::LoggerState::new(), driver),
        LOGGER.run_uart(uart),
    )
    .await;
}

#[embassy_executor::task]
async fn uart_reader(mut rx: BufferedUartRx<'static, UART0>) {
    loop {
        let mut buf = [0; 31];
        if let Ok(n) = rx.read(&mut buf).await {
            if let Ok(s) = core::str::from_utf8(&buf[0..n]) {
                write!(SCREEN.get().lock().await, "UART RX: {s}\r\n").ok();
            }
        }
    }
}
