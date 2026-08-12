//! Highlighting text in a pane with the mouse, and copying it on release.
//!
//! A terminal multiplexer draws its own grid, so the terminal underneath has no idea where one
//! pane ends and the next begins. Drag across a split in tmux without its mouse mode and you get
//! both panes' text welded together line by line. horde already owns every cell it draws, so it
//! can do the obvious thing instead: a selection belongs to one pane, and stops at its edge.
//!
//! Two decisions worth stating, because both could plausibly have gone the other way:
//!
//! - **Line-oriented, not rectangular.** Dragging from the middle of one line to the middle of
//!   another takes everything between them, the way selecting prose does. A block selection is
//!   occasionally what you want for columnar output, and never what you want for a stack trace.
//! - **Copy on release, with no key to press.** That is the behaviour being asked for, and the
//!   risk it carries is clobbering your clipboard by accident — so a click that does not move
//!   selects nothing and copies nothing, which is what an accidental drag mostly is.
//!
//! Coordinates here are always relative to a pane's *content* rect, so nothing in this file has
//! to know where on screen the pane sits.

use crate::proto::{PaneId, Row};

/// An in-progress or finished selection inside one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub pane: PaneId,
    /// Where the drag started. Stays put while the pointer moves.
    anchor: (u16, u16),
    /// Where the pointer is now. May be above or left of the anchor.
    head: (u16, u16),
    /// True while the button is still held.
    pub dragging: bool,
}

impl Selection {
    pub fn new(pane: PaneId, at: (u16, u16)) -> Selection {
        Selection { pane, anchor: at, head: at, dragging: true }
    }

    pub fn extend(&mut self, to: (u16, u16)) {
        self.head = to;
    }

    /// Start and end in reading order, so callers never deal with a backwards drag.
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (a, h) = (self.anchor, self.head);
        // Compare row first: a drag leftwards on a *later* line still ends later.
        if (a.1, a.0) <= (h.1, h.0) {
            (a, h)
        } else {
            (h, a)
        }
    }

    /// True when the pointer never left the cell it started in.
    ///
    /// A plain click focuses a pane, and must not also wipe the clipboard.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Whether a content-relative cell is inside the selection, line-wise.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        if self.is_empty() {
            return false;
        }
        let (start, end) = self.ordered();
        if y < start.1 || y > end.1 {
            return false;
        }
        let from = if y == start.1 { start.0 } else { 0 };
        let to = if y == end.1 { end.0 } else { u16::MAX };
        x >= from && x <= to
    }

    /// The selected text, one line per row, trailing blanks trimmed.
    ///
    /// Trimming matters more than it sounds: a terminal row is padded to the full pane width, so
    /// without it every copied line arrives with a tail of spaces.
    pub fn text(&self, rows: &[Row]) -> String {
        if self.is_empty() {
            return String::new();
        }
        let (start, end) = self.ordered();
        let mut out: Vec<String> = Vec::new();
        for y in start.1..=end.1 {
            let Some(row) = rows.get(y as usize) else { continue };
            let from = if y == start.1 { start.0 } else { 0 };
            let to = if y == end.1 { end.0 } else { u16::MAX };
            let line: String = columns(row)
                .filter(|(col, _)| *col >= from && *col <= to)
                .map(|(_, ch)| ch)
                .collect();
            out.push(line.trim_end().to_string());
        }
        out.join("\n")
    }
}

