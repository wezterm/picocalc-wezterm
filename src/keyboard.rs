use embassy_rp::i2c::I2c;
use embassy_rp::peripherals::I2C1;

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

const KBD_ADDR: u8 = 0x1f;

/// Control the lcd backlight brightness level.
/// The firmware uses the value as a pwm signal at 10_000 Hz.
/// https://github.com/clockworkpi/PicoCalc/blob/939b9bbad9030655a35ff07062024691abb12240/Code/picocalc_keyboard/backlight.ino#L20-L31
pub async fn set_lcd_backlight(i2c_bus: &mut I2c<'_, I2C1, embassy_rp::i2c::Async>, level: u8) {
    let _ = i2c_bus.write_async(KBD_ADDR, [0x05, level]).await;
}

/// Control the keyboard backlight brightness level.
/// The firmware uses the value as a pwm signal at 10_000 Hz.
/// Values < 20 turn off the keyboard backlight
pub async fn set_keyboard_backlight(
    i2c_bus: &mut I2c<'_, I2C1, embassy_rp::i2c::Async>,
    level: u8,
) {
    let _ = i2c_bus.write_async(KBD_ADDR, [0x0a, level]).await;
}

async fn read_keyboard(
    i2c_bus: &mut I2c<'_, I2C1, embassy_rp::i2c::Async>,
) -> Result<(KeyState, Key), embassy_rp::i2c::Error> {
    let mut buf = [0u8; 2];
    i2c_bus.write_read_async(KBD_ADDR, [0x09], &mut buf).await?;
    Ok((buf[0].into(), buf[1].into()))
}
