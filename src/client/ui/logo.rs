//! The wordmark in the top-left corner.
//!
//! Two sizes of the same block-letter banner, in the spirit of the one an editor throws up
//! on its start screen, picked by how much of the panel can be spent on it without crowding
//! the sections underneath. Below the small one there is only room for the word itself.
//!
//! The banner is centred in its slot, with a blank row above and below, so the leftover
//! columns are split either side instead of pooling on the right. The plain-word fallback
//! stays on one line at the left margin, where it lines up with the section labels — it is
//! the shape used when there is no room to spare, so it spends none.

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{color, put_line};
use crate::theme::{mix, Theme};

struct Banner {
    /// Letterforms, top row first. Every row is the same display width.
    rows: &'static [&'static str],
}

impl Banner {
    fn width(&self) -> u16 {
        self.rows[0].chars().count() as u16
    }

    fn height(&self) -> u16 {
        self.rows.len() as u16
    }
}

#[rustfmt::skip]
const BIG: Banner = Banner {
    rows: &[
        "██╗  ██╗ ██████╗ ██████╗ ██████╗ ███████╗",
        "██║  ██║██╔═══██╗██╔══██╗██╔══██╗██╔════╝",
        "███████║██║   ██║██████╔╝██║  ██║█████╗  ",
        "██╔══██║██║   ██║██╔══██╗██║  ██║██╔══╝  ",
        "██║  ██║╚██████╔╝██║  ██║██████╔╝███████╗",
        "╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝╚═════╝ ╚══════╝",
    ],
};

#[rustfmt::skip]
const SMALL: Banner = Banner {
    rows: &[
        "█ █ █▀█ █▀▄ █▀▄ █▀▀",
        "█▀█ █ █ █▀▄ █ █ █▀▀",
        "▀ ▀ ▀▀▀ ▀ ▀ ▀▀▀ ▀▀▀",
    ],
};

/// Rows the panel keeps for its own sections before a banner is worth drawing. A logo that
/// eats the agent list is worse than no logo.
const MIN_BODY: u16 = 10;

/// Blank rows above and below the letters. Without them the banner reads as jammed between
/// the tab bar and the rule rather than sitting in a slot of its own.
const PAD_Y: u16 = 1;

/// Total rows a banner occupies, letters plus the breathing room either side.
fn slot(b: &Banner) -> u16 {
    b.height() + 2 * PAD_Y
}

fn pick(w: u16, room: u16) -> Option<&'static Banner> {
    [&BIG, &SMALL].into_iter().find(|b| w >= b.width() && room >= slot(b) + MIN_BODY)
}

/// Rows `draw` will occupy, so the panel can budget the rest of its space up front.
pub fn height(w: u16, room: u16) -> u16 {
    pick(w, room).map_or(1, slot)
}

/// Draw the wordmark in the `w` columns starting at `x, y`. Returns the rows used.
pub fn draw(buf: &mut Buffer, x: u16, y: u16, w: u16, room: u16, t: &Theme) -> u16 {
    let bg = color(t.ui.panel_bg);
    let Some(b) = pick(w, room) else {
        let style = Style::default().fg(color(t.ui.accent)).bg(bg).add_modifier(Modifier::BOLD);
        put_line(buf, x + 1, y, w.saturating_sub(1), Line::from(Span::styled("horde", style)));
        return 1;
    };

    // Split the leftover columns either side. An odd column goes to the right, where the
    // panel has its own margin anyway.
    let left = (w - b.width()) / 2;

    // The letters sink into the panel as they descend, so the word reads as rising out of
    // the dark rather than sitting flat on it.
    let last = (b.rows.len() - 1).max(1) as f32;
    for (i, row) in b.rows.iter().enumerate() {
        let fg = color(mix(t.ui.accent, t.ui.panel_bg, 0.3 * (i as f32 / last)));
        let style = Style::default().fg(fg).bg(bg);
        let ry = y + PAD_Y + i as u16;
        put_line(buf, x + left, ry, w - left, Line::from(Span::styled(*row, style)));
    }
    slot(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect as TRect;
    use unicode_width::UnicodeWidthChar;

    fn render(w: u16, room: u16) -> (String, u16) {
        let area = TRect::new(0, 0, w, room);
        let mut buf = Buffer::empty(area);
        let t = Theme::horde();
        super::super::fill(&mut buf, area, t.ui.panel_bg);
        let used = draw(&mut buf, 0, 0, w, room, &t);
        let text = (0..room)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (text, used)
    }

    /// Every row of a banner has to be the same width, or the letters shear apart.
    #[test]
    fn banner_rows_are_rectangular_and_single_width() {
        for b in [&BIG, &SMALL] {
            let w = b.width();
            for row in b.rows {
                assert_eq!(row.chars().count() as u16, w, "{row:?}");
                for ch in row.chars() {
                    assert_eq!(UnicodeWidthChar::width(ch), Some(1), "{ch:?} in {row:?}");
                }
            }
        }
    }

    #[test]
    fn a_wide_tall_panel_gets_the_big_banner() {
        let (out, used) = render(48, 24);
        println!("\n{out}\n");
        assert_eq!(used, slot(&BIG));
        assert!(out.contains(BIG.rows[0]), "{out}");
    }

    #[test]
    fn the_default_panel_width_gets_the_small_banner() {
        let (out, used) = render(24, 20);
        println!("\n{out}\n");
        assert_eq!(used, slot(&SMALL));
        assert!(out.contains(SMALL.rows[0]), "{out}");
    }

    /// The slot holds the letters in the middle: blank row top and bottom, and the spare
    /// columns split either side rather than left over on one edge.
    #[test]
    fn the_banner_is_centred_in_its_slot() {
        for (w, b) in [(48u16, &BIG), (24, &SMALL), (30, &SMALL)] {
            let (out, used) = render(w, 24);
            let lines: Vec<&str> = out.lines().collect();
            assert_eq!(used, slot(b), "width {w}");
            assert_eq!(lines[0].trim(), "", "no blank row above at width {w}:\n{out}");
            assert_eq!(lines[used as usize - 1].trim(), "", "none below at width {w}:\n{out}");

            for (i, row) in b.rows.iter().enumerate() {
                let line = lines[PAD_Y as usize + i];
                assert_eq!(line.trim(), row.trim(), "row {i} at width {w}");
            }
            // Margins either side of the letter block differ by at most the one column an
            // odd remainder cannot split.
            let left = lines[PAD_Y as usize].chars().take_while(|c| *c == ' ').count();
            let right = w as usize - left - b.width() as usize;
            assert!(right >= left && right - left <= 1, "{left}/{right} at width {w}");
        }
    }

    #[test]
    fn a_narrow_or_short_panel_falls_back_to_the_word() {
        // Too narrow for the small one, too short for either, too narrow for both.
        for (w, room) in [(18u16, 20u16), (48, 12), (12, 40)] {
            let (out, used) = render(w, room);
            assert_eq!(used, 1, "{w}x{room}:\n{out}");
            // Indented by one, to sit under the same margin as SPACES and AGENTS.
            assert!(out.starts_with(" horde"), "{w}x{room}:\n{out}");
        }
    }

    #[test]
    fn nothing_is_drawn_past_the_given_width() {
        // Including one column short of each banner, where it must not be clipped in half.
        for w in [1u16, 5, 18, 19, 24, 40, 41, 60] {
            let (out, _) = render(w, 24);
            for line in out.lines() {
                assert_eq!(line.chars().count(), w as usize, "width {w}: {line:?}");
            }
        }
    }
}
