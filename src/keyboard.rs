use crate::process::SHELL;
use crate::screen::SCREEN;
use embassy_rp::i2c::I2c;
use embassy_rp::peripherals::I2C1;
use embassy_time::{Duration, Ticker};

const KBD_ADDR: u8 = 0x1f;
const REG_ID_BKL: u8 = 0x05;
const REG_ID_FIF: u8 = 0x09;
const REG_ID_BK2: u8 = 0x0a;

#[derive(Debug, Default, PartialEq, Clone, Copy)]
#[repr(u8)]
pub enum KeyState {
    #[default]
    Idle = 0,
    Pressed = 1,
    Hold = 2,
    Released = 3,
}

impl From<u8> for KeyState {
    fn from(s: u8) -> Self {
        match s {
            1 => Self::Pressed,
            2 => Self::Hold,
            3 => Self::Released,
            0 | _ => Self::Idle,
        }
    }
}

#[derive(Debug, Default, PartialEq, Clone, Copy)]
pub enum Key {
    #[default]
    None,
    JoyUp,
    JoyDown,
    JoyLeft,
    JoyRight,
    JoyCenter,
    ButtonLeft1,
    ButtonRight1,
    ButtonLeft2,
    ButtonRight2,
    BackSpace,
    Tab,
    Enter,
    ModAlt,
    ModShiftLeft,
    ModShiftRight,
    ModSymbol,
    ModControl,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Break,
    Insert,
    Home,
    Del,
    End,
    PageUp,
    PageDown,
    CapsLock,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    Char(char),
    Other(u8),
}

impl From<u8> for Key {
    fn from(k: u8) -> Self {
        match k {
            0 => Self::None,
            1 => Self::JoyUp,
            2 => Self::JoyDown,
            3 => Self::JoyLeft,
            4 => Self::JoyRight,
            5 => Self::JoyCenter,
            6 => Self::ButtonLeft1,
            7 => Self::ButtonRight1,
            8 => Self::BackSpace,
            9 => Self::Tab,
            0x0a => Self::Enter,
            0x11 => Self::ButtonLeft2,
            0x12 => Self::ButtonRight2,
            0xa1 => Self::ModAlt,
            0xa2 => Self::ModShiftLeft,
            0xa3 => Self::ModShiftRight,
            0xa4 => Self::ModSymbol,
            0xa5 => Self::ModControl,
            0xb1 => Self::Escape,
            0xb4 => Self::Left,
            0xb5 => Self::Up,
            0xb6 => Self::Down,
            0xb7 => Self::Right,
            0xd0 => Self::Break,
            0xd1 => Self::Insert,
            0xd2 => Self::Home,
            0xd4 => Self::Del,
            0xd5 => Self::End,
            0xd6 => Self::PageUp,
            0xd7 => Self::PageDown,
            0xc1 => Self::CapsLock,
            0x81 => Self::F1,
            0x82 => Self::F2,
            0x83 => Self::F3,
            0x84 => Self::F4,
            0x85 => Self::F5,
            0x86 => Self::F6,
            0x87 => Self::F7,
            0x88 => Self::F8,
            0x89 => Self::F9,
            0x90 => Self::F10,
            _ => match char::from_u32(k as u32) {
                Some(c) => Self::Char(c),
                None => Self::Other(k),
            },
        }
    }
}

#[derive(Debug, Default, PartialEq, Clone, Copy)]
pub struct KeyReport {
    pub state: KeyState,
    pub key: Key,
    pub modifiers: Modifiers,
}

bitflags::bitflags! {
    #[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
    pub struct Modifiers: u8 {
        const NONE = 0;
        const CTRL = 1;
        const ALT = 2;
        const LSHIFT = 4;
        const RSHIFT = 8;
        const SYM = 16;
    }
}

#[derive(Default)]
pub struct KeyBoardState {
    last_key: (KeyState, Key),
    modifiers: Modifiers,
}

