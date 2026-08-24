//! Blitting a pane's row cache into the ratatui buffer, plus its border and title.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::{color, rstyle};
use crate::proto::{attrs, AgentState, PaneInfo, Rgb, Row};
use crate::theme::Theme;

/// Draws one pane's cells. `rows` is the client's cache for that pane.
pub struct PaneView<'a> {
    pub rows: &'a [Row],
    pub theme: &'a Theme,
    /// A mouse highlight to paint over the cells, when one belongs to this pane.
    pub selection: Option<&'a crate::client::selection::Selection>,
}

impl Widget for PaneView<'_> {
    fn render(self, area: TRect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Paint the pane background first so short rows do not show the page through.
        let bg = Style::default().bg(color(self.theme.bg));
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                    cell.set_symbol(" ");
                    cell.set_style(bg);
                }
            }
        }

        self.blit(area, buf);
        self.highlight(area, buf);
    }
}

impl PaneView<'_> {
    /// Repaint the selected cells' background, leaving their text and colour alone.
    ///
    /// A separate pass over the area rather than a test inside the blit: that loop already has
    /// wide glyphs and combining marks to think about, and this needs nothing it computes.
    fn highlight(&self, area: TRect, buf: &mut Buffer) {
        let Some(sel) = self.selection else { return };
        let bg = color(self.theme.ui.selection);
        for y in 0..area.height {
            for x in 0..area.width {
                if sel.contains(x, y) {
                    if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                        cell.set_bg(bg);
                    }
                }
            }
        }
    }

    fn blit(&self, area: TRect, buf: &mut Buffer) {
        for (y, row) in self.rows.iter().enumerate().take(area.height as usize) {
            let mut x: u16 = 0;
            let py = area.y + y as u16;

            for run in &row.runs {
                let style = rstyle(run.fg, run.bg, run.attrs);
                for ch in run.text.chars() {
                    let w = crate::client::glyphs::width(ch);

                    // Zero-width marks (combining accents, variation selectors, skin-tone
                    // modifiers) belong to the cell before them, not a cell of their own.
                    if w == 0 {
                        if x > 0 {
                            if let Some(prev) = buf.cell_mut((area.x + x - 1, py)) {
                                let mut s = prev.symbol().to_string();
                                s.push(ch);
                                prev.set_symbol(&s);
                            }
                        }
                        continue;
                    }

                    if x >= area.width {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((area.x + x, py)) {
                        cell.set_char(ch);
                        cell.set_style(style);
                    }
                    // A double-width glyph owns the following cell; blanking it stops
                    // ratatui writing a stray character into the right half.
                    if w == 2 {
                        if x + 1 < area.width {
                            if let Some(next) = buf.cell_mut((area.x + x + 1, py)) {
                                next.set_symbol("");
                                next.set_style(style);
                            }
                        } else {
                            // No room for the second half; leave the cell blank rather
                            // than let the glyph spill past the pane edge.
                            if let Some(cell) = buf.cell_mut((area.x + x, py)) {
                                cell.set_symbol(" ");
                            }
                        }
                    }
                    x += w as u16;
                }
                if x >= area.width {
                    break;
                }
            }
        }
    }
}

/// Spinner frames for a working agent. Braille cycles smoothly and reads as motion even at
/// a low refresh rate.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn spinner_frame(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

/// A role's glyph and colour, declared or derived.
fn role_look(role: &str, roles: &[crate::config::Role], theme: &Theme) -> (String, Rgb) {
    crate::config::role_style(roles, role, theme)
}

