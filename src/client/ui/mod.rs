//! Frame composition and shared drawing helpers.

pub mod bus_drawer;
pub mod logo;
pub mod overlays;
pub mod pane_widget;
pub mod sidebar;
pub mod statusbar;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::Frame;

use super::{App, Mode};
use crate::proto::{AgentState, Rect, Rgb};
use crate::theme::{mix, Theme};

pub fn color(c: Rgb) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Display columns a string occupies, which is not its character count once a glyph is wide.
pub fn width(s: &str) -> usize {
    s.chars().map(|c| crate::client::glyphs::width(c)).sum()
}

/// The glyph and colour an agent state is drawn with.
///
/// Shared rather than sidebar-local because more than one view now draws agent states, and
/// two copies of this would drift the moment one gained a state the other did not.
pub fn state_look(state: AgentState, t: &Theme, tick: usize, animate: bool) -> (String, Rgb) {
    let glyph = match state {
        AgentState::Working if animate => pane_widget::spinner_frame(tick).to_string(),
        _ => state.glyph().to_string(),
    };
    let c = match state {
        AgentState::Working => t.ui.working,
        AgentState::Blocked => t.ui.blocked,
        AgentState::Done => t.ui.done,
        AgentState::Idle => t.ui.idle,
        AgentState::Unknown => t.ui.unknown,
        AgentState::Serving => t.ui.serving,
    };
    (glyph, c)
}

/// Style for a terminal cell run.
pub fn rstyle(fg: Rgb, bg: Rgb, attrs: u8) -> Style {
    Style::default().fg(color(fg)).bg(color(bg)).add_modifier(pane_widget::modifiers(attrs))
}

pub fn trect(r: Rect) -> TRect {
    TRect::new(r.x, r.y, r.w, r.h)
}

pub fn fill(buf: &mut Buffer, area: TRect, bg: Rgb) {
    let style = Style::default().bg(color(bg));
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_symbol(" ");
                c.set_style(style);
            }
        }
    }
}

/// Write a line clipped to `w` columns, honouring double-width glyphs.
pub fn put_line(buf: &mut Buffer, x: u16, y: u16, w: u16, line: Line<'_>) {
    let mut cx = x;
    let end = x.saturating_add(w);
    for span in line.spans {
        for ch in span.content.chars() {
            let cw = crate::client::glyphs::width(ch);
            if cw == 0 {
                continue;
            }
            // Stop rather than clip a wide glyph in half.
            if cx + cw as u16 > end {
                return;
            }
            if let Some(cell) = buf.cell_mut((cx, y)) {
                cell.set_char(ch);
                cell.set_style(span.style);
            }
            if cw == 2 {
                if let Some(next) = buf.cell_mut((cx + 1, y)) {
                    next.set_symbol("");
                    next.set_style(span.style);
                }
            }
            cx += cw as u16;
        }
    }
}

/// Shorten to `w` display columns, ending in an ellipsis when it does not fit.
pub fn truncate(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    let width: usize = s.chars().map(|c| crate::client::glyphs::width(c)).sum();
    if width <= w {
        return s.to_string();
    }
    if w == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = crate::client::glyphs::width(ch);
        if used + cw > w - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Greedy word wrap on display width. Words longer than the line are hard-split rather than
/// allowed to overflow.
pub fn wrap_text(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        for word in para.split_whitespace() {
            let ww: usize = word.chars().map(|c| crate::client::glyphs::width(c)).sum();
            if ww > w {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                // Hard-split an over-long word across lines.
                let mut chunk = String::new();
                let mut cused = 0;
                for ch in word.chars() {
                    let cw = crate::client::glyphs::width(ch);
                    if cused + cw > w {
                        out.push(std::mem::take(&mut chunk));
                        cused = 0;
                    }
                    chunk.push(ch);
                    cused += cw;
                }
                if !chunk.is_empty() {
                    line = chunk;
                    used = cused;
                }
                continue;
            }
            let need = if line.is_empty() { ww } else { ww + 1 };
            if used + need > w {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += ww;
        }
        out.push(line);
    }
    // A trailing empty line from a final newline is noise.
    while out.last().is_some_and(|l| l.is_empty()) && out.len() > 1 {
        out.pop();
    }
    out
}

/// Push every cell toward the background, so an overlay reads as being in front.
pub fn dim_area(buf: &mut Buffer, area: TRect, theme: &Theme, amount: f32) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            let Some(cell) = buf.cell_mut((x, y)) else { continue };
            let st = cell.style();
            let f = |c: Option<Color>, fallback: Rgb| -> Color {
                match c {
                    Some(Color::Rgb(r, g, b)) => {
                        color(mix(Rgb::new(r, g, b), theme.ui.bg, amount))
                    }
                    _ => color(mix(fallback, theme.ui.bg, amount)),
                }
            };
            let fg = f(st.fg, theme.ui.text);
            let bg = f(st.bg, theme.ui.bg);
            cell.set_style(Style::default().fg(fg).bg(bg));
        }
    }
}

