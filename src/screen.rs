use crate::PicoCalcDisplay;
use core::ops::{Deref, DerefMut};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
use embassy_time::{Duration, Instant, Ticker};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyleBuilder};
use embedded_graphics::pixelcolor::{Rgb565, Rgb888};
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::*;
use embedded_graphics::text::Text;
use wezterm_escape_parser::color::ColorSpec;
use wezterm_escape_parser::parser::Parser;
use wezterm_escape_parser::{Action, ControlCode, Esc, EscCode};

extern crate alloc;

pub const SCREEN_HEIGHT: u16 = 320;
pub const SCREEN_WIDTH: u16 = 320;

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

bitflags::bitflags! {
    #[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
    pub struct Attributes: u8 {
        const NONE = 0;
        const REVERSE = 1;
        const BOLD = 2;
        const HALF_BRIGHT = 4;
        const UNDERLINE = 8;
        const STRIKE_THROUGH = 16;
    }
}

const MAX_COLS: usize = 80;

#[derive(Copy, Clone)]
pub struct Line {
    pub ascii: [u8; MAX_COLS],
    pub attributes: [Attributes; MAX_COLS],
    /// The encoding for colors is two nybbles;
    /// the high nybble represents the bg color,
    /// the low nybble is the fg color.
    /// value 0 in a nybble indicates the default
    /// color for that position.
    /// value 1..=0xf is the 1-based index into ANSI_COLOR_IDX
    pub colors: [u8; MAX_COLS],
    needs_paint: bool,
}

#[derive(Debug)]
pub struct Cluster<'a> {
    pub text: &'a str,
    pub attributes: Attributes,
    pub color: u8,
    pub start_col: usize,
    pub end_col: usize,
}

use core::iter::{Copied, Enumerate, Peekable, Zip};
use core::slice::Iter;

pub struct ClusterIter<'a> {
    line: &'a Line,
    last_attr: (Attributes, u8),
    start_idx: Option<usize>,
    attr_iter: Peekable<Enumerate<Zip<Copied<Iter<'a, Attributes>>, Copied<Iter<'a, u8>>>>>,
    cursor_x: Option<usize>,
}

impl<'a> ClusterIter<'a> {
    fn take_current(&mut self, end_col: usize) -> Option<Cluster<'a>> {
        let start_col = self.start_idx.take()?;

        let byte_slice = &self.line.ascii[start_col..end_col];
        let text = core::str::from_utf8(byte_slice).unwrap_or("");

        Some(Cluster {
            text,
            start_col,
            end_col,
            attributes: self.last_attr.0,
            color: self.last_attr.1,
        })
    }
}

impl<'a> Iterator for ClusterIter<'a> {
    type Item = Cluster<'a>;

    fn next(&mut self) -> Option<Cluster<'a>> {
        loop {
            if let Some(cursor_x) = self.cursor_x {
                if let Some((idx, attr_tuple)) = self.attr_iter.peek() {
                    if *idx == cursor_x {
                        let idx = *idx;
                        let attr_tuple = *attr_tuple;
                        if let Some(cluster) = self.take_current(idx) {
                            return Some(cluster);
                        }

                        // Consume the peeked cursor position
                        self.attr_iter.next();

                        // Stage an entry for the cursor, flipping it
                        // to reverse its video attributes
                        self.last_attr = attr_tuple;
                        self.last_attr.0.toggle(Attributes::REVERSE);
                        self.start_idx = Some(idx);
                    }
                }
            }

            if let Some((idx, attr_tuple)) = self.attr_iter.next() {
                match self.start_idx {
                    Some(_) => {
                        if attr_tuple == self.last_attr {
                            continue;
                        }

                        let cluster = self.take_current(idx);
                        self.last_attr = attr_tuple;
                        self.start_idx = Some(idx);
                        return cluster;
                    }
                    None => {
                        self.start_idx = Some(idx);
                        self.last_attr = attr_tuple;
                    }
                }
            } else {
                break;
            }
        }

        self.take_current(MAX_COLS - 1)
    }
}

impl Line {
    pub fn clear(&mut self) {
        self.ascii.fill(0x20);
        self.attributes.fill(Attributes::NONE);
        self.colors.fill(0);
        self.needs_paint = true;
    }

