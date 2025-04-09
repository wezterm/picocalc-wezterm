use crate::Irqs;
use embassy_rp::PeripheralRef;
use embassy_rp::gpio::Drive;
use embassy_rp::peripherals::{DMA_CH1, DMA_CH2, PIN_2, PIN_3, PIN_20, PIN_21, PIO1};
use embassy_rp::pio::program::pio_asm;
use embassy_rp::pio::{Config, Direction, Pio, ShiftDirection};
use embassy_time::{Duration, Timer};
use fixed::FixedU32;

// The physical connections in the picocalc schematic are:
// LABEL     PICO      ESP-PSRAM64H
// RAM_CS  - PIN_20    CE                    (pulled up to 3v3 via 10kOhm)
// RAM_SCK - PIN_21    SCLK
// RAM_TX  - PIN_2     SI/SIO0
// RAM_RX  - PIN_3     SO/SIO1
// RAM_IO2 - PIN_4     SIO2     (QPI Mode)
// RAM_IO3 - PIN_5     SIO3     (QPI Mode)

#[allow(unused)]
const PSRAM_CMD_QUAD_END: u8 = 0xf5;
#[allow(unused)]
const PSRAM_CMD_QUAD_ENABLE: u8 = 0x35;
#[allow(unused)]
const PSRAM_CMD_READ_ID: u8 = 0x9F;
const PSRAM_CMD_RSTEN: u8 = 0x66;
const PSRAM_CMD_RST: u8 = 0x99;
const PSRAM_CMD_WRITE: u8 = 0x02;
const PSRAM_CMD_FAST_READ: u8 = 0x0B;
#[allow(unused)]
const PSRAM_CMD_QUAD_READ: u8 = 0xEB;
#[allow(unused)]
const PSRAM_CMD_QUAD_WRITE: u8 = 0x38;
#[allow(unused)]
const PSRAM_CMD_NOOP: u8 = 0xFF;
#[allow(unused)]
const PSRAM_KNOWN_GOOD_DIE_PASS: u8 = 0x5d;

pub struct PsRam {
    sm: embassy_rp::pio::StateMachine<'static, PIO1, 0>,
    tx_ch: PeripheralRef<'static, DMA_CH1>,
    rx_ch: PeripheralRef<'static, DMA_CH2>,
}

impl PsRam {
    pub async fn send_command(&mut self, cmd: &[u8], out: &mut [u8]) {
        self.sm
            .tx()
            .dma_push(self.tx_ch.reborrow(), cmd, false)
            .await;
        if !out.is_empty() {
            self.sm
                .rx()
                .dma_pull(self.rx_ch.reborrow(), out, false)
                .await;
        }
    }

    pub async fn write(&mut self, mut addr: u32, mut data: &[u8]) {
        // I haven't seen this work reliably over 24 bytes
        const MAX_CHUNK: usize = 24;
        while data.len() > 0 {
            let to_write = data.len().min(MAX_CHUNK);
            log::info!("writing {to_write} @ {addr}");

            #[rustfmt::skip]
            let mut to_send = [
                32 + (to_write as u8 * 8), // write address + data
                0,                         // read 0 bits
                PSRAM_CMD_WRITE,
                ((addr >> 16) & 0xff) as u8,
                ((addr >> 8) & 0xff) as u8,
                (addr & 0xff) as u8,
                // This sequence must be MAX_CHUNK in length
                0, 0, 0, 0,
                0, 0, 0, 0,
                0, 0, 0, 0,
                0, 0, 0, 0,
                0, 0, 0, 0,
                0, 0, 0, 0,
            ];

            for (src, dst) in data.iter().zip(to_send.iter_mut().skip(6)) {
                *dst = *src;
            }

            self.send_command(&to_send[0..6 + to_write], &mut []).await;
            addr += to_write as u32;
            data = &data[to_write..];
        }
    }

    pub async fn read(&mut self, mut addr: u32, mut out: &mut [u8]) {
        // Cannot get reliable reads above 4 bytes at a time.
        // out[4] will always have a bit error
        const MAX_CHUNK: usize = 4;
        while out.len() > 0 {
            let to_read = out.len().min(MAX_CHUNK);
            log::info!("reading {to_read} @ {addr}");
            self.send_command(
                &[
                    40,                // write 40 bits
                    to_read as u8 * 8, // read n bytes
                    PSRAM_CMD_FAST_READ,
                    ((addr >> 16) & 0xff) as u8,
                    ((addr >> 8) & 0xff) as u8,
                    (addr & 0xff) as u8,
                    0, // 8 cycle delay by sending 8 bits of don't care data
                ],
                &mut out[0..to_read],
            )
            .await;
            addr += to_read as u32;
            out = &mut out[to_read..];
        }
    }

    #[allow(unused)]
    pub async fn write8(&mut self, addr: u32, data: u8) {
        log::info!("write8 addr {addr} <- {data:x}");
        self.send_command(
            &[
                40, // write 40 bits
                0,  // read 0 bits
                PSRAM_CMD_WRITE,
                ((addr >> 16) & 0xff) as u8,
                ((addr >> 8) & 0xff) as u8,
                (addr & 0xff) as u8,
                data,
            ],
            &mut [],
        )
        .await;
    }