/// Border and inline title for a pane.
///
/// The title lives in the top border rather than on its own row, which buys back a line of
/// terminal for every pane on screen.
#[allow(clippy::too_many_arguments)]
fn pane_frame(
    pane: &PaneInfo,
    focused: bool,
    zoomed: bool,
    theme: &Theme,
    tick: usize,
    animate: bool,
    accent: Rgb,
    roles: &[crate::config::Role],
) -> (Vec<Span<'static>>, Rgb) {
    // The focused pane keeps the one unmistakable border; project identity is what the
    // *others* now carry. Tinting the focused one too would make "which pane has the
    // keyboard" a question of hue, which is the one thing this border exists to answer.
    let border_color = if focused { theme.ui.border_focus } else { accent };
    let mut spans: Vec<Span<'static>> = Vec::new();

    spans.push(Span::styled(" ", Style::default().fg(color(border_color))));

    // State glyph, coloured by state. A working agent animates so motion, not just colour,
    // distinguishes it — useful when several panes are busy.
    if let Some(agent) = &pane.agent {
        let (glyph, c) = match agent.state {
            AgentState::Working => (
                if animate { spinner_frame(tick) } else { agent.state.glyph() },
                theme.ui.working,
            ),
            AgentState::Blocked => (agent.state.glyph(), theme.ui.blocked),
            AgentState::Done => (agent.state.glyph(), theme.ui.done),
            AgentState::Idle => (agent.state.glyph(), theme.ui.idle),
            AgentState::Unknown => (agent.state.glyph(), theme.ui.unknown),
            AgentState::Serving => (agent.state.glyph(), theme.ui.serving),
        };
        spans.push(Span::styled(glyph.to_string(), Style::default().fg(color(c))));
        spans.push(Span::raw(" "));
    }

    // What the pane is *for*, before what it is called. `reviewer` says more about why this
    // pane is on screen than `claude-3` does, and it is the same word in every project.
    if let Some(role) = &pane.role {
        let (glyph, c) = role_look(role, roles, theme);
        spans.push(Span::styled(format!("{glyph} {role} "), Style::default().fg(color(c))));
    }

    // Its own branch, but only for an agent horde put in a worktree. Every other pane is on
    // the project's branch, which the sidebar already says once for the whole project —
    // repeating it on six pane titles would be six copies of one fact, and would bury the
    // one case where the answer differs per pane.
    if let Some(r) = pane.repo.as_ref().filter(|r| r.worktree) {
        let dirty = if r.dirty { "*" } else { "" };
        spans.push(Span::styled(
            format!("⑂ {}{dirty} ", r.branch),
            Style::default().fg(color(theme.ui.accent_alt)),
        ));
    }

    let title_style = if focused {
        Style::default().fg(color(theme.ui.text)).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color(theme.ui.text_dim))
    };
    spans.push(Span::styled(pane.title.clone(), title_style));

    if let Some(agent) = &pane.agent {
        let detail = match agent.state {
            // Elapsed time matters while working; otherwise the label is more informative.
            AgentState::Working => format!(" {}", fmt_elapsed(agent.elapsed)),
            AgentState::Blocked => " needs you".to_string(),
            _ => format!(" {}", agent.state.label()),
        };
        let c = match agent.state {
            AgentState::Working => theme.ui.working,
            AgentState::Blocked => theme.ui.blocked,
            AgentState::Done => theme.ui.done,
            AgentState::Serving => theme.ui.serving,
            _ => theme.ui.text_faint,
        };
        spans.push(Span::styled(detail, Style::default().fg(color(c))));
    }

    if pane.exited {
        spans.push(Span::styled(
            " exited".to_string(),
            Style::default().fg(color(theme.ui.text_faint)),
        ));
    }
    if pane.scroll_offset > 0 {
        spans.push(Span::styled(
            format!(" ↑{}", pane.scroll_offset),
            Style::default().fg(color(theme.ui.accent_alt)),
        ));
    }
    if zoomed {
        spans.push(Span::styled(" zoom".to_string(), Style::default().fg(color(theme.ui.accent))));
    }

    spans.push(Span::styled(" ", Style::default().fg(color(border_color))));

    (spans, border_color)
}

// The frame is drawn with the one-eighth block elements rather than the box-drawing set,
// because those are the only rules that land on a cell *boundary*.
//
// A `─` is drawn through the middle of its cell. The cell is one row tall — 37 screen pixels
// on a retina display — and the rule is two of them, so whichever background that cell is
// given, half a row of it sits on the wrong side of the line: paint the frame in the
// terminal's colour and the terminal appears to run half a row past its own border; paint it
// in the chrome colour and the chrome appears to reach half a row inside. It is the same half
// cell either way, which is why no choice of colour ever settled it.
//
// `▔ ▁ ▏ ▕` sit flush against the edge of their cell. Painting the frame cell in the
// terminal's colour then puts the boundary between terminal and chrome exactly on the rule,
// because the rule *is* the cell edge. The error is not reduced, it is zero.
//
// The cost is square corners: rounded ones only exist in the box-drawing set, which is
// mid-cell by construction. A corner that is a pixel blunter is worth an edge that is exact.
const RULE_TOP: &str = "▔";
const RULE_BOTTOM: &str = "▁";
const RULE_LEFT: &str = "▏";
const RULE_RIGHT: &str = "▕";

