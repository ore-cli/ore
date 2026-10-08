//! ore's crystal, still, where upstream draws its blossom.
//!
//! Upstream's fresh-conversation stage renders OpenAI's mark procedurally, in
//! braille dots. ore paints one pose of its own crystal there instead, in the
//! same dots -- one braille cell per character of the crystal, denser where it
//! is brighter -- at whatever opacity the stage asks for, so the idle mark stays
//! faint and a click still fades it in.

use codex_ansi_escape::ansi_escape_line;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::Line;

use crate::terminal_palette;
use crate::terminal_palette::StdoutColorLevel;

/// The crystal's front pose, in the ramp the welcome screen opens on.
const POSE: usize = 0;
const RAMP: usize = 0;

/// Braille dot bits in an order that fills a cell evenly, alternating columns.
const DOT_ORDER: [u8; 8] = [0x01, 0x08, 0x04, 0x20, 0x02, 0x10, 0x40, 0x80];

/// The crystal's pale highlight, for a character the frame left uncolored.
const UNCOLORED: (u8, u8, u8) = (210, 221, 235);

/// One braille cell whose dot count follows the pixel's brightness.
fn braille_for((r, g, b): (u8, u8, u8)) -> char {
    let luminance = (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0;
    let dots = ((luminance * 8.0).round() as usize).clamp(1, DOT_ORDER.len());
    let bits = DOT_ORDER[..dots].iter().fold(0u8, |bits, dot| bits | dot);
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
}

/// Paints the largest crystal that fits `area`, centered, or nothing when even
/// the small one would be clipped.
pub(crate) fn paint(area: Rect, buffer: &mut Buffer, opacity: f32) {
    let Some(variants) = crate::frames::variants_for_area(area.width, area.height, 0) else {
        return;
    };
    let frame = variants[RAMP][POSE];
    let rows: Vec<_> = frame.lines().map(ansi_escape_line).collect();
    let cols = rows.iter().map(Line::width).max().unwrap_or(0) as u16;
    let x0 = area.x + area.width.saturating_sub(cols) / 2;
    let y0 = area.y + area.height.saturating_sub(rows.len() as u16) / 2;

    let background = terminal_palette::default_bg();
    let color_level = if background.is_some() {
        terminal_palette::effective_stdout_color_level()
    } else {
        StdoutColorLevel::Unknown
    };
    let background = background.unwrap_or((15, 20, 37));
    // Default-color terminals can only step between normal and dim intensity.
    let dim = opacity < 0.5
        && matches!(
            color_level,
            StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown
        );

    for (dy, row) in rows.iter().enumerate() {
        let y = y0 + dy as u16;
        if y >= area.bottom() {
            break;
        }
        let mut x = x0;
        for span in &row.spans {
            let rgb = match span.style.fg {
                Some(Color::Rgb(r, g, b)) => (r, g, b),
                _ => UNCOLORED,
            };
            for ch in span.content.chars() {
                if x >= area.right() {
                    break;
                }
                if !ch.is_whitespace() {
                    let cell = &mut buffer[(x, y)];
                    cell.set_char(braille_for(rgb));
                    let color = crate::color::blend(rgb, background, opacity);
                    cell.set_fg(terminal_palette::best_color_for_level(color, color_level));
                    if dim {
                        cell.set_style(cell.style().dim());
                    }
                }
                x += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn painted(area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        paint(area, &mut buffer, /*opacity*/ 0.35);
        buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn the_stage_shows_the_crystal_in_braille_dots() {
        let drawn = painted(Rect::new(0, 0, 80, 24));
        let marks: Vec<char> = drawn.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(!marks.is_empty(), "nothing was painted");
        assert!(
            marks
                .iter()
                .all(|ch| ('\u{2801}'..='\u{28ff}').contains(ch)),
            "every painted cell is a braille dot cell"
        );
    }

    #[test]
    fn brighter_pixels_get_more_dots() {
        let dots = |ch: char| (u32::from(ch) - 0x2800).count_ones();
        assert!(dots(braille_for((230, 240, 250))) > dots(braille_for((30, 60, 90))));
        assert_eq!(dots(braille_for((0, 0, 0))), 1);
    }

    #[test]
    fn a_stage_smaller_than_the_crystal_stays_empty() {
        let drawn = painted(Rect::new(0, 0, 20, 10));
        assert!(drawn.chars().all(char::is_whitespace));
    }
}
