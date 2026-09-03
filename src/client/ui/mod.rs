//! Frame composition and shared drawing helpers.

pub mod bus_drawer;
pub mod dashboard;
pub mod graph_view;
pub mod kanban;
pub mod logo;
pub mod markdown;
pub mod notes;
pub mod overlays;
pub mod pane_widget;
pub mod sidebar;
pub mod setup;
pub mod sprite;
pub mod statusbar;
pub mod zombie;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::Frame;

use super::{App, Mode};
use crate::proto::{AgentState, Rect, Rgb, Severity, Snapshot};
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

/// The editor's text column and how many rows of it there are, for a screen this size.
///
/// A column, not a wall — the same measure the reader uses. Shared with the key handler
/// because scrolling has to agree with drawing about where a line wraps, and two copies of
/// this arithmetic would agree only until one of them changed.
pub fn editor_page(area: TRect) -> (u16, u16) {
    (area.width.saturating_sub(6).min(96), area.height.saturating_sub(4))
}

/// Break an already-styled line into the rows it takes at `w` columns, keeping its styling.
///
/// The same wrap the editor's cursor is measured against ([`editor::wrap_breaks`]), applied
/// to spans rather than to a string — so a rendered line and the cursor in it always agree
/// about where the rows start.
pub fn wrap_spans(line: &Line<'_>, w: usize) -> Vec<Line<'static>> {
    let own = |line: &Line<'_>| -> Line<'static> {
        Line::from(
            line.spans
                .iter()
                .map(|s| Span::styled(s.content.to_string(), s.style))
                .collect::<Vec<_>>(),
        )
    };
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>().concat();
    let breaks = crate::client::editor::wrap_breaks(&text, w);
    if breaks.len() < 2 {
        return vec![own(line)];
    }
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new(); breaks.len()];
    let mut at = 0usize; // char offset into the whole line
    let mut row = 0usize;
    for span in &line.spans {
        let mut held = String::new();
        for ch in span.content.chars() {
            if row + 1 < breaks.len() && at == breaks[row + 1] {
                if !held.is_empty() {
                    rows[row].push(Span::styled(std::mem::take(&mut held), span.style));
                }
                row += 1;
            }
            held.push(ch);
            at += 1;
        }
        if !held.is_empty() {
            rows[row].push(Span::styled(held, span.style));
        }
    }
    rows.into_iter().map(Line::from).collect()
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

/// How the screen splits between a note and the vault's tree: `(note, tree)`.
///
/// One function because the renderer wraps the text to the note's width and the key handler
/// pages and follows the cursor by it. Two copies of this arithmetic agree until the tree
/// appears, and then the editor wraps at a column that is no longer where the text ends.
pub fn editor_split(screen: TRect, tree: bool) -> (TRect, Option<TRect>) {
    if !tree {
        return (screen, None);
    }
    match notes::tree_width(screen) {
        Some(w) => {
            let mut note = screen;
            note.width -= w;
            let side = TRect { x: screen.x + note.width, y: screen.y, width: w, height: screen.height };
            (note, Some(side))
        }
        None => (screen, None),
    }
}

/// The board or the list, with its header — everything the kanban screen is except the hint
/// row, the question and the status bar.
///
/// Extracted because a card opened from the list draws the list behind it, and a popup whose
/// background is a second copy of the drawing code is a popup that stops matching what it is
/// floating over. Returns the body area, which is what the rest of the screen is measured off.
#[allow(clippy::too_many_arguments)]
fn kanban_surface(
    f: &mut Frame,
    app: &mut App,
    snap: &Snapshot,
    theme: &Theme,
    area: TRect,
    view: kanban::View,
    col: usize,
    sel: usize,
) -> TRect {
    let project = (!app.kanban_all)
        .then(|| snap.focused_space.and_then(|s| snap.spaces.iter().find(|x| x.id == s)))
        .flatten()
        .map(|s| s.name.clone());
    let cols = kanban::columns(
        app.kanban.as_ref(),
        project.as_deref(),
        app.kanban_archived,
        &app.kanban_query,
    );
    let body = kanban::body_area(area, trect(snap.status));
    let now = now_millis();
    let due = cols
        .iter()
        .flat_map(|c| c.cards.iter())
        .filter(|c| c.due.is_some_and(|d| kanban::days_until(d, now) <= 0))
        .count();
    kanban::draw_header(
        f.buffer_mut(),
        area,
        theme,
        project.as_deref(),
        &app.kanban_query,
        app.kanban_archived,
        due,
    );
    match view {
        kanban::View::Board => {
            // Recorded so the mouse handler resolves a cell against the same area the
            // renderer laid out — the layout itself it computes for free.
            app.kanban_area = Some(body);
            app.kanban_scroll.resize(cols.len().max(1), 0);
            let lay = kanban::layout(&cols, body, &app.kanban_scroll, col);
            let card = kanban::selected(&cols, col, sel);
            kanban::draw(f.buffer_mut(), theme, &cols, &lay, col, card, app.card_drag.as_ref(), now);
        }
        kanban::View::List => {
            app.kanban_area = Some(body);
            let rows = kanban::list_rows(&cols, now);
            let card = rows.get(sel).map(|(c, _)| c.id);
            let lay = kanban::list_layout(body, rows.len(), sel);
            kanban::draw_list(f.buffer_mut(), &lay, theme, &rows, card, now);
        }
    }
    body
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

    // The dashboard is the one view that *replaces* the panes rather than floating over
    // them. Everything else in horde is an overlay over a dimmed frame, because everything
    // else is something you do to a session you can still see. A start screen is not.
    if let Mode::Graph { sel } = app.mode {
        // Recorded so the mouse handler resolves a cell the same way the renderer placed it.
        app.graph_plot = Some(graph_view::split(area, app.graph_panel).0);
        if let (Some(sim), Some(g)) = (app.sim.as_ref(), app.graph.as_ref()) {
            app.graph_hits = graph_view::draw(
                f.buffer_mut(),
                area,
                &theme,
                g,
                sim,
                sel,
                app.graph_zoom,
                app.graph_centre,
                // The client's own clock rather than the daemon's: "recently" means recently
                // to the person looking at the screen.
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                app.graph_since.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0),
                app.graph_panel.then(|| graph_view::Preview {
                    title: g.nodes.get(sel).map(|n| n.label.as_str()).unwrap_or(""),
                    note: app.preview.as_deref(),
                    ghost: g.nodes.get(sel).is_some_and(|n| n.ghost),
                }),
                &mut app.images,
            );
        }
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

    if let Mode::Setup { step } = app.mode {
        setup::draw(f.buffer_mut(), area, &theme, step, &app.setup);
        overlays::toasts(f, area, app);
        return;
    }

    // Your own board, and one card of it.
    //
    // The board replaces the frame, for the reason the start screen does: arranging work is
    // not something you are doing *to* a session you can still see. A card opened from the
    // *list* is the other case — the list is a view you scan, and reading one row of it is a
    // peek rather than a departure, so that card floats over the list it came from and the
    // place you had keeps itself.
    if let Mode::Kanban { view, col, sel } = app.mode {
        let body = kanban_surface(f, app, &snap, &theme, area, view, col, sel);
        // The question, where the keys would otherwise be — because while it is up, those
        // keys are not what the keyboard is doing.
        match app.kanban_ask.as_ref() {
            Some(asking) => kanban::draw_ask(f.buffer_mut(), body, &theme, asking),
            None => kanban::draw_hints(f.buffer_mut(), body, &theme, view),
        }
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

    if let Mode::Card { id, focus } = app.mode {
        // Whichever view it was opened from is still the view it belongs to. From the list it
        // floats; from the board it takes the frame, as it always has.
        let (back_view, back_col, back_sel) = app.kanban_back;
        let popped = back_view == kanban::View::List;
        let full = kanban::body_area(area, trect(snap.status));
        let into = match popped {
            true => {
                kanban_surface(f, app, &snap, &theme, area, back_view, back_col, back_sel);
                let rect = kanban::popup_area(full);
                app.card_popup = Some(rect);
                overlays::panel(f, rect, &format!("card #{id}"), &theme)
            }
            false => {
                app.card_popup = None;
                full
            }
        };
        let card = app.kanban.as_ref().and_then(|k| k.cards.iter().find(|c| c.id == id));
        if let Some(card) = card {
            app.card_lines = kanban::draw_detail(
                f.buffer_mut(),
                into,
                &theme,
                card,
                focus,
                app.card_edit.as_ref(),
                app.card_scroll,
                now_millis(),
            );
        }
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

    if let Mode::Editor { ref path, scroll, vim: ref vim_mode, project } = app.mode {
        let vim_mode = vim_mode.clone();
        let theme2 = theme.clone();
        fill(f.buffer_mut(), area, theme2.ui.bg);

        // The vault's tree, when a note is being read inside the vault rather than on its own.
        // Taken off the right before anything is measured, so the note's own column is centred
        // in what is left rather than under the tree.
        app.vault_tree_hits.clear();
        let (area, side) = editor_split(area, app.vault_tree && !project);
        {
            if let Some(tree) = side {
                let rows = notes::rows(app.vault_index.as_ref(), "");
                // Follow the note being edited, so entering the vault at a note filed deep in
                // it does not open on a tree scrolled to somebody else's folder.
                let at = rows.iter().position(|r| r.path == *path).unwrap_or(0);
                let room = (tree.height as usize).saturating_sub(2);
                let scroll = (at + 1).saturating_sub(room);
                app.vault_tree_hits = notes::draw_tree(
                    f.buffer_mut(),
                    tree,
                    &theme2,
                    app.vault_index.as_ref(),
                    &rows,
                    path,
                    scroll,
                );
            }
        }

        let (col, rows) = editor_page(area);
        let x = area.x + (area.width.saturating_sub(col)) / 2;
        let dirty = app.buffer.as_ref().is_some_and(|b| b.dirty);
        // What a language server has said about this file, if one is watching it.
        let diags: &[crate::proto::Diag] =
            app.diags.get(path).map(|v| v.as_slice()).unwrap_or(&[]);
        let errors = diags.iter().filter(|d| d.severity == Severity::Error).count();
        let warnings = diags.iter().filter(|d| d.severity == Severity::Warning).count();
        let mut head = vec![ratatui::text::Span::styled(
            format!("{}{}", path, if dirty { "  •" } else { "" }),
            Style::default()
                .fg(color(if dirty { theme2.ui.working } else { theme2.ui.text_faint }))
                .bg(color(theme2.ui.bg)),
        )];
        // Counted in the header as well as marked in the margin, because the margin only
        // says what is on this screen and the file is longer than the screen.
        if errors > 0 {
            head.push(ratatui::text::Span::styled(
                format!("   {errors}◍"),
                Style::default().fg(color(theme2.ui.blocked)).bg(color(theme2.ui.bg)),
            ));
        }
        if warnings > 0 {
            head.push(ratatui::text::Span::styled(
                format!("  {warnings}△"),
                Style::default().fg(color(theme2.ui.working)).bg(color(theme2.ui.bg)),
            ));
        }
        put_line(f.buffer_mut(), x, area.y, col, Line::from(head));

        // Where the cursor ends up on screen, filled in as the lines are laid out. The
        // completion list needs it too, and after wrapping it is no longer something either
        // of them can work out from the line number alone.
        let mut cursor_at: Option<(u16, u16)> = None;
        if let Some(buf) = app.buffer.as_ref() {
            let top = area.y + 2;
            // Live preview: every line renders as it will read, except the one the cursor
            // is on, which shows its source. Hiding characters under a cursor would make the
            // arrow keys lie about where they are going — so the line you are working on
            // tells the truth, and the rest of the page shows you what you are making.
            // Live preview is a markdown thing. Applying it to source would style `**p` as
            // bold and eat the asterisks — a renderer confidently rewriting code it does not
            // understand, which is worse than showing it plainly.
            let markdownish = path.rsplit('.').next().is_some_and(|e| {
                matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdx")
            });
            // Code gets colour instead. Recomputed only when the text changes, which is what
            // keeps typing feeling like typing.
            if !markdownish {
                let rev = app.buffer.as_ref().map(|b| b.rev).unwrap_or(0);
                let stale =
                    app.highlight.as_ref().is_none_or(|(p, r, _)| *r != rev || p != path);
                if stale {
                    let text = app.buffer.as_ref().map(|b| b.text()).unwrap_or_default();
                    app.highlight = crate::client::syntax::highlight(path, &text, &theme2)
                        .map(|lines| (path.clone(), rev, lines));
                }
            }
            // A long line is drawn down the page rather than off the side of it, so `used`
            // counts screen rows and `n` counts lines in the file. They stopped being the
            // same number the moment anything wrapped.
            let mut used = 0u16;
            for (n, line) in buf.lines.iter().enumerate().skip(scroll) {
                if used >= rows {
                    break;
                }
                let rendered = if markdownish && n != buf.line {
                    markdown::live_line(line, &theme2)
                } else if let Some(hl) =
                    app.highlight.as_ref().and_then(|(_, _, ls)| ls.get(n)).filter(|_| !markdownish)
                {
                    hl.clone()
                } else {
                    Line::from(ratatui::text::Span::styled(
                        line.clone(),
                        Style::default().fg(color(theme2.ui.text)).bg(color(theme2.ui.bg)),
                    ))
                };
                let first = used;
                for row in wrap_spans(&rendered, col as usize) {
                    if used >= rows {
                        break;
                    }
                    put_line(f.buffer_mut(), x, top + used, col, row);
                    used += 1;
                }
                // The cursor's own line is drawn from its source, so the wrap it was measured
                // against is the wrap that was drawn.
                if n == buf.line {
                    let (r, c) = buf.wrapped_cursor(col as usize);
                    if first + r as u16 <= rows.saturating_sub(1) {
                        cursor_at =
                            Some((x + (c as u16).min(col.saturating_sub(1)), top + first + r as u16));
                    }
                }
                // The mark goes in the margin rather than under the text: a squiggle drawn
                // into an already-styled line means splitting spans the highlighter just
                // built, and the margin is where the eye looks for "which line" anyway. It
                // marks the line's first row, which is where the line starts.
                if let Some(worst) = diags
                    .iter()
                    .filter(|d| d.line as usize == n)
                    .min_by_key(|d| d.severity)
                {
                    put_line(
                        f.buffer_mut(),
                        x.saturating_sub(2),
                        top + first,
                        2,
                        Line::from(ratatui::text::Span::styled(
                            worst.severity.glyph().to_string(),
                            Style::default()
                                .fg(color(match worst.severity {
                                    Severity::Error => theme2.ui.blocked,
                                    Severity::Warning => theme2.ui.working,
                                    _ => theme2.ui.text_faint,
                                }))
                                .bg(color(theme2.ui.bg)),
                        )),
                    );
                }
            }
            // The cursor belongs to whichever line is being typed into. While the `:` line is
            // open that is the prompt, not the text — put it in both places and the one you
            // are actually editing is anybody's guess.
            if vim_mode.prompt().is_none() {
                if let Some(pos) = cursor_at {
                    f.set_cursor_position(pos);
                }
            }
        }

        // One row up from the bottom, because the bottom row is the status bar and it is
        // drawn after this. The hint row had been landing underneath it, which is why nobody
        // had ever seen it.
        // The completion list, over the text and under the cursor's line.
        if let (Some(c), Some(buf)) = (app.completions.as_ref(), app.buffer.as_ref()) {
            let prefix = buf.text_from(c.from);
            let items = c.matching(&prefix);
            if let (false, Some((cursor_x, cursor_row))) = (items.is_empty(), cursor_at) {
                let below = area.y + area.height.saturating_sub(2) - cursor_row.min(area.y + area.height);
                // Below the line normally; above it when the line is near the bottom, so the
                // list never covers the thing being completed.
                let want = (items.len() as u16).min(8);
                let (list_y, room) = if below > want + 1 {
                    (cursor_row + 1, want)
                } else {
                    (cursor_row.saturating_sub(want.min(cursor_row - area.y)), want.min(cursor_row - area.y))
                };
                let widest = items
                    .iter()
                    .take(room as usize)
                    .map(|i| width(&i.label) + i.kind.as_deref().map(|k| width(k) + 2).unwrap_or(0))
                    .max()
                    .unwrap_or(10);
                let w = (widest as u16 + 3).min(col.saturating_sub(2)).max(12);
                // Under the word being completed, which after wrapping is the cursor less
                // what has been typed rather than a column counted from the start of a line.
                let lx = cursor_x
                    .saturating_sub(width(&prefix) as u16)
                    .max(x)
                    .min(x + col.saturating_sub(w));
                let top_at = c.sel.saturating_sub(room.saturating_sub(1) as usize);
                for (i, item) in items.iter().skip(top_at).take(room as usize).enumerate() {
                    let selected = top_at + i == c.sel;
                    let bg = if selected { theme2.ui.selection } else { theme2.ui.panel_bg };
                    let y = list_y + i as u16;
                    fill(f.buffer_mut(), TRect { x: lx, y, width: w, height: 1 }, bg);
                    let mut spans = vec![ratatui::text::Span::styled(
                        format!(" {}", truncate(&item.label, w as usize - 2)),
                        Style::default()
                            .fg(color(if selected { theme2.ui.text } else { theme2.ui.text_dim }))
                            .bg(color(bg)),
                    )];
                    // The kind is worth a column of its own: `fn` against `field` is most of
                    // what you are choosing between when two names look alike.
                    if let Some(kind) = item.kind.as_deref() {
                        let used = width(&item.label) + 1;
                        if used + kind.len() + 2 < w as usize {
                            spans.push(ratatui::text::Span::styled(
                                format!("{}{kind} ", " ".repeat(w as usize - used - kind.len() - 1)),
                                Style::default().fg(color(theme2.ui.text_faint)).bg(color(bg)),
                            ));
                        }
                    }
                    put_line(f.buffer_mut(), lx, y, w, Line::from(spans));
                }
            }
        }

        let foot = area.y + area.height.saturating_sub(2);
        match vim_mode.prompt() {
            // The line being typed *is* the hint: it shows the command as it is written, the
            // way it does in the editor everyone learned this from.
            Some((glyph, typed)) => {
                put_line(
                    f.buffer_mut(),
                    x,
                    foot,
                    col,
                    Line::from(ratatui::text::Span::styled(
                        format!("{glyph}{typed}"),
                        Style::default().fg(color(theme2.ui.text)).bg(color(theme2.ui.bg)),
                    )),
                );
                let cx = x + 1 + typed.chars().count() as u16;
                f.set_cursor_position((cx.min(x + col.saturating_sub(1)), foot));
            }
            // What is wrong with the line you are on outranks the list of keys: the keys do
            // not change, and this does. Nowhere else has room for the message — the margin
            // holds one glyph and the line itself is the text you are writing.
            None => {
                let here = app.buffer.as_ref().map(|b| b.line).unwrap_or(0);
                let on_line: Vec<&crate::proto::Diag> =
                    diags.iter().filter(|d| d.line as usize == here).collect();
                let line = match on_line.first() {
                    Some(d) => {
                        let more = match on_line.len() {
                            1 => String::new(),
                            n => format!("   (+{} more)", n - 1),
                        };
                        let source = d.source.as_deref().map(|s| format!("{s}: ")).unwrap_or_default();
                        Line::from(ratatui::text::Span::styled(
                            truncate(&format!("{source}{}{more}", d.message), col as usize),
                            Style::default()
                                .fg(color(match d.severity {
                                    Severity::Error => theme2.ui.blocked,
                                    Severity::Warning => theme2.ui.working,
                                    _ => theme2.ui.text_dim,
                                }))
                                .bg(color(theme2.ui.bg)),
                        ))
                    }
                    None => {
                        // In the vault the two keys worth naming are the ones the vault adds:
                        // a note to write and the tree to see the rest. A hint row that lists
                        // `dd yy p` while the thing you cannot guess goes unmentioned is a
                        // hint row spent on the parts vim already taught you.
                        let hint = if vim_mode.typing() {
                            "esc normal   ctrl+s save   ctrl+z undo   ctrl+r read"
                        } else if side.is_some() {
                            "i insert   ctrl+n new note   \\ tree   / search   :wq write and go"
                        } else if diags.is_empty() {
                            "i insert   : command   / search   dd yy p   u undo   :wq write and go"
                        } else {
                            "i insert   : command   / search   ]d [d next problem   :wq write and go"
                        };
                        Line::from(ratatui::text::Span::styled(
                            hint.to_string(),
                            Style::default()
                                .fg(color(theme2.ui.text_faint))
                                .bg(color(theme2.ui.bg)),
                        ))
                    }
                };
                put_line(f.buffer_mut(), x, foot, col, line);
            }
        }
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

    if let Mode::Reader { scroll, link } = app.mode {
        let theme2 = theme.clone();
        let body = app.vault.as_ref().and_then(|v| v.body.clone()).unwrap_or_default();
        let title = app
            .vault
            .as_ref()
            .and_then(|v| v.notes.first().map(|n| n.title.clone()))
            .unwrap_or_default();
        let backlinks = app.vault.as_ref().map(|v| v.backlinks.len()).unwrap_or(0);
        // A column, not a wall. Prose is read at a comfortable measure whatever the terminal
        // is doing, which is the one typographic rule a terminal can still honour.
        let col = area.width.saturating_sub(6).min(96);
        let x = area.x + (area.width.saturating_sub(col)) / 2;
        fill(f.buffer_mut(), area, theme2.ui.bg);

        // Both counts in the header, because both answer "is there more to this note than
        // the note" — and the tasks are below the fold until you scroll to them.
        let open = app
            .vault
            .as_ref()
            .map(|v| v.tasks.iter().filter(|t| !t.done && !t.dropped).count())
            .unwrap_or(0);
        let head = match (backlinks, open) {
            (0, 0) => title.clone(),
            (0, n) => format!("{title}   ◇ {n} open"),
            (1, 0) => format!("{title}   ← 1 note links here"),
            (b, 0) => format!("{title}   ← {b} notes link here"),
            (1, n) => format!("{title}   ← 1 note links here   ◇ {n} open"),
            (b, n) => format!("{title}   ← {b} notes link here   ◇ {n} open"),
        };
        put_line(
            f.buffer_mut(),
            x,
            area.y,
            col,
            Line::from(ratatui::text::Span::styled(
                head,
                Style::default()
                    .fg(color(theme2.ui.text_faint))
                    .bg(color(theme2.ui.bg)),
            )),
        );

        // Where the note is, so the pictures it embeds can be found and drawn.
        let root = app.vault.as_ref().map(|v| std::path::PathBuf::from(&v.root));
        let dir = app
            .vault
            .as_ref()
            .and_then(|v| v.notes.first())
            .and_then(|n| std::path::Path::new(&n.path).parent().map(|p| p.to_path_buf()))
            .and_then(|rel| root.as_ref().map(|r| r.join(rel)));
        // Half the reading area. A picture that fills the screen buries the note it is in,
        // and the note is the thing you opened.
        let home = markdown::Home { dir: dir.clone(), vault: root.clone() };
        let mut rendered =
            markdown::render_in(&body, col, &theme2, home.at((area.height / 2).clamp(6, 24)));

        // The work outstanding on this note, appended to it. Here rather than in a panel
        // because it belongs to the note the way its backlinks do — and because scrolling
        // then carries it, which a panel would not.
        let tasks = app.vault.as_ref().map(|v| v.tasks.clone()).unwrap_or_default();
        if !tasks.is_empty() {
            let faint = Style::default().fg(color(theme2.ui.text_faint)).bg(color(theme2.ui.bg));
            rendered.lines.push(Line::from(""));
            rendered.lines.push(Line::from(ratatui::text::Span::styled(
                "─".repeat(col as usize),
                faint,
            )));
            rendered.lines.push(Line::from(ratatui::text::Span::styled("TASKS".to_string(), faint)));
            rendered.lines.push(Line::from(""));
            for t in &tasks {
                let (glyph, colour) = match (t.dropped, t.done, t.owner.is_some()) {
                    (true, _, _) => ("✕", theme2.ui.text_faint),
                    (_, true, _) => ("✓", theme2.ui.done),
                    (_, _, true) => ("◆", theme2.ui.working),
                    _ => ("◇", theme2.ui.text_dim),
                };
                let who = t.owner.as_deref().map(|o| format!("   {o}")).unwrap_or_default();
                rendered.lines.push(Line::from(vec![
                    ratatui::text::Span::styled(
                        format!("{glyph} "),
                        Style::default().fg(color(colour)).bg(color(theme2.ui.bg)),
                    ),
                    ratatui::text::Span::styled(
                        truncate(&t.text, col as usize - 4 - width(&who)),
                        Style::default().fg(color(theme2.ui.text_dim)).bg(color(theme2.ui.bg)),
                    ),
                    ratatui::text::Span::styled(who, faint),
                ]));
            }
        }
        let selected_line = rendered.links.get(link).map(|(l, _)| *l);
        let body_top = area.y + 2;
        let rows = area.height.saturating_sub(4);
        // Pictures the terminal draws itself, translated from "which line" into "which cell
        // on screen". Only the ones actually in view: an image scrolled off the top must be
        // taken down, not left floating over the text that replaced it.
        app.images = rendered
            .images
            .iter()
            .filter_map(|p| markdown::visible(p, scroll, rows as usize, x, body_top))
            .collect();

        for (i, line) in rendered.lines.iter().skip(scroll).take(rows as usize).enumerate() {
            let y = body_top + i as u16;
            // The link under the cursor is marked in the margin: the reader has to know
            // which one `enter` will follow before pressing it.
            if selected_line == Some(scroll + i) {
                put_line(
                    f.buffer_mut(),
                    x.saturating_sub(2),
                    y,
                    2,
                    Line::from(ratatui::text::Span::styled(
                        "▸".to_string(),
                        Style::default().fg(color(theme2.ui.accent)).bg(color(theme2.ui.bg)),
                    )),
                );
            }
            put_line(f.buffer_mut(), x, y, col, line.clone());
        }

        let more = rendered.lines.len().saturating_sub(scroll + rows as usize);
        let hint = if more > 0 {
            format!("tab links   enter follow   e edit   esc back   {more} more lines")
        } else {
            "tab links   enter follow   e edit   esc back".to_string()
        };
        put_line(
            f.buffer_mut(),
            x,
            // Above the status bar, which is drawn after this and would otherwise cover it —
            // the same mistake the editor's hint row was making.
            area.y + area.height.saturating_sub(2),
            col,
            Line::from(ratatui::text::Span::styled(
                hint,
                Style::default().fg(color(theme2.ui.text_faint)).bg(color(theme2.ui.bg)),
            )),
        );
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

    if let Mode::Files { ref query, sel } = app.mode {
        let rows = notes::file_rows(app.files.as_ref(), query, &app.open_dirs);
        app.notes_hits =
            notes::draw_files(f.buffer_mut(), area, &theme, app.files.as_ref(), &rows, query, sel);
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

    if let Mode::Notes { ref query, sel } = app.mode {
        let rows = notes::rows(app.vault_index.as_ref(), query);
        app.notes_hits =
            notes::draw(f.buffer_mut(), area, &theme, app.vault_index.as_ref(), &rows, query, sel);
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

    if let Mode::Dashboard { sel } = app.mode {
        let rows = dashboard::rows(&snap, now_millis());
        // Where the wordmark landed, recorded for the loop: it is how the client knows
        // whether there is a stage to walk across without drawing a frame to find out.
        app.zombie_stage = dashboard::stage(area, &rows);
        let walk = app.zombie.and_then(|w| w.phase().at());
        app.dashboard_hits = dashboard::draw(f.buffer_mut(), area, &theme, &rows, sel, walk);
        statusbar::StatusBar {
            snap: &snap,
            theme: &theme,
            mode: app.mode.clone(),
            prefix: app.cfg.prefix.describe(),
        }
        .render(trect(snap.status), f.buffer_mut());
        overlays::toasts(f, area, app);
        return;
    }

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
        // Drawn above instead of here: it replaces the frame rather than sitting on it, and
        // this arm is only reached when there is a frame to sit on.
        Mode::Dashboard { .. }
        | Mode::Notes { .. }
        | Mode::Graph { .. }
        | Mode::Reader { .. }
        | Mode::Editor { .. }
        | Mode::Setup { .. }
        | Mode::Kanban { .. }
        | Mode::Card { .. }
        | Mode::Files { .. } => {}
        // No dimming: which-key is a hint you read while still looking at your work, not a
        // panel that takes the screen. It also has to be legible the instant it appears.
        Mode::Leader { pending, .. } => overlays::which_key(f, area, app, pending),
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
            // Over the surface it was asked from, not over the panes. This match runs after
            // the panes are drawn, so a prompt opened from a full-screen view used to dim
            // that view away and replace it with the multiplexer — which reads as being
            // thrown out of what you were doing in order to answer one question.
            //
            // The editor does not appear here: it asks with its own footer and never leaves.
            if let Some(back) = app.prompt_back.clone() {
                if let Mode::Notes { ref query, sel } = *back {
                    let rows = notes::rows(app.vault_index.as_ref(), query);
                    notes::draw(f.buffer_mut(), area, &theme, app.vault_index.as_ref(), &rows, query, sel);
                }
            }
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
                    tabs: vec![1], focused_tab: Some(1), agent_count: 2, attention_count: 1, accent: 0, collapsed: false, repo: None, notes: None, lsp: Vec::new() },
                SpaceInfo { id: 2, name: "docs".into(), cwd: "/y".into(),
                    tabs: vec![2], focused_tab: Some(2), agent_count: 1, attention_count: 0, accent: 1, collapsed: false, repo: None, notes: None, lsp: Vec::new() },
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
            cards_due: 0,
            triggers_armed: 0,
            recents: Vec::new(),
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

    /// A card opened from the list floats over it: the list is still there behind, and the
    /// description and the thread — the part you opened it for — are on it.
    #[test]
    fn a_card_opened_from_the_list_floats_over_it() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        let mut c = crate::proto::Card {
            id: 12,
            title: "wire up the importer".into(),
            column: "Todo".into(),
            pos: 0,
            body: "Read the CSV in chunks so a 200MB file does not blow the heap.".into(),
            due: Some(now_millis() + 2 * 86_400_000),
            tags: vec!["api".into(), "p1".into()],
            project: Some("horde".into()),
            created: now_millis() - 2 * 86_400_000,
            updated: 0,
            archived: false,
            comments: vec![crate::proto::Comment {
                by: "josh@joshmacbook".into(),
                at: now_millis(),
                body: "parked until the schema lands".into(),
            }],
            assist: Some(2 * 86_400_000),
            handed: None,
        };
        c.pos = 0;
        app.kanban = Some(crate::proto::KanbanReply {
            cards: vec![c, crate::proto::Card {
                id: 21,
                title: "someday: rewrite the parser".into(),
                column: "Backlog".into(),
                pos: 0,
                body: String::new(),
                due: None,
                tags: Vec::new(),
                project: None,
                created: 0,
                updated: 0,
                archived: false,
                comments: Vec::new(),
                assist: None,
                handed: None,
            }],
            columns: ["Backlog", "Todo", "Doing", "Done"].iter().map(|s| s.to_string()).collect(),
            project: None,
        });
        app.kanban_all = true;
        app.kanban_back = (kanban::View::List, 0, 0);
        app.mode = Mode::Card { id: 12, focus: kanban::Field::Title };

        let out = render(&mut app, 120, 32);
        println!("\n{out}\n");
        assert!(out.contains("card #12"), "the panel is titled: {out}");
        assert!(out.contains("Read the CSV in chunks"), "the description is on it: {out}");
        assert!(out.contains("parked until the schema lands"), "and the thread: {out}");
        // The list is still there: the popup is inset, so the rows it does not cover show.
        assert!(out.contains("#21"), "the list is still behind it: {out}");
        assert!(out.contains("ID") && out.contains("PROJECT"), "header and all: {out}");
        assert!(app.card_popup.is_some(), "and it recorded where it floated");
    }

    /// From the board it still takes the whole frame, which is the behaviour that was there.
    #[test]
    fn a_card_opened_from_the_board_takes_the_frame() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        app.kanban = Some(crate::proto::KanbanReply {
            cards: vec![crate::proto::Card {
                id: 3,
                title: "fix the auth refresh".into(),
                column: "Doing".into(),
                pos: 0,
                body: String::new(),
                due: None,
                tags: Vec::new(),
                project: None,
                created: 0,
                updated: 0,
                archived: false,
                comments: Vec::new(),
                assist: None,
                handed: None,
            }],
            columns: ["Backlog", "Todo", "Doing", "Done"].iter().map(|s| s.to_string()).collect(),
            project: None,
        });
        app.kanban_all = true;
        app.kanban_back = (kanban::View::Board, 2, 0);
        app.mode = Mode::Card { id: 3, focus: kanban::Field::Title };

        let out = render(&mut app, 120, 32);
        assert!(out.contains("fix the auth refresh"), "{out}");
        assert!(!out.contains("card #3"), "no floating panel: {out}");
        assert!(app.card_popup.is_none());
    }

    fn vaulting() -> (App, crate::proto::Snapshot) {
        let (mut app, snap) = demo();
        app.vault_index = Some(crate::proto::VaultReply {
            root: "/Users/josh/notes".into(),
            notes: vec![
                note("Home.md", "Home"),
                note("Daily/2026-08-20.md", "2026-08-20"),
                note("Projects/horde.md", "horde"),
                note("Projects/horde-desktop.md", "horde-desktop"),
                note("Reading/obsidian.md", "obsidian sync"),
            ],
            body: None,
            backlinks: Vec::new(),
            graph: None,
            space: 0,
            tasks: Vec::new(),
        });
        app.buffer = Some(crate::client::editor::Buffer::new(
            "# Home\n\nThe front page of the vault.\n\n- [[Projects/horde]]\n- [[Daily/2026-08-20]]\n",
        ));
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: crate::client::Vim::Normal,
        };
        (app, snap)
    }

    fn note(path: &str, title: &str) -> crate::proto::NoteLine {
        crate::proto::NoteLine {
            path: path.into(),
            title: title.into(),
            tags: Vec::new(),
            backlinks: 0,
            mtime: 0,
        }
    }

    /// The vault: a note to read and write in, and where it sits among the others.
    #[test]
    fn the_vault_draws_a_note_with_the_tree_beside_it() {
        let (mut app, snap) = vaulting();
        app.snapshot = Some(snap);
        let out = render(&mut app, 110, 24);
        println!("\n{out}\n");
        assert!(out.contains("The front page of the vault"), "the note is being read: {out}");
        assert!(out.contains("VAULT  5"), "the tree says how many: {out}");
        assert!(out.contains("Projects/"), "folders are on it: {out}");
        assert!(out.contains("obsidian sync"), "and notes in them: {out}");
        assert!(out.contains("▸ Home"), "with the one being edited marked: {out}");
        assert!(!app.vault_tree_hits.is_empty(), "and every note it drew is clickable");
    }

    /// A note squeezed to forty columns so a list of its neighbours can fit is the wrong thing
    /// to protect. The note is what you came for.
    #[test]
    fn a_narrow_terminal_keeps_the_note_and_drops_the_tree() {
        let (mut app, snap) = vaulting();
        app.snapshot = Some(snap);
        let out = render(&mut app, 70, 24);
        assert!(out.contains("The front page of the vault"), "{out}");
        assert!(!out.contains("VAULT"), "no tree at this width: {out}");
        assert!(app.vault_tree_hits.is_empty(), "and nothing to click");
    }

    /// The tree is a vault idea. A project file has its own tree at `F` and no reason to grow
    /// a second one listing somebody's notes beside their source.
    #[test]
    fn a_project_file_never_grows_a_vault_tree() {
        let (mut app, snap) = vaulting();
        app.snapshot = Some(snap);
        app.mode = Mode::Editor {
            path: "src/main.rs".into(),
            scroll: 0,
            project: true,
            vim: crate::client::Vim::Normal,
        };
        let out = render(&mut app, 110, 24);
        assert!(!out.contains("VAULT"), "{out}");
    }

    /// Both halves of the split come from one function, so the column the text wraps at is the
    /// column the text ends at.
    #[test]
    fn the_split_gives_the_tree_its_width_and_the_note_the_rest() {
        let screen = TRect::new(0, 0, 110, 24);
        let (note, side) = editor_split(screen, true);
        let side = side.expect("there is room at 110");
        assert_eq!(note.width + side.width, 110);
        assert_eq!(side.x, note.width, "the tree is on the right, flush against the note");

        let (note, side) = editor_split(screen, false);
        assert_eq!((note.width, side), (110, None), "off means the whole screen");

        let (note, side) = editor_split(TRect::new(0, 0, 70, 24), true);
        assert_eq!((note.width, side), (70, None), "and too narrow means the same");
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

    /// A sentence longer than the column used to run off the side of the page and vanish:
    /// there was nothing to see it in, and nothing to say it had happened. It goes down the
    /// page instead, and the cursor goes with it — including when the cursor is sitting
    /// exactly on the break, where the next character will land.
    #[test]
    fn a_long_line_wraps_down_the_page_instead_of_off_the_side_of_it() {
        let (mut app, snap) = demo();
        app.snapshot = Some(snap);
        let text = "the quick brown fox jumps over the lazy dog and keeps on running \
                    well past the end of any column a page could hold";
        let mut b = crate::client::editor::Buffer::new(text);
        b.end();
        app.buffer = Some(b);
        app.mode = crate::client::Mode::Editor {
            path: "note.md".into(),
            scroll: 0,
            project: false,
            vim: crate::client::editor::Vim::Insert,
        };

        let (w, h) = (60u16, 24u16);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let cursor = term.get_cursor_position().unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| {
                        let s = buf.cell((x, y)).unwrap().symbol();
                        if s.is_empty() { " " } else { s }
                    })
                    .collect::<String>()
            })
            .collect();
        let page = rows.join("\n");
        println!("\n{page}\n");

        assert!(page.contains("the quick brown fox"), "the start of it: {page}");
        assert!(page.contains("could hold"), "and the end of it too: {page}");
        // Two rows of one line, not one row of a truncated one.
        let start = rows.iter().position(|r| r.contains("the quick brown fox")).unwrap();
        let end = rows.iter().position(|r| r.contains("could hold")).unwrap();
        assert!(end > start, "the tail is below the head, not beside it");
        // The cursor is where the next character goes: on the last row of the line, just
        // past its last character.
        assert_eq!(cursor.y as usize, end, "the cursor is on the row it is typing into");
        let typed = rows[end].trim_end().chars().count();
        assert_eq!(cursor.x as usize, typed, "and immediately after the text on it");
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
