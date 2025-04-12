use crate::Irqs;
use embassy_futures::yield_now;
use embassy_rp::PeripheralRef;
use embassy_rp::gpio::Drive;
use embassy_rp::peripherals::{DMA_CH1, DMA_CH2, PIN_2, PIN_3, PIN_20, PIN_21, PIO1};
use embassy_rp::pio::program::pio_asm;
use embassy_rp::pio::{Config, Direction, Pio, ShiftDirection};
use embassy_time::{Duration, Instant, Timer};
use fixed::FixedU32;
use fixed::types::extra::U8;

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
    pub size: u32,
}

impl PsRam {
    pub async fn send_command(&mut self, cmd: &[u8], out: &mut [u8]) {
        if out.is_empty() {
            self.sm
                .tx()
                .dma_push(self.tx_ch.reborrow(), cmd, false)
                .await;
        } else {
            let (rx, tx) = self.sm.rx_tx();
            tx.dma_push(self.tx_ch.reborrow(), cmd, false).await;
            rx.dma_pull(self.rx_ch.reborrow(), out, false).await;
        }
    }

    pub async fn write(&mut self, mut addr: u32, mut data: &[u8]) {
        // I haven't seen this work reliably over 24 bytes
        const MAX_CHUNK: usize = 24;
        while data.len() > 0 {
            let to_write = data.len().min(MAX_CHUNK);
            //log::info!("writing {to_write} @ {addr}");

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

    pub async fn read_id(&mut self) -> [u8; 3] {
        let mut id = [0u8; 3];
        #[rustfmt::skip]
        self.send_command(
            &[
                32,    // write 32 bits
                3 * 8, // read 8 bytes = 64 bits
                PSRAM_CMD_READ_ID,
                // don't care: 24-bit "address"
                0, 0, 0,
            ],
            &mut id,
        )
        .await;
        id
    }

    pub async fn read(&mut self, mut addr: u32, mut out: &mut [u8]) {
        // Cannot get reliable reads above 4 bytes at a time.
        // out[4] will always have a bit error
        const MAX_CHUNK: usize = 4;
        while out.len() > 0 {
            let to_read = out.len().min(MAX_CHUNK);
            //log::info!("reading {to_read} @ {addr}");
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
        //log::info!("write8 addr {addr} <- {data:x}");
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

    let clock_hz = FixedU32::from_num(embassy_rp::clocks::clk_sys_freq());
    let max_psram_freq: FixedU32<U8> = FixedU32::from_num(100_000_000);

    let divider = if clock_hz <= max_psram_freq {
        FixedU32::from_num(1)
    } else {
        clock_hz / max_psram_freq
    };
    let effective_clock = clock_hz / divider;
    use embassy_rp::clocks::*;
    log::info!(
        "pll_sys_freq={} rosc_freq={} xosc_freq={}",
        pll_sys_freq(),
        rosc_freq(),
        xosc_freq()
    );
    log::info!("sys clock is {clock_hz}. using divider {divider} -> clock {effective_clock}",);

    // This pio program was taken from
    // <https://github.com/polpo/rp2040-psram/blob/7786c93ec8d02dbb4f94a2e99645b25fb4abc2db/psram_spi.pio>
    // which is Copyright © 2023 Ian Scott, reproduced here under the MIT license

    let p = pio_asm!(
        r#"
.side_set 2                        ; sideset bit 1 is SCK, bit 0 is CS
begin:
    out x, 8            side 0b01  ; x = number of bits to output. CS deasserted
    out y, 8            side 0b01  ; y = number of bits to input
    jmp x--, writeloop  side 0b01  ; Pre-decement x by 1 so loop has correct number of iterations
writeloop:
    out pins, 1         side 0b00  ; Write value on pin, lower clock. CS asserted
    jmp x--, writeloop  side 0b10  ; Raise clock: this is when PSRAM reads the value. Loop if we have more to write
    jmp !y,  done       side 0b00  ; If this is a write-only operation, jump back to beginning
    nop                 side 0b10  ; Fudge factor of extra clock cycle; the PSRAM needs 1 extra for output to start appearing
    jmp readloop_mid    side 0b00  ; Jump to middle of readloop to decrement y and get right clock phase
readloop:
    in pins, 1          side 0b00  ; Read value on pin, lower clock. Datasheet says to read on falling edge > 83MHz
readloop_mid:
    jmp y--, readloop   side 0b10  ; Raise clock. Loop if we have more to read
done:
    nop                 side 0b11  ; CS deasserted
    "#
    );
    let prog = pio.common.load_program(&p.program);

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
    cfg.clock_divider = divider;

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
        size: 0,
    };

    // Issue a reset command
    psram.send_command(&[8, 0, PSRAM_CMD_RSTEN], &mut []).await;
    Timer::after(Duration::from_micros(50)).await;
    psram.send_command(&[8, 0, PSRAM_CMD_RST], &mut []).await;
    Timer::after(Duration::from_micros(100)).await;

    log::info!("Verifying 1 byte write and read...");
    for i in 0..10u8 {
        psram.write8(i as u32, i).await;
    }
    for i in 0..10u32 {
        let n = psram.read8(i as u32).await;
        if n as u32 != i {
            log::error!("error @ {i}, expected {i}, but got {n}");
        }
    }
    log::info!("testing read again @ 0");
    let mut got = [0u8; 8];
    psram.read(0, &mut got).await;
    const EXPECT: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7];
    if got != EXPECT {
        log::error!("got = {got:x?} but expected {EXPECT:x?}");
    }

    const DEADBEEF: &[u8] = &[0xd, 0xe, 0xa, 0xd, 0xb, 0xe, 0xe, 0xf];
    log::info!("testing write of deadbeef at 0");
    psram.write(0, DEADBEEF).await;

    log::info!("testing read of deadbeef from 0");
    psram.read(0, &mut got).await;
    if got != DEADBEEF {
        log::error!("got = {got:x?}, but expected {DEADBEEF:x?}");

        for addr in 0..DEADBEEF.len() {
            let bad = got[addr];
            if bad != DEADBEEF[addr] {
                let x = psram.read8(addr as u32).await;
                log::error!("addr = {addr:x}, bad was {bad:x}, read single again -> {x:x}");
            }
        }
    }

    const TEST_STRING: &[u8] = b"hello there, this is a test, how is it?";
    psram.write(16, TEST_STRING).await;

    let mut buffer = [0u8; 42];
    psram.read(16, &mut buffer).await;

    let got = &buffer[0..TEST_STRING.len()];

    if got != TEST_STRING {
        log::error!("mismatch got {got:x?}");
        log::error!("expected     {TEST_STRING:x?}");
    }

    log::info!("PSRAM test complete");

    let id = psram.read_id().await;
    // id: [d, 5d, 53, 15, 49, e3, 7c, 7b]
    // id[0] -- manufacturer id
    // id[1] -- "known good die" status
    log::info!("id: {id:x?}");
    if id[1] == PSRAM_KNOWN_GOOD_DIE_PASS {
        // See <https://github.com/espressif/esp-idf/blob/1c468f68259065ef51afd114605d9122f13d9d72/components/esp_psram/esp32/esp_psram_impl_quad.c#L67-L86>
        // for information on deciding the size of ESP PSRAM chips,
        // such as the one used in the picocalc
        let size = match (id[2] >> 5) & 0x7 {
            0 => 16,
            1 => 32,
            2 => 64,
            _ => 0,
        };
        psram.size = size * 1024 * 1024 / 8;
        log::info!("psram is {size} Mbits, {} bytes", psram.size);
    }

    psram
}

#[allow(unused)]
async fn test_psram(psram: &mut PsRam) -> bool {
    const REPORT_CHUNK: u32 = 256 * 1024;
    const BLOCK_SIZE: usize = 8;
    let limit = psram.size; //.min(4 * 1024 * 1024);

    log::info!("testing {BLOCK_SIZE} byte reads and writes");
    let start = Instant::now();

    fn expect(addr: u32) -> [u8; BLOCK_SIZE] {
        [
            !((addr >> 24 & 0xff) as u8),
            !((addr >> 16 & 0xff) as u8),
            !((addr >> 8 & 0xff) as u8),
            !((addr & 0xff) as u8),
            ((addr >> 24 & 0xff) as u8),
            ((addr >> 16 & 0xff) as u8),
            ((addr >> 8 & 0xff) as u8),
            ((addr & 0xff) as u8),
        ]
    }

    for i in 0..limit / BLOCK_SIZE as u32 {
        let addr = i * BLOCK_SIZE as u32;
        let data = expect(addr);
        psram.write(addr, &data).await;
        if addr > 0 && addr % REPORT_CHUNK == 0 {
            if start.elapsed() > Duration::from_secs(5) {
                log::info!(
                    "writing, addr={addr:x}, elapsed={}s, {}/s",
                    start.elapsed().as_secs(),
                    addr as u64 / start.elapsed().as_secs().max(1)
                );
            }
        }
        // Yield so that the watchdog doesn't kick in
        yield_now().await;
    }
    let writes_took = start.elapsed();

    log::info!("Starting reads...");
    Timer::after(Duration::from_millis(200)).await;

    let start = Instant::now();
    let mut bad_count = 0;
    let mut data = [0u8; BLOCK_SIZE];
    for i in 0..limit / BLOCK_SIZE as u32 {
        let addr = i * BLOCK_SIZE as u32;
        let expect = expect(addr);
        psram.read(addr, &mut data).await;
        if addr == 0 {
            log::info!("first chunk is {data:x?}, expect {expect:x?}");
            Timer::after(Duration::from_millis(200)).await;
        }
        if data != expect {
            bad_count += 1;
            if bad_count < 50 {
                log::info!("bad read @{addr:x} got {data:x?} vs {expect:x?}",);
            }
        }
        if addr > 0 && addr % REPORT_CHUNK == 0 {
            if start.elapsed() > Duration::from_secs(5) {
                log::info!(
                    "reading, bad={bad_count}, addr={addr:x}, elapsed={}s, {}/s",
                    start.elapsed().as_secs(),
                    addr as u64 / start.elapsed().as_secs().max(1)
                );
            }
        }

        // Yield so that the watchdog doesn't kick in
        yield_now().await;
    }
    let reads_took = start.elapsed();

    log::info!(
        "COMPLETED {BLOCK_SIZE} byte check of {limit} bytes. {bad_count} bad chunks. Writes took {}s {}/s, reads took {}s {}/s",
        writes_took.as_secs(),
        limit as u64 / writes_took.as_secs().max(1),
        reads_took.as_secs(),
        limit as u64 / reads_took.as_secs().max(1),
    );

    bad_count == 0
}
