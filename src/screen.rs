use crate::{PicoCalcDisplay, SCREEN_HEIGHT, SCREEN_WIDTH};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;

pub static SCREEN: LazyLock<AsyncMutex<CriticalSectionRawMutex, ScreenModel>> =
    LazyLock::new(|| AsyncMutex::new(ScreenModel::default()));

#[derive(Copy, Clone)]
pub struct Line {
    pub ascii: [u8; 80],
}

impl Default for Line {
    fn default() -> Line {
        Line { ascii: [0x20; 80] }
    }
}

pub struct ScreenModel {
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