    #[allow(unused)]
    pub async fn read8(&mut self, addr: u32) -> u8 {
        let mut buf = [0u8];
        self.send_command(
            &[
                40, // write 40 bits
                8,  // read 8 bits
                PSRAM_CMD_FAST_READ,
                ((addr >> 16) & 0xff) as u8,
                ((addr >> 8) & 0xff) as u8,
                (addr & 0xff) as u8,
                0, // 8 cycle delay
            ],
            &mut buf,
        )
        .await;
        buf[0]
    }
}

pub async fn init_psram(
    pio_1: PIO1,
    sclk: PIN_21,
    mosi: PIN_2,
    miso: PIN_3,
    cs: PIN_20,
    dma_ch1: DMA_CH1,
    dma_ch2: DMA_CH2,
) -> PsRam {
    let mut pio = Pio::new(pio_1, Irqs);

    // This pio program was taken from
    // <https://github.com/polpo/rp2040-psram/blob/7786c93ec8d02dbb4f94a2e99645b25fb4abc2db/psram_spi.pio>
    // which is Copyright © 2023 Ian Scott, reproduced here under the MIT license
    let prog = pio_asm!(
        r#"
.side_set 2                        ; sideset bit 1 is SCK, bit 0 is CS
begin:
    out x, 8            side 0b01  ; x = number of bits to output. CS deasserted
    out y, 8            side 0b01  ; y = number of bits to input
    jmp x--, writeloop  side 0b01  ; Pre-decement x by 1 so loop has correct number of iterations
writeloop:
    out pins, 1         side 0b00  ; Write value on pin, lower clock. CS asserted
    jmp x--, writeloop  side 0b10  ; Raise clock: this is when PSRAM reads the value. Loop if we have more to write
    jmp !y, begin       side 0b00  ; If this is a write-only operation, jump back to beginning
    nop                 side 0b10  ; Fudge factor of extra clock cycle; the PSRAM needs 1 extra for output to start appearing
    jmp readloop_mid    side 0b00  ; Jump to middle of readloop to decrement y and get right clock phase
readloop:
    in pins, 1          side 0b00  ; Read value on pin, lower clock. Datasheet says to read on falling edge > 83MHz
readloop_mid:
    jmp y--, readloop   side 0b10  ; Raise clock. Loop if we have more to read
    "#
    );
    let prog = pio.common.load_program(&prog.program);

    let mut cfg = Config::default();

    let mut cs = pio.common.make_pio_pin(cs);
    let mut sclk = pio.common.make_pio_pin(sclk);
    let mut mosi = pio.common.make_pio_pin(mosi);
    let mut miso = pio.common.make_pio_pin(miso);

    cs.set_drive_strength(Drive::_4mA);
    sclk.set_drive_strength(Drive::_4mA);
    mosi.set_drive_strength(Drive::_4mA);
    miso.set_drive_strength(Drive::_4mA);

    cfg.use_program(&prog, &[&cs, &sclk]);
    cfg.set_out_pins(&[&mosi]);
    cfg.set_in_pins(&[&miso]);

    cfg.shift_out.direction = ShiftDirection::Left;
    cfg.shift_out.auto_fill = true;
    cfg.shift_out.threshold = 8;

    cfg.shift_in = cfg.shift_out;
    cfg.clock_divider = FixedU32::from_num(1);

    let mut sm = pio.sm0;
    sm.set_pin_dirs(Direction::Out, &[&cs, &sclk]);
    sm.set_pin_dirs(Direction::Out, &[&mosi]);
    sm.set_pin_dirs(Direction::In, &[&miso]);
    miso.set_input_sync_bypass(true);

    sm.set_config(&cfg);
    sm.set_enable(true);

    let dma_ch1 = PeripheralRef::new(dma_ch1);
    let dma_ch2 = PeripheralRef::new(dma_ch2);

    let mut psram = PsRam {
        sm,
        tx_ch: dma_ch1,
        rx_ch: dma_ch2,
    };

    // Issue a reset command
    psram.send_command(&[8, 0, PSRAM_CMD_RSTEN], &mut []).await;
    Timer::after(Duration::from_micros(50)).await;
    psram.send_command(&[8, 0, PSRAM_CMD_RST], &mut []).await;
    Timer::after(Duration::from_micros(100)).await;

    for i in 0..10u8 {
        psram.write8(i as u32, i).await;
    }
    for i in 0..10u32 {
        let n = psram.read8(i as u32).await;
        log::info!("{i} = {n}");
    }
    log::info!("testing read again 0");
    let mut got = [0u8; 8];
    psram.read(0, &mut got).await;
    log::info!("got = {got:x?}");

    log::info!("testing write of deadbeef at 0");
    psram
        .write(0, &[0xd, 0xe, 0xa, 0xd, 0xb, 0xe, 0xe, 0xf])
        .await;

    log::info!("testing read of deadbeef from 0");
    psram.read(0, &mut got).await;
    log::info!("got = {got:x?}");
    psram.read(0, &mut got).await;
    log::info!("again = {got:x?}");

    for i in 0..10u32 {
        let n = psram.read8(i as u32).await;
        log::info!("{i} = {n:x}");
    }

    const TEST_STRING: &[u8] = b"hello there, this is a test, how is it?";
    psram.write(16, TEST_STRING).await;

    let mut buffer = [0u8; 42];
    psram.read(16, &mut buffer).await;
    Timer::after(Duration::from_millis(100)).await;

    for (idx, (expect, got)) in TEST_STRING.iter().zip(buffer.iter()).enumerate() {
        if *expect != *got {
            log::info!("mismatch at idx={idx} expect={:x} got={:x}", *expect, *got);
        }
    }

    log::info!("PSRAM test complete");

    psram
}
