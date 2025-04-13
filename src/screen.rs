use crate::{PicoCalcDisplay, SCREEN_HEIGHT, SCREEN_WIDTH};
use core::ops::{Deref, DerefMut};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use vtparse::{CsiParam, VTActor, VTParser};

static FONTS: &[&MonoFont] = &[
    &profont::PROFONT_7_POINT,
    &profont::PROFONT_9_POINT,
    &profont::PROFONT_10_POINT,
    &profont::PROFONT_12_POINT,
    &profont::PROFONT_14_POINT,
    &profont::PROFONT_18_POINT,
    &profont::PROFONT_24_POINT,
];

pub static SCREEN: LazyLock<AsyncMutex<CriticalSectionRawMutex, Screen>> =
    LazyLock::new(|| AsyncMutex::new(Screen::new()));

#[derive(Copy, Clone)]
pub struct Line {
    pub ascii: [u8; 80],
}

impl Default for Line {
    fn default() -> Line {
        Line { ascii: [0x20; 80] }
    }
}

pub struct Screen {
    model: ScreenModel,
    vt_parser: VTParser,
}

impl Deref for Screen {
    type Target = ScreenModel;
    fn deref(&self) -> &ScreenModel {
        &self.model
    }
}

impl DerefMut for Screen {
    fn deref_mut(&mut self) -> &mut ScreenModel {
        &mut self.model
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            model: ScreenModel::default(),
            vt_parser: VTParser::new(),
        }
    }

    pub fn parse_bytes(&mut self, bytes: &[u8]) {
        self.vt_parser.parse(bytes, &mut self.model);
    }

    pub fn print(&mut self, text: &str) {
        self.parse_bytes(text.as_bytes())
    }
}

impl VTActor for ScreenModel {
    fn print(&mut self, c: char) {
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

    fn execute_c0_or_c1(&mut self, c: u8) {
        match c {
            b'\r' => {
                self.x = 0;
            }
            b'\n' => {
                self.y += 1;
                // FIXME: scroll
            }
            _ => {}
        }
    }

    fn dcs_hook(&mut self, _: u8, _: &[i64], _: &[u8], _: bool) {}
    fn dcs_put(&mut self, _: u8) {}
    fn dcs_unhook(&mut self) {}
    fn esc_dispatch(&mut self, _: &[i64], _: &[u8], _: bool, _: u8) {}
    fn csi_dispatch(&mut self, _: &[CsiParam], _: bool, _: u8) {}
    fn osc_dispatch(&mut self, _: &[&[u8]]) {}
}

pub struct ScreenModel {
    pub lines: [Line; 60],
    pub x: u8,
    pub y: u8,
    pub width: u8,
    pub height: u8,
    pub font: &'static MonoFont<'static>,
    pub full_repaint: bool,
}

impl core::fmt::Write for Screen {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.print(s);
        Ok(())
    }
}

impl ScreenModel {
    pub fn increase_font(&mut self) {
        let Some(idx) = FONTS.iter().position(|&f| f == self.font) else {
            return;
        };
        if let Some(font) = FONTS.get(idx + 1) {
            self.font = font;
            self.full_repaint = true;
        }
    }

    pub fn decrease_font(&mut self) {
        let Some(idx) = FONTS.iter().position(|&f| f == self.font) else {
            return;
        };
        if let Some(font) = FONTS.get(idx.saturating_sub(1)) {
            self.font = font;
            self.full_repaint = true;
        }
    }

    pub fn update_display(&mut self, display: &mut PicoCalcDisplay) {
        if self.full_repaint {
            display.clear(Rgb565::BLACK).unwrap();
            self.full_repaint = false;
        }

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
        let font = FONTS[2];
        ScreenModel {
            x: 0,
            y: 0,
            width: ((SCREEN_WIDTH as u32) / (font.character_size.width + font.character_spacing))
                as u8,
            height: ((SCREEN_HEIGHT as u32) / font.character_size.height) as u8,
            font,

            lines: [Line::default(); 60],
            full_repaint: true,
        }
    }
}