    pub fn cluster<'a>(&'a self, cursor_x: Option<u8>) -> ClusterIter<'a> {
        ClusterIter {
            line: self,
            last_attr: (Attributes::NONE, 0),
            start_idx: None,
            attr_iter: self
                .attributes
                .iter()
                .copied()
                .zip(self.colors.iter().copied())
                .enumerate()
                .peekable(),
            cursor_x: cursor_x.map(|x| x as usize),
        }
    }
}

impl Default for Line {
    fn default() -> Line {
        Line {
            ascii: [0x20; MAX_COLS],
            attributes: [Attributes::NONE; MAX_COLS],
            colors: [0; MAX_COLS],
            needs_paint: true,
        }
    }
}

pub struct Screen {
    model: ScreenModel,
    parser: Parser,
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
            parser: Parser::new(),
        }
    }

    pub fn parse_bytes(&mut self, bytes: &[u8]) {
        self.parser
            .parse(bytes, |action| self.model.apply_action(action));
    }

    pub fn print(&mut self, text: &str) {
        self.parse_bytes(text.as_bytes())
    }
}

impl ScreenModel {
    fn apply_action(&mut self, action: Action) {
        match action {
            Action::Print(c) => {
                self.print(c);
            }
            Action::PrintString(s) => {
                for c in s.chars() {
                    self.print(c);
                }
            }
            Action::Control(c) => {
                match c {
                    ControlCode::CarriageReturn => {
                        self.cursor_x = 0;
                        self.line_log_mut(self.cursor_y).unwrap().needs_paint = true;
                    }
                    ControlCode::LineFeed => {
                        self.line_log_mut(self.cursor_y).unwrap().needs_paint = true;
                        self.cursor_y.0 += 1;
                        self.check_scroll();
                    }
                    ControlCode::Backspace => {
                        // FIXME: margins!
                        if self.cursor_x == 0 {
                            self.line_log_mut(self.cursor_y).unwrap().needs_paint = true;
                            self.cursor_y.0 = self.cursor_y.0.saturating_sub(1);
                            self.cursor_x = self.width;
                        } else {
                            self.cursor_x -= 1;
                        }
                        self.line_log_mut(self.cursor_y).unwrap().needs_paint = true;
                    }
                    unhandled => {
                        log::info!("c0/c1: unhandled {unhandled:?}");
                    }
                }
            }
            Action::Esc(esc) => match esc {
                unhandled @ Esc::Unspecified { .. } => {
                    log::info!("esc: unhandled {unhandled:?}");
                }
                Esc::Code(EscCode::StringTerminator) => {}
                unhandled => {
                    log::info!("esc: unhandled {unhandled:?}");
                }
            },
            Action::CSI(csi) => {
                use wezterm_escape_parser::csi::*;

                match csi {
                    CSI::Edit(Edit::EraseInLine(EraseInLine::EraseToEndOfLine)) => {
                        let x = self.cursor_x;
                        let current_attributes = self.current_attributes;
                        let current_color = self.current_color;
                        let line = self.line_log_mut(self.cursor_y).unwrap();
                        for (ascii, (attr, color)) in line
                            .ascii
                            .iter_mut()
                            .zip(line.attributes.iter_mut().zip(line.colors.iter_mut()))
                            .skip(x as usize)
                        {
                            *ascii = 0x20;
                            *attr = current_attributes;
                            *color = current_color;
                        }
                        line.needs_paint = true;
                    }
                    CSI::Edit(Edit::EraseInDisplay(EraseInDisplay::EraseDisplay)) => {
                        // Erase in display
                        for y in 0..self.height {
                            if let Some(line) = self.line_log_mut(LogicalY(y)) {
                                line.clear();
                            }
                        }
                    }
                    CSI::Sgr(Sgr::Intensity(Intensity::Bold)) => {
                        self.current_attributes.set(Attributes::BOLD, true);
                        self.current_attributes.set(Attributes::HALF_BRIGHT, false);
                    }
                    CSI::Sgr(Sgr::Intensity(Intensity::Normal)) => {
                        self.current_attributes.set(Attributes::BOLD, false);
                        self.current_attributes.set(Attributes::HALF_BRIGHT, false);
                    }
                    CSI::Sgr(Sgr::Intensity(Intensity::Half)) => {
                        self.current_attributes.set(Attributes::BOLD, false);
                        self.current_attributes.set(Attributes::HALF_BRIGHT, true);
                    }
                    CSI::Sgr(Sgr::StrikeThrough(enable)) => {
                        self.current_attributes
                            .set(Attributes::STRIKE_THROUGH, enable);
                    }
                    CSI::Sgr(Sgr::Inverse(enable)) => {
                        self.current_attributes.set(Attributes::REVERSE, enable);
                    }
                    CSI::Sgr(Sgr::Italic(_enable)) => {}
                    CSI::Sgr(Sgr::Blink(_)) => {}
                    CSI::Sgr(Sgr::Underline(Underline::None)) => {
                        self.current_attributes.set(Attributes::UNDERLINE, false);
                    }
                    CSI::Sgr(Sgr::Underline(_)) => {
                        self.current_attributes.set(Attributes::UNDERLINE, true);
                    }
                    CSI::Sgr(Sgr::Reset) => {
                        self.current_attributes = Attributes::NONE;
                        self.current_color = 0;
                    }
                    CSI::Sgr(Sgr::Foreground(ColorSpec::Default)) => {
                        // Set default fg
                        self.current_color &= 0xf0;
                    }
                    CSI::Sgr(Sgr::Background(ColorSpec::Default)) => {
                        // Set default bg
                        self.current_color &= 0x0f;
                    }
                    CSI::Sgr(Sgr::Foreground(ColorSpec::PaletteIndex(idx))) => {
                        // Set fg color
                        self.current_color &= 0xf0;
                        self.current_color |= (idx + 1) as u8;
                    }
                    CSI::Sgr(Sgr::Background(ColorSpec::PaletteIndex(idx))) => {
                        // Set bg color
                        self.current_color &= 0x0f;
                        self.current_color |= ((idx + 1) as u8) << 4;
                    }
                    unhandled => {
                        log::info!("csi: unhandled {unhandled:?}");
                    }
                }
            }
            Action::OperatingSystemCommand(osc) => {
                log::info!("osc: unhandled {osc:?}");
            }
            Action::DeviceControl(ctrl) => {
                log::info!("unhandled {ctrl:?}");
            }
            Action::Sixel(_sixel) => {}
            Action::XtGetTcap(_tcap) => {}
            Action::KittyImage(_img) => {}
        }
    }