/// Walk a row's runs as `(column, char)`.
///
/// Runs are run-length encoded by style, so a column has to be counted rather than indexed — and
/// counted by display width, since one wide glyph covers two columns and a combining mark covers
/// none. Getting that wrong shifts every selection right of a CJK character or an emoji.
fn columns(row: &Row) -> impl Iterator<Item = (u16, char)> + '_ {
    use unicode_width::UnicodeWidthChar;
    let mut col: u16 = 0;
    row.runs.iter().flat_map(|r| r.text.chars()).map(move |ch| {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if w == 0 {
            // Belongs to the cell before it, so it travels with that cell's column.
            return (col.saturating_sub(1), ch);
        }
        let at = col;
        col += w;
        (at, ch)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Rgb, Run};

    fn row(text: &str) -> Row {
        Row {
            runs: vec![Run {
                text: text.into(),
                fg: Rgb::new(200, 200, 200),
                bg: Rgb::new(0, 0, 0),
                attrs: 0,
            }],
        }
    }

    /// Split across runs on purpose: styles change mid-line constantly, and a selection that
    /// only worked inside one run would break on any coloured output.
    fn styled_row(parts: &[&str]) -> Row {
        Row {
            runs: parts
                .iter()
                .map(|p| Run {
                    text: (*p).into(),
                    fg: Rgb::new(200, 200, 200),
                    bg: Rgb::new(0, 0, 0),
                    attrs: 0,
                })
                .collect(),
        }
    }

    fn drag(from: (u16, u16), to: (u16, u16)) -> Selection {
        let mut s = Selection::new(1, from);
        s.extend(to);
        s
    }

    #[test]
    fn a_click_that_does_not_move_selects_nothing() {
        let s = Selection::new(1, (4, 2));
        assert!(s.is_empty());
        assert_eq!(s.text(&[row("hello")]), "");
        assert!(!s.contains(4, 2), "and nothing is highlighted");
    }

    #[test]
    fn a_drag_within_one_line_takes_the_span() {
        let rows = vec![row("cargo test --quiet")];
        // "test"
        assert_eq!(drag((6, 0), (9, 0)).text(&rows), "test");
        // Backwards drags are the same span.
        assert_eq!(drag((9, 0), (6, 0)).text(&rows), "test");
    }

    #[test]
    fn a_drag_across_lines_takes_whole_lines_in_between() {
        let rows = vec![row("first line"), row("middle line"), row("last line")];
        let s = drag((6, 0), (4, 2));
        assert_eq!(s.text(&rows), "line\nmiddle line\nlast");
    }

    /// A backwards drag upwards is the same selection as the forwards one.
    #[test]
    fn dragging_up_the_screen_reads_the_same_as_dragging_down() {
        let rows = vec![row("alpha"), row("beta")];
        assert_eq!(drag((2, 0), (3, 1)).text(&rows), drag((3, 1), (2, 0)).text(&rows));
    }

    /// Terminal rows are padded to the pane width; without trimming every line would arrive
    /// with a tail of spaces.
    #[test]
    fn trailing_blanks_are_trimmed_off_each_line() {
        let rows = vec![row("short         "), row("also short    ")];
        assert_eq!(drag((0, 0), (13, 1)).text(&rows), "short\nalso short");
    }

    #[test]
    fn a_selection_spanning_style_changes_is_still_one_string() {
        let rows = vec![styled_row(&["error", ": ", "file not found"])];
        assert_eq!(drag((0, 0), (20, 0)).text(&rows), "error: file not found");
    }

    /// One wide glyph covers two columns. Counting it as one shifts everything after it.
    #[test]
    fn wide_glyphs_occupy_the_columns_they_are_drawn_in() {
        let rows = vec![row("日本 ok")];
        // Columns: 日=0,1  本=2,3  space=4  o=5  k=6
        assert_eq!(drag((5, 0), (6, 0)).text(&rows), "ok");
        assert_eq!(drag((0, 0), (3, 0)).text(&rows), "日本");
    }

    #[test]
    fn highlighting_covers_the_lines_between_the_ends() {
        let s = drag((6, 0), (4, 2));
        assert!(!s.contains(5, 0), "before the start on the first line");
        assert!(s.contains(6, 0));
        assert!(s.contains(0, 1), "a whole middle line");
        assert!(s.contains(99, 1));
        assert!(s.contains(4, 2));
        assert!(!s.contains(5, 2), "past the end on the last line");
        assert!(!s.contains(0, 3), "and nothing below it");
    }

    /// Rows come and go as output scrolls; asking for a line that is no longer there must not
    /// panic, it must just contribute nothing.
    #[test]
    fn a_selection_past_the_end_of_the_grid_is_harmless() {
        let rows = vec![row("only line")];
        assert_eq!(drag((0, 0), (4, 9)).text(&rows), "only line");
    }
}