/// Paint a pane's frame and its title into `area`.
#[allow(clippy::too_many_arguments)]
pub fn draw_frame(
    pane: &PaneInfo,
    focused: bool,
    zoomed: bool,
    theme: &Theme,
    tick: usize,
    animate: bool,
    accent: Rgb,
    roles: &[crate::config::Role],
    area: TRect,
    buf: &mut Buffer,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let (spans, border_color) =
        pane_frame(pane, focused, zoomed, theme, tick, animate, accent, roles);

    // The whole cell in the terminal's colour first: the frame belongs to the pane, and the
    // rules are drawn on top of it at the outer edge.
    let style = Style::default().fg(color(border_color)).bg(color(theme.bg));
    super::fill(buf, area, theme.bg);

    let (x0, y0) = (area.x, area.y);
    let (x1, y1) = (area.x + area.width - 1, area.y + area.height - 1);
    for x in x0..=x1 {
        if let Some(c) = buf.cell_mut((x, y0)) {
            c.set_symbol(RULE_TOP);
            c.set_style(style);
        }
        if let Some(c) = buf.cell_mut((x, y1)) {
            c.set_symbol(RULE_BOTTOM);
            c.set_style(style);
        }
    }
    // The sides stop short of the rows the top and bottom rules already span, so the corners
    // are owned by one rule rather than fought over by two.
    for y in (y0 + 1)..y1 {
        if let Some(c) = buf.cell_mut((x0, y)) {
            c.set_symbol(RULE_LEFT);
            c.set_style(style);
        }
        if let Some(c) = buf.cell_mut((x1, y)) {
            c.set_symbol(RULE_RIGHT);
            c.set_style(style);
        }
    }

    // The title sits in the top row, notched into the rule the way it always has.
    let inset = 2u16.min(area.width);
    super::put_line(buf, x0 + inset, y0, area.width.saturating_sub(inset), Line::from(spans));
}

/// `45s`, `2m18s`, `1h04m`.
pub fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// True when a style has a given attribute bit. Kept here so the attribute bit layout has
/// one reader.
pub fn has_attr(a: u8, bit: u8) -> bool {
    a & bit != 0
}