    fn print(&mut self, c: char) {
        let ascii = if c.is_ascii() {
            c as u32 as u8
        } else {
            0x20 // space
        };

        let cursor_x = self.cursor_x as usize;
        let attributes = self.current_attributes;
        let color = self.current_color;
        let line = self.line_log_mut(self.cursor_y).unwrap();
        line.needs_paint = true;
        line.ascii[cursor_x] = ascii;
        line.attributes[cursor_x] = attributes;
        line.colors[cursor_x] = color;
        self.cursor_x += 1;
        if self.cursor_x >= self.width {
            self.cursor_x = 0;
            self.cursor_y.0 += 1;
            self.line_log_mut(self.cursor_y).unwrap().needs_paint = true;
            self.check_scroll();
        }
    }
}

const MAX_LINES: usize = 60;

const ANSI_COLOR_IDX: [Rgb888; 16] = [
    // Black
    Rgb888::new(0x00, 0x00, 0x00),
    // Maroon
    Rgb888::new(0xcc, 0x55, 0x55),
    // Green
    Rgb888::new(0x55, 0xcc, 0x55),
    // Olive
    Rgb888::new(0xcd, 0xcd, 0x55),
    // Navy
    Rgb888::new(0x54, 0x55, 0xcb),
    // Purple
    Rgb888::new(0xcc, 0x55, 0xcc),
    // Teal
    Rgb888::new(0x7a, 0xca, 0xca),
    // Silver
    Rgb888::new(0xcc, 0xcc, 0xcc),
    // Grey
    Rgb888::new(0x55, 0x55, 0x55),
    // Red
    Rgb888::new(0xff, 0x55, 0x55),
    // Lime
    Rgb888::new(0x55, 0xff, 0x55),
    // Yellow
    Rgb888::new(0xff, 0xff, 0x55),
    // Blue
    Rgb888::new(0x55, 0x55, 0xff),
    // Fuchsia
    Rgb888::new(0xff, 0x55, 0xff),
    // Aqua
    Rgb888::new(0x55, 0xff, 0xff),
    // White
    Rgb888::new(0xff, 0xff, 0xff),
];

