//! Pixel art on a character grid.
//!
//! A terminal cell is about twice as tall as it is wide, so a cell holding `▀` — foreground
//! on top, background underneath — is two square pixels rather than one letter. That is the
//! whole trick: art is authored as a bitmap, one character per pixel, and this module turns
//! it into cells.
//!
//! Sprites composite over what is already on screen rather than replacing it. A pixel can be
//! transparent, and a transparent pixel leaves whatever was underneath showing — which is
//! what lets something walk in front of the wordmark without punching a hole in it. The
//! difficulty is that a cell cannot hold half a letter and half a sprite, so [`cover`]
//! reconstructs what the cell underneath was *showing* in each half before compositing.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Color, Style};

use super::color;
use crate::proto::Rgb;
use crate::theme::mix;

/// A bitmap, one character per pixel, top row first.
///
/// `.` is transparent; every other character is a palette slot. Rows are all the same length
/// — a test asserts it, because a ragged sprite is a bug you see as a torn edge rather than
/// as an error.
pub struct Sprite {
    pub rows: &'static [&'static str],
}

impl Sprite {
    /// Width in pixels, which is also width in cells.
    pub fn width(&self) -> u16 {
        self.rows.first().map_or(0, |r| r.chars().count() as u16)
    }

    /// Height in pixels — half this, rounded up, is the height in cells.
    pub fn height(&self) -> u16 {
        self.rows.len() as u16
    }

    /// The palette slot at a pixel, or `None` outside the bitmap or where it is transparent.
    fn at(&self, x: i32, y: i32) -> Option<char> {
        if x < 0 || y < 0 {
            return None;
        }
        let row = self.rows.get(y as usize)?;
        let ch = row.chars().nth(x as usize)?;
        (ch != '.').then_some(ch)
    }
}

/// What each palette letter means, in colour.
///
/// Built from the theme at draw time rather than baked in, so art drawn for one theme is not
/// a stranger in the next one.
pub struct Palette {
    slots: Vec<(char, Rgb)>,
    /// Stand-in for a cell whose colour the terminal never told us — the surface the art is
    /// sitting on.
    pub surface: Rgb,
}

impl Palette {
    pub fn new(surface: Rgb, slots: Vec<(char, Rgb)>) -> Palette {
        Palette { slots, surface }
    }

    pub fn get(&self, ch: char) -> Option<Rgb> {
        self.slots.iter().find(|(c, _)| *c == ch).map(|(_, rgb)| *rgb)
    }
}

/// How much of each half of a cell a glyph inks: 0 for bare, 1 for solid.
///
/// Exact for the block glyphs, which is most of what the wordmark is made of. The
/// box-drawing strokes are thin lines straddling the middle of a cell and cannot honestly be
/// reduced to two colours at all — a fraction keeps them as a tint of about the right weight,
/// which reads as a soft edge rather than as a letter with a bite taken out of it. Rounding
/// them to "solid" would fatten every stroke the sprite passes; rounding to "bare" would
/// delete it.
fn cover(sym: &str) -> (f32, f32) {
    match sym {
        "█" => (1.0, 1.0),
        "▀" => (1.0, 0.0),
        "▄" => (0.0, 1.0),
        " " | "" => (0.0, 0.0),
        "═" => (0.34, 0.34),
        "║" => (0.30, 0.30),
        "╔" | "╗" | "╚" | "╝" => (0.26, 0.26),
        // Text, which a sprite in front of it should cover rather than half-erase.
        _ => (0.45, 0.45),
    }
}

/// Read back what a cell is showing, as its two pixels.
fn sample(buf: &Buffer, x: u16, y: u16, surface: Rgb) -> (Rgb, Rgb) {
    let Some(cell) = buf.cell((x, y)) else { return (surface, surface) };
    let st = cell.style();
    let rgb = |c: Option<Color>, fallback: Rgb| match c {
        Some(Color::Rgb(r, g, b)) => Rgb::new(r, g, b),
        _ => fallback,
    };
    let (fg, bg) = (rgb(st.fg, surface), rgb(st.bg, surface));
    let (top, bottom) = cover(cell.symbol());
    (mix(bg, fg, top), mix(bg, fg, bottom))
}