/// Centre a `w` x `h` box inside `area`, clamped to fit.
pub fn centered(area: TRect, w: u16, h: u16) -> TRect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    TRect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w,
        h,
    )
}

/// Draw one frame.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme = app.cfg.theme.clone();
    fill(f.buffer_mut(), area, theme.ui.bg);

    let Some(snap) = app.snapshot.clone() else {
        // Before the first snapshot arrives there is nothing to lay out.
        let msg = Line::from("connecting to horde daemon…");
        put_line(f.buffer_mut(), area.x + 1, area.y + area.height / 2, area.width, msg);
        return;
    };

    // Panes
    let mut cursor_at: Option<(u16, u16)> = None;
    for pane in &snap.panes {
        if pane.cell.is_empty() {
            continue; // not on screen
        }
        let focused = snap.focused_pane == Some(pane.id);
        let zoomed = snap.view.zoom == Some(pane.id);

        // Whether a frame is drawn is read off the geometry the daemon sent, not off the
        // client's own `pane_titles`. Both processes load the same config file, but they load
        // it separately and can disagree — a reload one of them rejected, a setting changed in
        // one and not yet the other. When they do, the daemon hands over a content rect that is
        // the whole cell while the client still draws a border, and since the content is
        // painted after the frame the terminal goes straight over the border it is meant to sit
        // inside. Asking the rects removes the disagreement: the ring exists exactly when the
        // daemon reserved one.
        let framed = pane.content != pane.cell;
        if framed {
            // The space's colour is resolved here rather than inside the widget: a pane knows
            // which space it is in, but only the snapshot knows that space's slot.
            let accent = snap
                .spaces
                .iter()
                .find(|s| s.id == pane.space)
                .map(|s| theme.space_accent(s.accent))
                .unwrap_or(theme.ui.border);
            pane_widget::draw_frame(
                pane,
                focused,
                zoomed,
                &theme,
                app.tick,
                app.cfg.animate,
                accent,
                &app.cfg.roles,
                trect(pane.cell),
                f.buffer_mut(),
            );
        }

        let empty: Vec<crate::proto::Row> = Vec::new();
        let rows = app.rows.get(&pane.id).unwrap_or(&empty);
        // Only the pane the highlight belongs to gets one; a selection is never shared.
        let sel = app.selection.as_ref().filter(|s| s.pane == pane.id);
        pane_widget::PaneView { rows, theme: &theme, selection: sel }
            .render(trect(pane.content), f.buffer_mut());

        if focused {
            if let Some(c) = app.cursors.get(&pane.id) {
                if c.visible && c.x < pane.content.w && c.y < pane.content.h {
                    cursor_at = Some((pane.content.x + c.x, pane.content.y + c.y));
                }
            }
        }
    }

    // Panels
    if !snap.sidebar.is_empty() {
        // Both of these are borrowed mutably by the widget while `app` still is, so they are
        // lifted out and put back — the same trick the hit list has always used.
        let mut hits = std::mem::take(&mut app.sidebar_hits);
        let mut state = std::mem::take(&mut app.sidebar);
        let focused = matches!(app.mode, Mode::Sidebar);
        sidebar::Sidebar {
            snap: &snap,
            board: Some((snap.tasks_open, snap.tasks_claimed)),
            theme: &theme,
            tick: app.tick,
            animate: app.cfg.animate,
            hits: &mut hits,
            state: &mut state,
            focused,
        }
        .render(trect(snap.sidebar), f.buffer_mut());
        app.sidebar_hits = hits;
        app.sidebar = state;
    }
    if !snap.bus.is_empty() {
        bus_drawer::BusDrawer { messages: &app.bus, theme: &theme, now: now_millis() }
            .render(trect(snap.bus), f.buffer_mut());
    }
    if !snap.tabbar.is_empty() {
        statusbar::TabBar { snap: &snap, theme: &theme }
            .render(trect(snap.tabbar), f.buffer_mut());
    }
    if !snap.status.is_empty() {
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
    }

    // Overlays sit in front of everything, over a dimmed frame.
    match &app.mode {
        Mode::Terminal => {}
        Mode::Prefix => {}
        // The panel is already on screen and already drew its own cursor; there is no overlay
        // to put in front of it.
        Mode::Sidebar => {}
        Mode::Roster { .. } => {
            dim_area(f.buffer_mut(), area, &theme, 0.6);
            overlays::roster(f, area, app);
        }
        Mode::Help => {
            dim_area(f.buffer_mut(), area, &theme, 0.6);
            overlays::help(f, area, app);
        }
        Mode::Palette { .. } | Mode::SpaceSwitcher { .. } => {
            dim_area(f.buffer_mut(), area, &theme, 0.6);
            overlays::picker(f, area, app);
        }
        Mode::Prompt { .. } => {
            dim_area(f.buffer_mut(), area, &theme, 0.5);
            overlays::prompt(f, area, app);
        }
        Mode::Settings { .. } => {
            dim_area(f.buffer_mut(), area, &theme, 0.6);
            overlays::settings(f, area, app);
        }
        Mode::Digest { .. } => {
            dim_area(f.buffer_mut(), area, &theme, 0.6);
            overlays::digest(f, area, app);
        }
        Mode::Approvals { .. } => {
            dim_area(f.buffer_mut(), area, &theme, 0.6);
            overlays::approvals(f, area, app);
        }
        Mode::Menu { .. } => {
            // A menu is a light touch on top of the session, not a modal takeover.
            dim_area(f.buffer_mut(), area, &theme, 0.25);
            overlays::menu(f, area, app);
        }
    }

    overlays::toasts(f, area, app);

    // Only show a cursor when keystrokes actually reach a pane.
    match (cursor_at, &app.mode) {
        (Some((x, y)), Mode::Terminal) => f.set_cursor_position((x, y)),
        _ => {}
    }
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
        // A double-width glyph counts as two columns.
        assert_eq!(truncate("日本語", 4), "日…");
    }

    #[test]
    fn wrap_breaks_on_words() {
        assert_eq!(wrap_text("one two three", 9), vec!["one two", "three"]);
        assert_eq!(wrap_text("short", 20), vec!["short"]);
    }

    #[test]
    fn wrap_hard_splits_words_longer_than_the_line() {
        let out = wrap_text("supercalifragilistic", 6);
        assert!(out.iter().all(|l| l.chars().count() <= 6), "{out:?}");
        assert_eq!(out.concat(), "supercalifragilistic");
    }

    #[test]
    fn wrap_never_exceeds_the_width() {
        let text = "the quick brown fox jumps over the lazy dog and keeps on running";
        for w in 4..40 {
            for line in wrap_text(text, w) {
                let width: usize =
                    line.chars().map(|c| crate::client::glyphs::width(c)).sum();
                assert!(width <= w, "width {w}: {line:?}");
            }
        }
    }

    #[test]
    fn wrap_of_zero_width_is_empty_rather_than_looping() {
        assert!(wrap_text("anything", 0).is_empty());
    }

    #[test]
    fn wrap_handles_embedded_newlines() {
        assert_eq!(wrap_text("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn centered_clamps_to_the_available_area() {
        let area = TRect::new(0, 0, 100, 40);
        let c = centered(area, 40, 10);
        assert_eq!((c.x, c.y, c.width, c.height), (30, 15, 40, 10));

        // An oversized box is clamped instead of overflowing.
        let c = centered(area, 500, 500);
        assert_eq!((c.x, c.y, c.width, c.height), (0, 0, 100, 40));
    }

    #[test]
    fn put_line_stops_before_splitting_a_wide_glyph() {
        let area = TRect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        put_line(&mut buf, 0, 0, 3, Line::from("a日本"));
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "日");
        // Column 2 is the second half of 日; 本 would not fit and must be dropped.
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "");
    }

    #[test]
    fn dim_area_moves_colors_toward_the_background() {
        let area = TRect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        buf.cell_mut((0, 0))
            .unwrap()
            .set_style(Style::default().fg(Color::Rgb(255, 255, 255)));
        dim_area(&mut buf, area, &theme, 1.0);
        // Fully dimmed means fully background.
        assert_eq!(buf.cell((0, 0)).unwrap().style().fg, Some(color(theme.ui.bg)));
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    use crate::client::App;
    use crate::proto::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Build a realistic session: two spaces, three panes, mixed agent states.
    fn demo() -> (App, Snapshot) {
        let cfg = crate::config::Config::default();
        let mut app = App::new_for_test(cfg);

        let mk_pane = |id: u32, title: &str, cell: Rect, agent: Option<(&str, AgentState, u64)>| {
            PaneInfo {
                id, tab: 1, space: 1,
                title: title.into(),
                cwd: "/Users/josh/Documents/dev/horde".into(),
                cell, content: cell.inset(1),
                cols: cell.inset(1).w, rows: cell.inset(1).h,
                agent: agent.map(|(n, s, e)| AgentInfo {
                    kind: "claude".into(), name: n.into(), class: Default::default(),
                    state: s, elapsed: e,
                    authority: "hook".into(), reason: "reported by integration".into(),
                    // The demo frame shows the activity line the hooks make possible.
                    activity: crate::proto::Activity {
                        tools: 12, files: 3, errors: 0, turns: 2,
                        last_tool: Some("Edit".into()),
                    },
                    question: None,
                }),                spawned_by: None,
                exited: false, scroll_offset: 0, wants_mouse: false, bracketed_paste: true, role: None, pinned: false, board: false,
                repo: None,
            }
        };

        let panes = vec![
            mk_pane(1, "builder", Rect::new(24, 1, 46, 25), Some(("builder", AgentState::Working, 138))),
            mk_pane(2, "reviewer", Rect::new(70, 1, 46, 25), Some(("reviewer", AgentState::Blocked, 12))),
            mk_pane(3, "tests", Rect::new(24, 26, 92, 12), None),
        ];

        let snap = Snapshot {
            protocol: 1,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            spaces: vec![
                SpaceInfo { id: 1, name: "api-refactor".into(), cwd: "/x".into(),
                    tabs: vec![1], focused_tab: Some(1), agent_count: 2, attention_count: 1, accent: 0, collapsed: false, repo: None },
                SpaceInfo { id: 2, name: "docs".into(), cwd: "/y".into(),
                    tabs: vec![2], focused_tab: Some(2), agent_count: 1, attention_count: 0, accent: 1, collapsed: false, repo: None },
            ],
            tabs: vec![
                TabInfo { id: 1, space: 1, name: "agents".into(), panes: vec![1,2,3], focused_pane: Some(1) },
                TabInfo { id: 2, space: 1, name: "logs".into(), panes: vec![], focused_pane: None },
            ],
            panes,
            focused_space: Some(1), focused_tab: Some(1), focused_pane: Some(1),
            view: ViewState { sidebar_open: true, bus_open: true, sidebar_width: 24, bus_width: 30, zoom: None },
            sidebar: Rect::new(0, 1, 24, 37),
            bus: Rect::new(116, 1, 30, 37),
            status: Rect::new(0, 38, 146, 1),
            tabbar: Rect::new(0, 0, 146, 1),
            tasks_open: 2,
            tasks_claimed: 1,
            triggers_armed: 0,
        };

        app.rows.insert(1, vec![
            row("> applying the migration…"), row("  003_users.sql"), row(""),
            row("  3 files changed"),
        ]);
        app.rows.insert(2, vec![
            row("Do you want to make this edit"), row("to src/mux.rs?"), row(""),
            row("  ❯ 1. Yes"), row("    2. No"),
        ]);
        app.rows.insert(3, vec![row("PASS  42   FAIL  0"), row("$ ")]);

        app.bus = vec![
            // A request and its reply, so the demo frame shows the threading.
            Message { id: 1, ts: 1_000_000, from: "builder".into(), to: "reviewer".into(),
                body: "is the gating logic sound?".into(),
                delivery: Delivery::Delivered, broadcast: false,
                expects_reply: true, reply_to: None },
            Message { id: 2, ts: 1_000_000, from: "reviewer".into(), to: "builder".into(),
                body: "yes, it holds".into(), delivery: Delivery::Queued,
                broadcast: false, expects_reply: false, reply_to: Some(1) },
        ];
        (app, snap)
    }

    fn row(s: &str) -> Row {
        let t = Theme::horde();
        Row { runs: vec![Run { text: s.into(), fg: t.fg, bg: t.bg, attrs: 0 }] }
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h).map(|y| {
            (0..w).map(|x| {
                let s = buf.cell((x, y)).unwrap().symbol();
                if s.is_empty() { " " } else { s }
            }).collect::<String>().trim_end().to_string()
        }).collect::<Vec<_>>().join("\n")
    }

    /// A layout that moved its edges has to force a full repaint.
    ///
    /// The client sends the host only the cells that changed since its last frame. When the
    /// lines move — a pane closes, the bus opens, a tab with a different split comes forward —
    /// the old borders sit in cells the new layout never writes to, and stay there until
    /// something happens to overwrite that exact cell. That is the debris below a pane that
    /// only a redraw clears.
    #[test]
    fn a_layout_that_moved_its_edges_is_recognised_as_a_new_shape() {
        let (_app, base) = demo();

        let same = base.clone();
        assert!(!crate::client::shape_changed(&base, &same), "an identical layout needs no repaint");

        // Text changing is the normal case and must not cost a full repaint.
        let mut renamed = base.clone();
        renamed.panes[0].title = "something else".into();
        assert!(!crate::client::shape_changed(&base, &renamed), "a title is not a shape");

        let mut moved = base.clone();
        moved.panes[0].cell.w -= 1;
        assert!(crate::client::shape_changed(&base, &moved), "a pane that changed width moved its border");

        let mut panel = base.clone();
        panel.bus = crate::proto::Rect::new(0, 0, 0, 0);
        assert!(crate::client::shape_changed(&base, &panel), "the bus closing moved an edge");

        let mut fewer = base.clone();
        fewer.panes.pop();
        assert!(crate::client::shape_changed(&base, &fewer), "a pane closing left its border behind");
    }

    /// The boundary between the terminal and the chrome around it has to fall exactly on the
    /// rule, and the only way to get that is a rule that sits on a cell boundary.
    ///
    /// A `─` is drawn through the middle of its cell, so half a row of that cell's background
    /// lands on the wrong side of the line whichever colour it is given: paint the frame in the
    /// terminal's colour and the terminal runs half a row past its border, paint it in the
    /// chrome colour and the chrome reaches half a row inside it. Reported first as the
    /// terminal bleeding out, then — with the colour swapped — as the chrome bleeding in. The
    /// one-eighth blocks are flush with the edge of their cell, so with the frame painted in
    /// the terminal's colour the error is not smaller, it is zero.
    #[test]
    fn the_rule_around_a_pane_sits_on_the_cell_boundary() {
        let (mut app, snap) = demo();
        let theme = app.cfg.theme.clone();
        let pane = snap.panes[0].clone();
        app.rows.insert(pane.id, vec![row("x"); pane.content.h as usize]);
        app.snapshot = Some(snap);

        let mut term = Terminal::new(TestBackend::new(146, 39)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();

        let terminal = color(theme.bg);
        let bottom = pane.cell.y + pane.cell.h - 1;
        let right = pane.cell.x + pane.cell.w - 1;

        // Every frame cell carries the terminal's own background, so there is no colour
        // boundary anywhere inside the rule for the eye to catch.
        for x in pane.cell.x..=right {
            for y in [pane.cell.y, bottom] {
                assert_eq!(
                    buf.cell((x, y)).unwrap().style().bg,
                    Some(terminal),
                    "frame cell ({x},{y}) is not the terminal's colour"
                );
            }
        }

        // And the rules are the edge-flush glyphs, not the mid-cell box-drawing ones. Checked
        // away from the corners and the title, which the rule legitimately breaks for.
        let mid_x = pane.cell.x + pane.cell.w / 2;
        let mid_y = pane.cell.y + pane.cell.h / 2;
        assert_eq!(buf.cell((mid_x, pane.cell.y)).unwrap().symbol(), "▔", "top rule");
        assert_eq!(buf.cell((mid_x, bottom)).unwrap().symbol(), "▁", "bottom rule");
        assert_eq!(buf.cell((pane.cell.x, mid_y)).unwrap().symbol(), "▏", "left rule");
        assert_eq!(buf.cell((right, mid_y)).unwrap().symbol(), "▕", "right rule");
    }

    #[test]
    fn full_frame_renders() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        let out = render(&mut app, 146, 39);
        println!("\n{out}\n");
        assert!(out.contains("horde"));
        assert!(out.contains("api-refactor"));
        assert!(out.contains("builder"));
        assert!(out.contains("needs you"));
        assert!(out.contains("bus"));
    }

    /// The frame and the content rect have to come from the same answer.
    ///
    /// The client and the daemon load the same config file but load it separately, and they
    /// can disagree — one of them rejected a reload, or a setting changed in one and not the
    /// other. When the daemon insets a pane's content to leave room for a border and the client
    /// has decided not to draw one, nothing paints that ring: the page shows through it, a
    /// one-cell frame of container colour around every pane, heaviest to notice along the
    /// bottom edge. Reading the answer off the rects means the ring is drawn exactly when the
    /// daemon reserved one.
    #[test]
    fn a_pane_whose_content_was_inset_is_framed_whatever_the_client_config_says() {
        let (mut app, mut snap) = demo();
        // The daemon reserved the ring; this client's own config says no titles.
        app.cfg.pane_titles = false;
        let cell = snap.panes[0].cell;
        snap.panes[0].content = cell.inset(1);
        let id = snap.panes[0].id;
        app.rows.insert(id, vec![row(&"#".repeat(cell.w as usize)); cell.h as usize]);
        app.snapshot = Some(snap);

        let out = render(&mut app, 146, 39);
        let lines: Vec<&str> = out.lines().collect();
        let top = lines[cell.y as usize].chars().nth(cell.x as usize).unwrap_or(' ');
        let bottom = lines[(cell.y + cell.h - 1) as usize]
            .chars()
            .nth(cell.x as usize)
            .unwrap_or(' ');
        assert_eq!(top, '▔', "no rule was drawn over the row the daemon reserved for one");
        assert_eq!(bottom, '▁', "the bottom of the ring is bare container");
    }

    #[test]
    fn help_overlay_renders() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        app.mode = crate::client::Mode::Help;
        let out = render(&mut app, 146, 39);
        println!("\n{out}\n");
        assert!(out.contains("keys"));
    }

    #[test]
    fn roster_overlay_renders() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        app.mode = crate::client::Mode::Roster { scroll: 0 };
        let out = render(&mut app, 146, 39);
        println!("\n{out}\n");
        assert!(out.contains("roster"), "{out}");
        assert!(out.contains("enter jumps"), "the footer hint must survive: {out}");
    }

    /// The roster is a whole-frame view, so every landable line must record where it was
    /// drawn or the mouse could not reach any of them.
    #[test]
    fn the_roster_records_a_hit_box_for_every_row_it_draws() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        app.mode = crate::client::Mode::Roster { scroll: 0 };
        let _ = render(&mut app, 146, 39);
        assert!(!app.roster_hits.is_empty());
        assert!(app.roster_hits.iter().all(|(_, _, w, _)| *w > 0));
    }

    /// A terminal too narrow for two cards gets one column rather than two unreadable ones.
    #[test]
    fn a_narrow_terminal_still_renders_the_roster() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        app.mode = crate::client::Mode::Roster { scroll: 0 };
        let out = render(&mut app, 60, 20);
        // Not every frame line is padded to full width — the tab bar never has been — so the
        // invariant worth asserting is that nothing *exceeds* it.
        for line in out.lines() {
            assert!(line.chars().count() <= 60, "{line:?}");
        }
        assert!(out.contains("roster"), "{out}");
        assert!(out.contains("api-refactor"), "one column, but still readable:\n{out}");
    }

    #[test]
    fn settings_panel_renders() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        crate::client::open_settings(&mut app, 0);
        let out = render(&mut app, 146, 39);
        println!("\n{out}\n");
        assert!(out.contains("settings"));
        assert!(out.contains("Theme"));
        assert!(out.contains("Sidebar width"));
        assert!(out.contains("Edit config.toml"));
        assert!(out.contains("change"));
    }

    #[test]
    fn keybindings_settings_page_renders() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        let cat = crate::client::settings::Category::all()
            .iter()
            .position(|c| *c == crate::client::settings::Category::Keys)
            .unwrap();
        crate::client::open_settings(&mut app, cat);
        let out = render(&mut app, 146, 39);
        println!("\n{out}\n");
        assert!(out.contains("Keybindings"));
        assert!(out.contains("ctrl+b"));
        assert!(out.contains("rebinds"));
    }

    #[test]
    fn long_settings_list_scrolls_to_keep_the_selection_visible() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        let cat = crate::client::settings::Category::all()
            .iter()
            .position(|c| *c == crate::client::settings::Category::Keys)
            .unwrap();
        crate::client::open_settings(&mut app, cat);

        // Jump the selection to the very last rebindable action.
        let rows = crate::client::settings::rows(&app.cfg, crate::client::settings::Category::Keys);
        let last = rows.iter().enumerate().filter(|(_, r)| r.selectable()).last().unwrap().0;
        app.mode = crate::client::Mode::Settings { cat, sel: last, capture: None };

        let out = render(&mut app, 146, 30);
        println!("\n{out}\n");
        let label = rows[last].label.clone();
        assert!(out.contains(&label), "selection {label:?} must be scrolled into view:\n{out}");
        // And the scroll position is reported rather than left implicit.
        assert!(out.contains('%'), "{out}");
    }

    #[test]
    fn context_menu_renders_over_a_pane() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap.clone());
        let level = crate::client::menu::build(
            crate::client::menu::Target::Pane(1),
            &snap,
            "ctrl+b",
        );
        app.mode = crate::client::Mode::Menu { stack: vec![level], at: (40, 6) };
        let out = render(&mut app, 146, 39);
        println!("\n{out}\n");
        assert!(out.contains("Split right"));
        assert!(out.contains("Send message"));
        assert!(out.contains("Settings"));
    }

    #[test]
    fn palette_renders() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        app.mode = crate::client::Mode::Palette { query: "sp".into(), sel: 0 };
        let out = render(&mut app, 146, 39);
        println!("\n{out}\n");
        assert!(out.contains("split"));
    }
}
