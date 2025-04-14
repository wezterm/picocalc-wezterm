use crate::{PicoCalcDisplay, SCREEN_HEIGHT, SCREEN_WIDTH};
use core::ops::{Deref, DerefMut};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
use embassy_time::{Duration, Instant, Ticker};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::*;
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct LogicalY(u8);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct PhysicalY(u8);

#[derive(Copy, Clone)]
pub struct Line {
    pub ascii: [u8; 80],
    needs_paint: bool,
}

impl Line {
    pub fn clear(&mut self) {
        self.ascii.fill(0x20);
        self.needs_paint = true;
    }
}

impl Default for Line {
    fn default() -> Line {
        Line {
            ascii: [0x20; 80],
            needs_paint: true,
        }
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

        let cursor_x = self.cursor_x as usize;
        let line = self.line_log_mut(self.cursor_y).unwrap();
        line.needs_paint = true;
        line.ascii[cursor_x] = ascii;
        self.cursor_x += 1;
        if self.cursor_x >= self.width {
            self.cursor_x = 0;
            self.cursor_y.0 += 1;
            self.check_scroll();
        }
    }

    fn execute_c0_or_c1(&mut self, c: u8) {
        match c {
            b'\r' => {
                self.cursor_x = 0;
            }
            b'\n' => {
                self.cursor_y.0 += 1;
                self.check_scroll();
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

const MAX_LINES: usize = 60;

pub struct ScreenModel {
    lines: [Line; MAX_LINES],
    /// cursor x,y in logical coordinates
    cursor_x: u8,
    cursor_y: LogicalY,
    pub width: u8,
    pub height: u8,
    font: &'static MonoFont<'static>,
    full_repaint: bool,
    /// physical offset to logical row 0
    first_line_idx: u8,
    /// addressing to video ram for logical row 0
    pixel_offset_first_line: u16,
}

impl core::fmt::Write for Screen {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.print(s);
        Ok(())
    }
}

impl ScreenModel {
    fn check_scroll(&mut self) {
        log::info!(
            "consider scroll, y={:?}, height={} first_line_idx={} pixel={}",
            self.cursor_y,
            self.height,
            self.first_line_idx,
            self.pixel_offset_first_line,
        );
        let mut cursor_y = self.cursor_y;
        while cursor_y.0 >= self.height {
            self.line_log_mut(cursor_y).unwrap().clear();
            self.first_line_idx += 1;
            self.pixel_offset_first_line += self.font.character_size.height as u16;
            cursor_y.0 -= 1;
        }

        self.pixel_offset_first_line %= 480;
        self.cursor_y = cursor_y;
        log::info!(
            "done scroll -> y={:?}, cell_height={} height={} first_line_idx={} pixel={}",
            self.cursor_y,
            self.font.character_size.height,
            self.height,
            self.first_line_idx,
            self.pixel_offset_first_line,
        );
    }

    fn line_phys(&self, phys: PhysicalY) -> Option<&Line> {
        self.lines.get(phys.0 as usize)
    }
    fn line_phys_mut(&mut self, phys: PhysicalY) -> Option<&mut Line> {
        self.lines.get_mut(phys.0 as usize)
    }

    fn log_to_phys(&self, log: LogicalY) -> Option<PhysicalY> {
        let idx = (self.first_line_idx + log.0) % MAX_LINES as u8;
        Some(PhysicalY(idx))
    }

    fn line_log(&self, log: LogicalY) -> Option<&Line> {
        self.line_phys(self.log_to_phys(log)?)
    }
    fn line_log_mut(&mut self, log: LogicalY) -> Option<&mut Line> {
        self.line_phys_mut(self.log_to_phys(log)?)
    }

    pub fn increase_font(&mut self) {
        let Some(idx) = FONTS.iter().position(|&f| f == self.font) else {
            return;
        };
        if let Some(font) = FONTS.get(idx + 1) {
            self.change_font(font);
        }
    }

    pub fn decrease_font(&mut self) {
        let Some(idx) = FONTS.iter().position(|&f| f == self.font) else {
            return;
        };
        if let Some(font) = FONTS.get(idx.saturating_sub(1)) {
            self.change_font(font);
        }
    }

    fn change_font(&mut self, font: &'static MonoFont) {
        let old_height = self.height;

        self.font = font;
        self.full_repaint = true;
        self.width =
            ((SCREEN_WIDTH as u32) / (font.character_size.width + font.character_spacing)) as u8;
        self.height = ((SCREEN_HEIGHT as u32) / font.character_size.height) as u8;

        if self.height > old_height {
            self.first_line_idx = self.first_line_idx.saturating_sub(self.height - old_height);
        } else {
            // FIXME: account for the last non-blank line when computing
            // the revised offset
            self.first_line_idx += old_height - self.height;
        }
    }

    pub fn update_display(&mut self, display: &mut PicoCalcDisplay) {
        let start = Instant::now();
        let is_full_repaint = self.full_repaint;
        if is_full_repaint {
            display.clear(Rgb565::BLACK).unwrap();
            self.full_repaint = false;
            self.pixel_offset_first_line = 0;
        }

        let style = MonoTextStyle::new(self.font, Rgb565::GREEN);
        let font = self.font;
        let width = self.width;

        let pixel_offset = self.pixel_offset_first_line;

        let boundary_y = (480 as u32 / font.character_size.height) * font.character_size.height;
        let boundary_height = 480 as u32 - boundary_y;

        let mut num_changed = 0;
        let mut row_y = pixel_offset as u32;

        let mut draw_text = |text: &str, row_y: u32, bg_color: Rgb565| -> bool {
            display
                .fill_solid(
                    &Rectangle::new(
                        Point::new(0, row_y as i32 % 480),
                        Size::new(SCREEN_WIDTH as u32, font.character_size.height as u32),
                    ),
                    bg_color,
                )
                .unwrap();

            Text::new(
                text,
                Point::new(0, (row_y as i32 + font.baseline as i32) % 480),
                style,
            )
            .draw(display)
            .unwrap();

            if row_y % 480 >= boundary_y
                || row_y % 480 + font.character_size.height - 1 >= boundary_y
            {
                // Wrapping around end of framebuffer
                // FIXME: This isn't quite right, but I've run out of patience
                // to debug it at the moment!
                log::info!("discontinuity at @ {row_y} vs {boundary_y} ****");
                let offset = font.character_size.height as i32 - boundary_height as i32;
                display
                    .fill_solid(
                        &Rectangle::new(
                            Point::new(0, (row_y as i32 + offset) % 480),
                            Size::new(SCREEN_WIDTH as u32, boundary_height),
                        ),
                        bg_color,
                    )
                    .unwrap();
                Text::new(
                    text,
                    Point::new(0, (row_y as i32 + font.baseline as i32 + offset) % 480),
                    style,
                )
                .draw(display)
                .unwrap();

                true
            } else {
                false
            }
        };

        for idx in 0..self.height {
            let y = LogicalY(idx);
            let phys_y = self.log_to_phys(y).unwrap();
            let line = self.line_phys_mut(phys_y).unwrap();

            if !line.needs_paint && !is_full_repaint {
                row_y = (row_y + font.character_size.height) % 480;
                continue;
            }
            line.needs_paint = false;
            num_changed += 1;

            let slice = &line.ascii[0..width as usize];
            let text = core::str::from_utf8(slice).unwrap_or("");

            draw_text(text, row_y, Rgb565::BLACK);

            row_y = (row_y + font.character_size.height) % 480;
        }

        if num_changed > 0 {
            log::info!("clear next row @ {row_y}");
            draw_text("", row_y, Rgb565::BLACK);
            if boundary_height > 0 {
                log::info!("clear EXTRA row @ {}", row_y + font.character_size.height);
                draw_text("", row_y + font.character_size.height, Rgb565::BLACK);
            }

            log::info!(
                "render of {num_changed} lines took {}ms. boundary_y={boundary_y} h={boundary_height} baseline={} pixel_offset={pixel_offset}",
                start.elapsed().as_millis(),
                font.baseline
            );

            display.set_vertical_scroll_offset(pixel_offset % 480).ok();
        }
    }
}

impl Default for ScreenModel {
    fn default() -> ScreenModel {
        let font = FONTS[2];
        ScreenModel {
            cursor_x: 0,
            cursor_y: LogicalY(0),
            width: ((SCREEN_WIDTH as u32) / (font.character_size.width + font.character_spacing))
                as u8,
            height: ((SCREEN_HEIGHT as u32) / font.character_size.height) as u8,
            font,

            lines: [Line::default(); MAX_LINES],
            full_repaint: true,
            first_line_idx: 0,
            pixel_offset_first_line: 0,
        }
    }
}

#[embassy_executor::task]
pub async fn screen_painter(mut display: PicoCalcDisplay<'static>) {
    display.clear(Rgb565::BLACK).unwrap();
    if let Err(err) = display.set_vertical_scroll_region(0, 0) {
        log::error!("failed to set_vertical_scroll_region: {err:?}");
    }

    // Display update takes ~128ms @ 40_000_000
    let mut ticker = Ticker::every(Duration::from_millis(200));
    loop {
        SCREEN.get().lock().await.update_display(&mut display);
        ticker.next().await;
    }
}