fn color_nybble(nybble: u8, default_value: Rgb565) -> Rgb565 {
    if nybble == 0 {
        return default_value;
    }

    let idx = nybble as usize - 1;
    let color = ANSI_COLOR_IDX[idx].into();

    color
}

pub struct ScreenModel {
    lines: [Line; MAX_LINES],
    /// cursor x,y in logical coordinates
    cursor_x: u8,
    cursor_y: LogicalY,
    current_attributes: Attributes,
    current_color: u8,
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
    pub fn clear(&mut self) {
        for line in &mut self.lines {
            line.clear();
        }
        self.cursor_x = 0;
        self.cursor_y = LogicalY(0);
        self.current_attributes = Attributes::NONE;
        self.current_color = 0;
        self.first_line_idx = 0;
        self.full_repaint = true;
        self.pixel_offset_first_line = 0;
    }

    fn check_scroll(&mut self) {
        log::trace!(
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
        self.line_log_mut(self.cursor_y).unwrap().needs_paint = true;
        log::trace!(
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

        let font = self.font;

        let pixel_offset = self.pixel_offset_first_line;

        let boundary_y = (480 as u32 / font.character_size.height) * font.character_size.height;
        let boundary_height = 480 as u32 - boundary_y;

        let mut num_changed = 0;
        let mut row_y = pixel_offset as u32;

        let mut draw_cluster = |cluster: &Cluster<'_>, row_y: u32| -> bool {
            let fg_color = if cluster.attributes.contains(Attributes::HALF_BRIGHT) {
                Rgb565::CSS_DARK_GREEN
            } else if cluster.attributes.contains(Attributes::BOLD) {
                Rgb565::CSS_SALMON
            } else {
                color_nybble(cluster.color & 0xf, Rgb565::GREEN)
            };
            let bg_color = color_nybble((cluster.color >> 4) & 0xf, Rgb565::BLACK);

            let (fg_color, bg_color) = if cluster.attributes.contains(Attributes::REVERSE) {
                (bg_color, fg_color)
            } else {
                (fg_color, bg_color)
            };

            let style = MonoTextStyleBuilder::new()
                .font(font)
                .text_color(fg_color)
                .background_color(bg_color)
                .build();

            let cell_width = font.character_size.width + font.character_spacing;
            let start_x = cluster.start_col as u32 * cell_width;
            let end_x = cluster.end_col as u32 * cell_width;
            let pixel_width = end_x - start_x;

            display
                .fill_solid(
                    &Rectangle::new(
                        Point::new(start_x as i32, row_y as i32 % 480),
                        Size::new(pixel_width, font.character_size.height as u32),
                    ),
                    bg_color,
                )
                .unwrap();

            Text::new(
                cluster.text,
                Point::new(start_x as i32, (row_y as i32 + font.baseline as i32) % 480),
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
                            Point::new(start_x as i32, (row_y as i32 + offset) % 480),
                            Size::new(pixel_width, boundary_height),
                        ),
                        bg_color,
                    )
                    .unwrap();
                Text::new(
                    cluster.text,
                    Point::new(
                        start_x as i32,
                        (row_y as i32 + font.baseline as i32 + offset) % 480,
                    ),
                    style,
                )
                .draw(display)
                .unwrap();

                true
            } else {
                false
            }
        };

        let cursor_x = self.cursor_x;
        let cursor_y = self.cursor_y;

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

            for cluster in line.cluster(if y == cursor_y { Some(cursor_x) } else { None }) {
                //log::info!("line {idx} cluster {cluster:?}");
                draw_cluster(&cluster, row_y);
            }

            row_y = (row_y + font.character_size.height) % 480;
        }

        if num_changed > 0 {
            //log::info!("clear next row @ {row_y}");

            let blank_cluster = Cluster {
                text: "",
                start_col: 0,
                end_col: MAX_COLS,
                attributes: Attributes::NONE,
                color: 0,
            };
            draw_cluster(&blank_cluster, row_y);
            if boundary_height > 0 {
                //log::info!("clear EXTRA row @ {}", row_y + font.character_size.height);
                draw_cluster(&blank_cluster, row_y + font.character_size.height);
            }

            log::trace!(
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
            current_attributes: Attributes::NONE,
            current_color: 0,
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

pub async fn cls_command(_args: &[&str]) {
    SCREEN.get().lock().await.clear();
}
