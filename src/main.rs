#![no_std]
#![no_main]

use core::str;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_rp::block::ImageDef;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::pio::{self, Pio};
use embassy_rp::{bind_interrupts, usb};
use panic_halt as _;
use static_cell::StaticCell;

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
async fn main(spawner: Spawner) {
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
    /*
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;
    */

    loop {
        // Scan WiFi networks:
        let mut scanner = control.scan(Default::default()).await;
        while let Some(bss) = scanner.next().await {
            if let Ok(ssid_str) = str::from_utf8(&bss.ssid) {
                log::info!("scanned {} == {:?}", ssid_str, bss.bssid);
            }
        }
    }

    /*
    let mut counter = 0;
    loop {
        counter += 1;
        log::info!("Tick {}", counter);
        Timer::after_secs(1).await;
    }

    let delay = Duration::from_millis(100);
    loop {
        info!("led on!");
        control.gpio_set(0, true).await;
        Timer::after(delay).await;

        info!("led off!");
        control.gpio_set(0, false).await;
        Timer::after(delay).await;
    }
    */
}