pub fn modifiers(a: u8) -> Modifier {
    let mut m = Modifier::empty();
    if has_attr(a, attrs::BOLD) {
        m |= Modifier::BOLD;
    }
    if has_attr(a, attrs::DIM) {
        m |= Modifier::DIM;
    }
    if has_attr(a, attrs::ITALIC) {
        m |= Modifier::ITALIC;
    }
    if has_attr(a, attrs::UNDERLINE) {
        m |= Modifier::UNDERLINED;
    }
    if has_attr(a, attrs::STRIKEOUT) {
        m |= Modifier::CROSSED_OUT;
    }
    if has_attr(a, attrs::HIDDEN) {
        m |= Modifier::HIDDEN;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Run;

    fn row(text: &str) -> Row {
        Row {
            runs: vec![Run {
                text: text.to_string(),
                fg: Rgb::new(255, 255, 255),
                bg: Rgb::new(0, 0, 0),
                attrs: 0,
            }],
        }
    }

    fn render(rows: &[Row], w: u16, h: u16) -> Buffer {
        let area = TRect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        PaneView { rows, theme: &theme, selection: None }.render(area, &mut buf);
        buf
    }

    fn line_of(buf: &Buffer, y: u16, w: u16) -> String {
        (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect()
    }

    /// The highlight has to land on exactly the selected cells, and change only their
    /// background — a selection that repainted the text would make it unreadable.
    #[test]
    fn a_selection_paints_the_cells_it_covers_and_no_others() {
        use crate::client::selection::Selection;
        let area = TRect::new(0, 0, 12, 2);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        let rows = [row("cargo test"), row("ok")];

        let mut sel = Selection::new(1, (6, 0));
        sel.extend((9, 0)); // "test"
        PaneView { rows: &rows, theme: &theme, selection: Some(&sel) }.render(area, &mut buf);

        let want = color(theme.ui.selection);
        for x in 0..12u16 {
            let cell = buf.cell((x, 0)).unwrap();
            let selected = (6..=9).contains(&x);
            assert_eq!(cell.bg == want, selected, "column {x} background");
        }
        // The text itself is untouched, and the row below is not involved.
        assert_eq!(line_of(&buf, 0, 12), "cargo test  ");
        assert_ne!(buf.cell((0, 1)).unwrap().bg, want, "an unselected row stays as it was");
    }

    #[test]
    fn renders_plain_ascii() {
        let buf = render(&[row("hello")], 10, 1);
        assert_eq!(line_of(&buf, 0, 10), "hello     ");
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        // CJK is double width; the second cell must be blanked, not filled with the glyph.
        let buf = render(&[row("日本")], 6, 1);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "日");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "本");
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), "");
    }

    #[test]
    fn combining_marks_attach_to_the_preceding_cell() {
        // A true zero-width mark (combining acute) must not consume a cell of its own.
        let buf = render(&[row("e\u{0301}x")], 6, 1);
        let first = buf.cell((0, 0)).unwrap().symbol();
        assert_eq!(first, "e\u{0301}", "the accent should join its base letter");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "x");
    }

    #[test]
    fn emoji_with_a_modifier_occupies_whatever_the_emulator_assigned() {
        // `unicode-width` reports skin-tone modifiers (U+1F3FB..U+1F3FF) as width 2 rather
        // than zero, so 👍🏽 spans four columns rather than two. That looks wrong in
        // isolation, but the mirror deliberately reproduces exactly what the emulator laid
        // out — diverging here would put the rendered cursor in the wrong column.
        //
        // The invariants that actually matter are: no panic, and no overflow.
        let buf = render(&[row("👍🏽x")], 8, 1);
        let rendered: String = (0..8).map(|x| buf.cell((x, 0)).unwrap().symbol()).collect();
        assert!(rendered.contains('👍'));
        assert!(rendered.contains('x'));
        assert_eq!(
            (0..8).filter(|&x| buf.cell((x, 0)).unwrap().symbol() == "").count(),
            2,
            "each double-width glyph should blank exactly one trailing cell"
        );
    }

    #[test]
    fn content_is_clipped_to_the_area() {
        let buf = render(&[row("abcdefghij")], 4, 1);
        assert_eq!(line_of(&buf, 0, 4), "abcd");
    }

    #[test]
    fn a_wide_char_straddling_the_edge_is_blanked_not_spilled() {
        // Only one column left, but the glyph needs two.
        let buf = render(&[row("a日")], 2, 1);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), " ", "half a glyph must not render");
    }

    #[test]
    fn rows_beyond_the_area_are_ignored() {
        let buf = render(&[row("one"), row("two"), row("three")], 5, 2);
        assert_eq!(line_of(&buf, 0, 5), "one  ");
        assert_eq!(line_of(&buf, 1, 5), "two  ");
    }

    #[test]
    fn short_rows_are_padded_with_the_theme_background() {
        let buf = render(&[row("hi")], 5, 1);
        let cell = buf.cell((4, 0)).unwrap();
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.style().bg, Some(color(Theme::horde().bg)));
    }

    #[test]
    fn zero_size_area_is_a_noop() {
        let area = TRect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(TRect::new(0, 0, 4, 1));
        let theme = Theme::horde();
        PaneView { rows: &[row("x")], theme: &theme, selection: None }
            .render(area, &mut buf);
        assert_eq!(line_of(&buf, 0, 4), "    ");
    }

    #[test]
    fn elapsed_formats_across_magnitudes() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(45), "45s");
        assert_eq!(fmt_elapsed(60), "1m00s");
        assert_eq!(fmt_elapsed(138), "2m18s");
        assert_eq!(fmt_elapsed(3600), "1h00m");
        assert_eq!(fmt_elapsed(3900), "1h05m");
    }

    #[test]
    fn spinner_cycles_without_panicking_on_large_ticks() {
        assert_eq!(spinner_frame(0), SPINNER[0]);
        assert_eq!(spinner_frame(SPINNER.len()), SPINNER[0]);
        assert_eq!(spinner_frame(usize::MAX), SPINNER[usize::MAX % SPINNER.len()]);
    }

    #[test]
    fn attribute_bits_map_to_modifiers() {
        assert!(modifiers(attrs::BOLD).contains(Modifier::BOLD));
        assert!(modifiers(attrs::UNDERLINE).contains(Modifier::UNDERLINED));
        let both = modifiers(attrs::BOLD | attrs::ITALIC);
        assert!(both.contains(Modifier::BOLD) && both.contains(Modifier::ITALIC));
        assert!(modifiers(0).is_empty());
    }
}