impl KeyBoardState {
    pub async fn process(
        &mut self,
        i2c_bus: &mut I2c<'_, I2C1, embassy_rp::i2c::Async>,
    ) -> Option<KeyReport> {
        let key = read_keyboard(i2c_bus).await.ok()?;
        if key == self.last_key {
            return None;
        }

        self.last_key = key;
        let (state, key) = key;
        match (state, key) {
            (KeyState::Idle, Key::None) => return None,
            (s @ KeyState::Hold | s @ KeyState::Released, Key::ModAlt) => {
                self.modifiers.set(Modifiers::ALT, s == KeyState::Hold);
            }
            (s @ KeyState::Hold | s @ KeyState::Released, Key::ModControl) => {
                self.modifiers.set(Modifiers::CTRL, s == KeyState::Hold);
            }
            (s @ KeyState::Hold | s @ KeyState::Released, Key::ModShiftLeft) => {
                self.modifiers.set(Modifiers::LSHIFT, s == KeyState::Hold);
            }
            (s @ KeyState::Hold | s @ KeyState::Released, Key::ModShiftRight) => {
                self.modifiers.set(Modifiers::RSHIFT, s == KeyState::Hold);
            }
            (s @ KeyState::Hold | s @ KeyState::Released, Key::ModSymbol) => {
                self.modifiers.set(Modifiers::SYM, s == KeyState::Hold);
            }
            _ => {}
        }
        Some(KeyReport {
            state,
            key,
            modifiers: self.modifiers,
        })
    }
}

/// Control the lcd backlight brightness level.
/// The firmware uses the value as a pwm signal at 10_000 Hz.
/// https://github.com/clockworkpi/PicoCalc/blob/939b9bbad9030655a35ff07062024691abb12240/Code/picocalc_keyboard/backlight.ino#L20-L31
#[allow(unused)]
pub async fn set_lcd_backlight(i2c_bus: &mut I2c<'_, I2C1, embassy_rp::i2c::Async>, level: u8) {
    let _ = i2c_bus.write_async(KBD_ADDR, [REG_ID_BKL, level]).await;
}

/// Control the keyboard backlight brightness level.
/// The firmware uses the value as a pwm signal at 10_000 Hz.
/// Values < 20 turn off the keyboard backlight
#[allow(unused)]
pub async fn set_keyboard_backlight(
    i2c_bus: &mut I2c<'_, I2C1, embassy_rp::i2c::Async>,
    level: u8,
) {
    let _ = i2c_bus.write_async(KBD_ADDR, [REG_ID_BK2, level]).await;
}

async fn read_keyboard(
    i2c_bus: &mut I2c<'_, I2C1, embassy_rp::i2c::Async>,
) -> Result<(KeyState, Key), embassy_rp::i2c::Error> {
    let mut buf = [0u8; 2];
    i2c_bus
        .write_read_async(KBD_ADDR, [REG_ID_FIF], &mut buf)
        .await?;
    Ok((buf[0].into(), buf[1].into()))
}

#[embassy_executor::task]
pub async fn keyboard_reader(
    mut i2c_bus: embassy_rp::i2c::I2c<
        'static,
        embassy_rp::peripherals::I2C1,
        embassy_rp::i2c::Async,
    >,
) {
    let mut keyboard = KeyBoardState::default();

    // First, drain any keys that might be buffered in its FIFO
    // prior to the last system reset. This prevents pending
    // key repeats of reset key combinations from triggering
    // as soon as we restart.
    while let Ok((state, key)) = read_keyboard(&mut i2c_bus).await {
        if state == KeyState::Idle && key == Key::None {
            // Drained
            break;
        }
    }

    // The keyboard MCU polls every 16ms, so let's match that
    let mut kbd_ticker = Ticker::every(Duration::from_millis(16));
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
                        log::warn!("Rebooting into BOOTSEL...");
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
                        log::warn!("Rebooting...");
                        embassy_rp::rom_data::reboot(
                            REBOOT_TYPE_NORMAL | NO_RETURN_ON_SUCCESS,
                            100,
                            0,
                            0,
                        );
                        loop {}
                    }
                    Key::Char('=') if key.modifiers == Modifiers::CTRL => {
                        SCREEN.get().lock().await.increase_font();
                    }
                    Key::Char('-') if key.modifiers == Modifiers::CTRL => {
                        SCREEN.get().lock().await.decrease_font();
                    }
                    _ => {
                        let shell = SHELL.get();
                        shell.key_input(key).await;
                        shell.render().await;
                    }
                }
            }
        }
    }
}