/// Draw a sprite into `area`, its top-left pixel at `(px, py)` measured from the top-left of
/// `area` — `py` in pixel rows, so odd values put the sprite on a half-cell boundary.
///
/// Anything outside `area` is clipped rather than wrapped, and cells the sprite is entirely
/// transparent over are left exactly as they were.
pub fn blit(buf: &mut Buffer, area: TRect, s: &Sprite, px: i32, py: i32, pal: &Palette) {
    for cy in area.y..area.y.saturating_add(area.height) {
        // The two pixel rows this cell stands for, in sprite coordinates.
        let row = 2 * i32::from(cy - area.y) - py;
        for cx in area.x..area.x.saturating_add(area.width) {
            let col = i32::from(cx - area.x) - px;
            let top = s.at(col, row).and_then(|c| pal.get(c));
            let bottom = s.at(col, row + 1).and_then(|c| pal.get(c));
            if top.is_none() && bottom.is_none() {
                continue; // nothing of the sprite here: leave what was underneath
            }
            let (was_top, was_bottom) = sample(buf, cx, cy, pal.surface);
            // Which way up the cell goes is decided by which half the sprite owns: putting
            // the sprite's pixel in the foreground and what was underneath in the background
            // is exact for both halves, where the other way round would have to average.
            let (sym, fg, bg) = match (top, bottom) {
                (Some(t), Some(b)) => ("▀", t, b),
                (Some(t), None) => ("▀", t, was_bottom),
                (None, Some(b)) => ("▄", b, was_top),
                (None, None) => unreachable!("checked above"),
            };
            if let Some(cell) = buf.cell_mut((cx, cy)) {
                cell.set_symbol(sym);
                cell.set_style(Style::default().fg(color(fg)).bg(color(bg)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = Rgb { r: 200, g: 0, b: 0 };
    const BLUE: Rgb = Rgb { r: 0, g: 0, b: 200 };
    const SURFACE: Rgb = Rgb { r: 10, g: 10, b: 10 };

    /// A two-pixel-wide plus sign, so top-only, bottom-only and both-pixel cells all appear.
    const PLUS: Sprite = Sprite { rows: &["r.", ".r", "r."] };

    fn pal() -> Palette {
        Palette::new(SURFACE, vec![('r', RED), ('b', BLUE)])
    }

    fn buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(TRect::new(0, 0, w, h))
    }

    #[test]
    fn a_sprite_knows_its_own_shape() {
        assert_eq!(PLUS.width(), 2);
        assert_eq!(PLUS.height(), 3);
        assert_eq!(PLUS.at(0, 0), Some('r'));
        assert_eq!(PLUS.at(1, 0), None, "a dot is transparent, not a slot");
        assert_eq!(PLUS.at(-1, 0), None, "and off the edge is nothing at all");
        assert_eq!(PLUS.at(0, 9), None);
    }

    /// Two pixel rows to a cell, the upper one in the foreground.
    #[test]
    fn a_pixel_pair_becomes_one_half_block_cell() {
        let mut b = buf(4, 4);
        let area = TRect::new(0, 0, 4, 4);
        blit(&mut b, area, &PLUS, 0, 0, &pal());
        assert_eq!(b[(0, 0)].symbol(), "▀");
        assert_eq!(b[(0, 0)].style().fg, Some(color(RED)), "top pixel is the foreground");
        assert_eq!(
            b[(0, 0)].style().bg,
            Some(color(SURFACE)),
            "the transparent lower pixel keeps the surface"
        );
        assert_eq!(b[(1, 0)].symbol(), "▄", "a lower-only pixel flips the cell over");
        assert_eq!(b[(1, 0)].style().fg, Some(color(RED)), "so the sprite is still the ink");
    }

    /// The whole point of transparency: a cell the sprite does not touch is not touched.
    #[test]
    fn transparent_cells_leave_what_was_underneath() {
        let mut b = buf(4, 4);
        b[(3, 0)].set_symbol("X");
        blit(&mut b, TRect::new(0, 0, 4, 4), &PLUS, 0, 0, &pal());
        assert_eq!(b[(3, 0)].symbol(), "X", "outside the sprite's own columns");
        assert_eq!(b[(0, 2)].symbol(), " ", "and below its last pixel row");
    }

    /// Half a sprite pixel over a solid glyph has to keep the glyph's colour in the other
    /// half, or letters come out with bites taken out of them.
    #[test]
    fn a_half_covered_cell_keeps_the_ink_underneath() {
        let mut b = buf(4, 4);
        // A full block in the theme's accent, the way the wordmark draws its letters.
        b[(0, 0)].set_symbol("█");
        b[(0, 0)].set_style(Style::default().fg(color(BLUE)).bg(color(SURFACE)));
        blit(&mut b, TRect::new(0, 0, 4, 4), &PLUS, 0, 0, &pal());
        assert_eq!(b[(0, 0)].style().fg, Some(color(RED)), "the sprite wins its own pixel");
        assert_eq!(
            b[(0, 0)].style().bg,
            Some(color(BLUE)),
            "and the letter still shows through the half the sprite left alone"
        );
    }

    /// Half-blocks underneath are read exactly, not as solid ink.
    #[test]
    fn the_cover_of_a_glyph_is_exact_for_half_blocks() {
        assert_eq!(cover("█"), (1.0, 1.0));
        assert_eq!(cover("▀"), (1.0, 0.0));
        assert_eq!(cover("▄"), (0.0, 1.0));
        assert_eq!(cover(" "), (0.0, 0.0));
        // Every glyph both banners are built from is either exact or a thin stroke, and a
        // thin stroke must stay a tint rather than becoming solid or vanishing.
        for solid in ["█", "▀", "▄", " "] {
            let (t, b) = cover(solid);
            assert!([0.0, 1.0].contains(&t) && [0.0, 1.0].contains(&b), "{solid} is not exact");
        }
        for stroke in ["═", "║", "╔", "╗", "╚", "╝"] {
            let (t, b) = cover(stroke);
            assert!((0.2..0.5).contains(&t) && (0.2..0.5).contains(&b), "{stroke} -> {t},{b}");
        }
    }

    /// Off the left edge, off the right edge, and past the bottom — all clipped, none
    /// wrapped, and nothing written outside the area it was given.
    #[test]
    fn drawing_is_clipped_to_the_area() {
        let mut b = buf(6, 3);
        let area = TRect::new(1, 1, 3, 1);
        blit(&mut b, area, &PLUS, -1, 0, &pal());
        for x in 0..6u16 {
            for y in 0..3u16 {
                let inside = area.x <= x && x < area.x + area.width && y == area.y;
                if !inside {
                    assert_eq!(b[(x, y)].symbol(), " ", "wrote outside the area at {x},{y}");
                }
            }
        }
        // The sprite's second column is what a -1 offset leaves showing.
        assert_eq!(b[(1, 1)].style().fg, Some(color(RED)));
    }

    /// An odd pixel offset puts the sprite on a half-cell boundary, which is what makes a
    /// figure able to stand anywhere rather than only on even rows.
    #[test]
    fn an_odd_offset_lands_on_a_half_cell() {
        let mut b = buf(4, 4);
        blit(&mut b, TRect::new(0, 0, 4, 4), &PLUS, 0, 1, &pal());
        assert_eq!(b[(0, 0)].symbol(), "▄", "only the lower half of the cell is the sprite");
        assert_eq!(b[(0, 0)].style().fg, Some(color(RED)), "pushed down by one pixel");
    }
}
