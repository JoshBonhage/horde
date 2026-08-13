//! Overlays drawn in front of a dimmed frame: help, pickers, rename, and toasts.

use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};
use ratatui::Frame;

use super::pane_widget::fmt_elapsed;
use super::statusbar::shorten_home;
use super::{centered, color, fill, put_line, truncate, wrap_text};
use crate::client::menu::Act;
use crate::client::settings::{self, Kind};
use crate::client::{App, Mode, PickKind};
use crate::config::{Action, Trigger};
use crate::proto::Snapshot;
use crate::proto::{AgentLine, AgentState, Delivery, Digest, NoticeLevel, Rgb};
use crate::theme::Theme;

/// A bordered panel with a title, used by every overlay so they read as one family.
fn panel(f: &mut Frame, area: TRect, title: &str, theme: &Theme) -> TRect {
    fill(f.buffer_mut(), area, theme.ui.panel_bg);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(theme.ui.accent)).bg(color(theme.ui.panel_bg)))
        .title(Line::from(vec![
            Span::styled(" ", Style::default().fg(color(theme.ui.accent))),
            Span::styled(
                title.to_string(),
                Style::default()
                    .fg(color(theme.ui.accent))
                    .bg(color(theme.ui.panel_bg))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default().fg(color(theme.ui.accent))),
        ]));
    let inner = block.inner(area);
    block.render(area, f.buffer_mut());
    inner
}

pub fn help(f: &mut Frame, area: TRect, app: &App) {
    let theme = app.cfg.theme.clone();
    let prefix = app.cfg.prefix.describe();

    // Only prefix bindings and direct chords are worth listing; unbound actions are not.
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, trigger, action) in app.cfg.keys.described() {
        let key = match trigger {
            Trigger::Prefix(c) => format!("{prefix} {}", c.describe()),
            Trigger::Direct(c) => c.describe(),
        };
        let desc = describe_action(&name, &action);
        rows.push((key, desc));
    }

    let w = (area.width.saturating_sub(8)).min(74).max(30);
    let h = (rows.len() as u16 + 4).min(area.height.saturating_sub(2));
    let outer = centered(area, w, h);
    let inner = panel(f, outer, "keys", &theme);

    let key_w = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(10).min(22) as u16;
    let mut y = inner.y;
    let bottom = inner.y + inner.height;

    put_line(
        f.buffer_mut(),
        inner.x,
        y,
        inner.width,
        Line::from(vec![Span::styled(
            "esc closes".to_string(),
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.panel_bg)),
        )]),
    );
    y += 2;

    for (key, desc) in rows {
        if y >= bottom {
            break;
        }
        let pad = key_w.saturating_sub(key.chars().count() as u16) + 2;
        put_line(
            f.buffer_mut(),
            inner.x,
            y,
            inner.width,
            Line::from(vec![
                Span::styled(
                    key,
                    Style::default()
                        .fg(color(theme.ui.accent))
                        .bg(color(theme.ui.panel_bg))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " ".repeat(pad as usize),
                    Style::default().bg(color(theme.ui.panel_bg)),
                ),
                Span::styled(
                    truncate(&desc, inner.width.saturating_sub(key_w + 2) as usize),
                    Style::default().fg(color(theme.ui.text)).bg(color(theme.ui.panel_bg)),
                ),
            ]),
        );
        y += 1;
    }
}

/// Human-readable label for a binding. Falls back to the action name with dashes, which is
/// already close to English.
fn describe_action(name: &str, action: &Action) -> String {
    match action {
        Action::Detach => "detach (agents keep running)".into(),
        Action::Help => "this help".into(),
        Action::Palette => "command palette".into(),
        Action::SpaceSwitcher => "switch space".into(),
        Action::CopyMode => "scrollback / copy mode".into(),
        Action::RenamePane => "rename the focused pane".into(),
        Action::Settings => "settings".into(),
        Action::SendPrefix => "send the prefix key to the pane".into(),
        Action::SidebarFocus => "walk the sidebar with j/k".into(),
        Action::TogglePin => "pin this agent to the top of the sidebar".into(),
        Action::Roster => "every project and agent, full screen".into(),
        Action::CycleLens => "filter the agent list".into(),
        // The generic name-to-words fallback would render this as bare "digest", which does
        // not say what it does.
        Action::Cmd(crate::proto::Cmd::RequestDigest) => "what happened while you were away".into(),
        Action::Cmd(_) => name.replace('_', " "),
    }
}

pub fn picker(f: &mut Frame, area: TRect, app: &App) {
    let theme = app.cfg.theme.clone();
    let (title, query, sel, items) = match &app.mode {
        Mode::Palette { query, sel } => ("command", query.clone(), *sel, app.palette_items()),
        Mode::SpaceSwitcher { query, sel } => ("space", query.clone(), *sel, app.space_items()),
        _ => return,
    };

    let w = (area.width.saturating_sub(10)).min(60).max(24);
    let h = ((items.len() as u16).min(12) + 4).min(area.height.saturating_sub(2));
    let outer = centered(area, w, h);
    let inner = panel(f, outer, title, &theme);

    let panel_bg = Style::default().bg(color(theme.ui.panel_bg));
    put_line(
        f.buffer_mut(),
        inner.x,
        inner.y,
        inner.width,
        Line::from(vec![
            Span::styled("❯ ".to_string(), panel_bg.fg(color(theme.ui.accent))),
            Span::styled(query.clone(), panel_bg.fg(color(theme.ui.text))),
            // A visible caret makes it obvious the field has focus.
            Span::styled("▏".to_string(), panel_bg.fg(color(theme.ui.accent))),
        ]),
    );

    let list_top = inner.y + 2;
    let room = inner.height.saturating_sub(2) as usize;
    // Keep the selection on screen when the list is longer than the panel.
    let start = sel.saturating_sub(room.saturating_sub(1));

    for (i, item) in items.iter().enumerate().skip(start).take(room) {
        let y = list_top + (i - start) as u16;
        let selected = i == sel;
        let bg = if selected { theme.ui.title_bg } else { theme.ui.panel_bg };
        let marker = if selected { "▎" } else { " " };
        put_line(
            f.buffer_mut(),
            inner.x,
            y,
            inner.width,
            Line::from(vec![
                Span::styled(
                    marker.to_string(),
                    Style::default().fg(color(theme.ui.accent)).bg(color(bg)),
                ),
                Span::styled(
                    format!(" {}", truncate(&item.label, inner.width.saturating_sub(4) as usize)),
                    if selected {
                        Style::default()
                            .fg(color(theme.ui.text))
                            .bg(color(bg))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(color(theme.ui.text_dim)).bg(color(bg))
                    },
                ),
            ]),
        );
    }

    if items.is_empty() {
        put_line(
            f.buffer_mut(),
            inner.x + 1,
            list_top,
            inner.width,
            Line::from(vec![Span::styled(
                "no matches".to_string(),
                panel_bg.fg(color(theme.ui.text_faint)),
            )]),
        );
    }
}

pub fn prompt(f: &mut Frame, area: TRect, app: &App) {
    let theme = app.cfg.theme.clone();
    let Mode::Prompt { prompt, value } = &app.mode else { return };

    let w = (area.width.saturating_sub(10)).min(60).max(24);
    let outer = centered(area, w, 5);
    let inner = panel(f, outer, prompt.title(), &theme);
    let panel_bg = Style::default().bg(color(theme.ui.panel_bg));

    // Show the tail of a long value so the caret stays visible.
    let room = inner.width.saturating_sub(4) as usize;
    let shown: String = if value.chars().count() > room {
        value.chars().skip(value.chars().count() - room).collect()
    } else {
        value.clone()
    };

    put_line(
        f.buffer_mut(),
        inner.x,
        inner.y + 1,
        inner.width,
        Line::from(vec![
            Span::styled("❯ ".to_string(), panel_bg.fg(color(theme.ui.accent))),
            Span::styled(shown, panel_bg.fg(color(theme.ui.text))),
            Span::styled("▏".to_string(), panel_bg.fg(color(theme.ui.accent))),
        ]),
    );
    put_line(
        f.buffer_mut(),
        inner.x,
        inner.y + 3,
        inner.width,
        Line::from(vec![Span::styled(
            prompt.hint().to_string(),
            panel_bg.fg(color(theme.ui.text_faint)),
        )]),
    );
}

/// A right-click context menu, anchored at the cursor and flipped to stay on screen.
pub fn menu(f: &mut Frame, area: TRect, app: &mut App) {
    let theme = app.cfg.theme.clone();
    let (stack, at) = match &app.mode {
        Mode::Menu { stack, at } => (stack.clone(), *at),
        _ => return,
    };
    let Some(level) = stack.last() else { return };

    let w = crate::client::menu::width_for(level);
    let h = level.items.len() as u16 + 2;
    // Flip rather than clip when the menu would run off an edge.
    let x = if at.0 + w <= area.x + area.width { at.0 } else { at.0.saturating_sub(w) };
    let y = if at.1 + h <= area.y + area.height { at.1 } else { at.1.saturating_sub(h) };
    let rect = TRect::new(
        x.min(area.x + area.width.saturating_sub(w)),
        y.min(area.y + area.height.saturating_sub(h)),
        w,
        h,
    );

    // Breadcrumb the title when inside a submenu, so it is clear esc steps back.
    let title = if stack.len() > 1 {
        format!("{} › {}", stack[stack.len() - 2].title, level.title)
    } else {
        level.title.clone()
    };
    let inner = panel(f, rect, &truncate(&title, w.saturating_sub(6) as usize), &theme);
    let panel_bg = Style::default().bg(color(theme.ui.panel_bg));

    app.menu_hits.clear();
    app.menu_rect =
        crate::proto::Rect::new(rect.x, rect.y, rect.width, rect.height);

    for (i, item) in level.items.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        if item.is_separator() {
            for dx in 0..inner.width {
                if let Some(c) = f.buffer_mut().cell_mut((inner.x + dx, y)) {
                    c.set_symbol("─");
                    c.set_style(panel_bg.fg(color(theme.ui.border)));
                }
            }
            continue;
        }
        let selected = i == level.sel;
        let bg = if selected { theme.ui.title_bg } else { theme.ui.panel_bg };
        let row_bg = Style::default().bg(color(bg));

        // A submenu entry advertises itself with a chevron rather than needing a legend.
        let arrow = if matches!(item.act, Act::Submenu(_)) { "›" } else { " " };
        let hint = item.hint.clone();
        let label_room = inner.width.saturating_sub(hint.chars().count() as u16 + 4);
        let label = truncate(&item.label, label_room as usize);
        let pad = inner
            .width
            .saturating_sub(2 + label.chars().count() as u16 + hint.chars().count() as u16 + 1);

        let mut label_style = row_bg.fg(color(match item.color {
            // An entry whose colour is its content keeps that colour whether selected or
            // not — dimming a swatch would misreport the colour it is offering.
            Some(c) => c,
            None if selected => theme.ui.text,
            None => theme.ui.text_dim,
        }));
        if selected {
            label_style = label_style.add_modifier(Modifier::BOLD);
        }

        put_line(
            f.buffer_mut(),
            inner.x,
            y,
            inner.width,
            Line::from(vec![
                Span::styled(
                    if selected { "▎" } else { " " },
                    row_bg.fg(color(theme.ui.accent)),
                ),
                Span::styled(label, label_style),
                Span::styled(" ".repeat(pad as usize), row_bg),
                Span::styled(hint, row_bg.fg(color(theme.ui.text_faint))),
                Span::styled(arrow.to_string(), row_bg.fg(color(theme.ui.accent))),
            ]),
        );
        app.menu_hits.push((y, i));
    }
}

/// The settings page: categories on the left, that category's settings on the right.
pub fn settings(f: &mut Frame, area: TRect, app: &mut App) {
    let theme = app.cfg.theme.clone();
    let (cat, sel, capturing) = match &app.mode {
        Mode::Settings { cat, sel, capture } => (*cat, *sel, capture.clone()),
        _ => return,
    };
    let cats = settings::Category::all();
    let cat = cat.min(cats.len() - 1);
    let category = cats[cat];
    let rows = settings::rows(&app.cfg, category);

    let w = (area.width.saturating_sub(6)).min(86).max(44);
    let h = (rows.len() as u16 + 4).max(cats.len() as u16 + 4).min(area.height.saturating_sub(2));
    let outer = centered(area, w, h);
    let inner = panel(f, outer, "settings", &theme);
    let panel_bg = Style::default().bg(color(theme.ui.panel_bg));

    let nav_w = 16u16.min(inner.width / 3);
    let body_x = inner.x + nav_w + 1;
    let body_w = inner.width.saturating_sub(nav_w + 1);
    let bottom = inner.y + inner.height;

    app.settings_cat_hits.clear();
    app.settings_row_hits.clear();

    // Vertical divider between nav and body.
    for dy in 0..inner.height.saturating_sub(1) {
        if let Some(c) = f.buffer_mut().cell_mut((inner.x + nav_w, inner.y + dy)) {
            c.set_symbol("│");
            c.set_style(panel_bg.fg(color(theme.ui.border)));
        }
    }

    for (i, c) in cats.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= bottom.saturating_sub(1) {
            break;
        }
        let active = i == cat;
        let bg = if active { theme.ui.title_bg } else { theme.ui.panel_bg };
        let mut st = Style::default()
            .bg(color(bg))
            .fg(color(if active { theme.ui.accent } else { theme.ui.text_dim }));
        if active {
            st = st.add_modifier(Modifier::BOLD);
        }
        let label = truncate(c.label(), nav_w.saturating_sub(2) as usize);
        let pad = nav_w.saturating_sub(1 + label.chars().count() as u16);
        put_line(
            f.buffer_mut(),
            inner.x,
            y,
            nav_w,
            Line::from(vec![
                Span::styled(
                    if active { "▎" } else { " " },
                    Style::default().bg(color(bg)).fg(color(theme.ui.accent)),
                ),
                Span::styled(label, st),
                Span::styled(" ".repeat(pad as usize), Style::default().bg(color(bg))),
            ]),
        );
        app.settings_cat_hits.push((y, i));
    }

    let value_w = 22u16.min(body_w / 2);
    let label_w = body_w.saturating_sub(value_w + 3);
    // One line of the body, flattened first so wrapped notes count toward the scroll
    // window. The Keybindings category alone is longer than any terminal is tall.
    enum L {
        Rule,
        Note(String),
        Row(usize),
    }
    let mut lines: Vec<L> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match &r.kind {
            Kind::Separator => lines.push(L::Rule),
            Kind::Note => {
                for line in wrap_text(&r.label, body_w.saturating_sub(3) as usize) {
                    lines.push(L::Note(line));
                }
            }
            _ => lines.push(L::Row(i)),
        }
    }

    // Scroll so the selection stays on screen, keeping a row of context either side.
    let cap = inner.height.saturating_sub(1) as usize;
    let sel_line = lines
        .iter()
        .position(|l| matches!(l, L::Row(i) if *i == sel))
        .unwrap_or(0);
    let offset = if lines.len() <= cap {
        0
    } else if sel_line < cap.saturating_sub(2) {
        0
    } else {
        (sel_line + 3).saturating_sub(cap).min(lines.len().saturating_sub(cap))
    };

    let mut y = inner.y;
    for l in lines.iter().skip(offset).take(cap) {
        match l {
            L::Rule => {
                for dx in 0..body_w {
                    if let Some(c) = f.buffer_mut().cell_mut((body_x + dx, y)) {
                        c.set_symbol("─");
                        c.set_style(panel_bg.fg(color(theme.ui.border)));
                    }
                }
            }
            L::Note(text) => {
                // Explanatory prose, indented and dim so it does not read as a setting.
                put_line(
                    f.buffer_mut(),
                    body_x + 2,
                    y,
                    body_w,
                    Line::from(vec![Span::styled(
                        text.clone(),
                        panel_bg.fg(color(theme.ui.text_faint)),
                    )]),
                );
            }
            L::Row(i) => {
                let r = &rows[*i];
                let selected = *i == sel;
                let bg = if selected { theme.ui.title_bg } else { theme.ui.panel_bg };
                let row_bg = Style::default().bg(color(bg));

                let base_color = match r.kind {
                    Kind::Action(_) | Kind::ReadOnly => theme.ui.text_faint,
                    _ => theme.ui.accent,
                };
                // While capturing, the row being rebound says so instead of showing the
                // key it is about to lose.
                let capturing_here = matches!(
                    &r.kind,
                    Kind::Keybind(n) if capturing.as_deref() == Some(n.as_str())
                );
                let value =
                    if capturing_here { "press a key…".to_string() } else { r.value.clone() };
                let value_color = if capturing_here { theme.ui.warn } else { base_color };

                let label = truncate(&r.label, label_w as usize);
                let value = truncate(&value, value_w as usize);
                let pad = body_w.saturating_sub(
                    2 + label.chars().count() as u16 + value.chars().count() as u16,
                );

                let mut label_style = row_bg.fg(color(match r.kind {
                    Kind::ReadOnly => theme.ui.text_faint,
                    _ if selected => theme.ui.text,
                    _ => theme.ui.text_dim,
                }));
                if selected {
                    label_style = label_style.add_modifier(Modifier::BOLD);
                }

                put_line(
                    f.buffer_mut(),
                    body_x,
                    y,
                    body_w,
                    Line::from(vec![
                        Span::styled(
                            if selected { "▎" } else { " " },
                            row_bg.fg(color(theme.ui.accent)),
                        ),
                        Span::styled(format!("{label} "), label_style),
                        Span::styled(" ".repeat(pad as usize), row_bg),
                        Span::styled(value, row_bg.fg(color(value_color))),
                    ]),
                );
                app.settings_row_hits.push((y, *i));
            }
        }
        y += 1;
    }

    // Say there is more, and roughly where you are in it.
    if lines.len() > cap {
        let pct = (offset * 100) / lines.len().saturating_sub(cap).max(1);
        let marker = format!(" {pct:>3}% ");
        let mw = marker.chars().count() as u16;
        if mw + 2 < body_w {
            put_line(
                f.buffer_mut(),
                body_x + body_w - mw - 1,
                bottom.saturating_sub(1),
                mw,
                Line::from(vec![Span::styled(
                    marker,
                    panel_bg.fg(color(theme.ui.text_faint)),
                )]),
            );
        }
    }

    let hint = if capturing.is_some() {
        "press the key to bind · esc cancels"
    } else {
        match rows.get(sel).map(|r| &r.kind) {
            Some(Kind::Keybind(_)) => "enter rebinds · tab switches section · esc closes",
            Some(Kind::Action(_)) => "enter runs · tab switches section · esc closes",
            _ => "←/→ change · ↑/↓ move · tab section · esc closes",
        }
    };
    put_line(
        f.buffer_mut(),
        inner.x + 1,
        bottom.saturating_sub(1),
        inner.width,
        Line::from(vec![Span::styled(
            hint.to_string(),
            panel_bg.fg(color(theme.ui.text_faint)),
        )]),
    );
}

/// Toasts stack down the top-right corner.

/// The catch-up report: what happened while you were detached.
///
/// Built as a flat list of styled lines and then windowed, rather than laid out section by
/// section, so scrolling is one offset instead of per-section arithmetic.
pub fn digest(f: &mut Frame, area: TRect, app: &App) {
    let theme = app.cfg.theme.clone();
    let Some(d) = app.digest.as_ref() else { return };
    let scroll = match app.mode {
        Mode::Digest { scroll } => scroll,
        _ => 0,
    };

    let w = (area.width.saturating_sub(6)).min(84).max(34);
    // Measure the content first so the panel is the size of the report rather than the size
    // of the screen: a three-line digest in a full-height frame looks broken.
    // The two borders and the footer are what the content does not occupy.
    let lines = digest_lines(d, w.saturating_sub(2) as usize, &theme);
    let wanted = lines.len() as u16 + 3;
    let h = wanted.min(area.height.saturating_sub(4)).max(6);
    let outer = centered(area, w, h);
    let title = format!("while you were away · {}", ago(d.now.saturating_sub(d.since)));
    let inner = panel(f, outer, &title, &theme);
    let bottom = inner.y + inner.height;
    // Reserve the last row for the footer, which has to stay visible at any scroll offset.
    let view_h = inner.height.saturating_sub(1) as usize;
    let max_scroll = lines.len().saturating_sub(view_h);
    let scroll = scroll.min(max_scroll);

    let mut y = inner.y;
    for line in lines.iter().skip(scroll).take(view_h) {
        if y + 1 >= bottom {
            break;
        }
        put_line(f.buffer_mut(), inner.x, y, inner.width, line.clone());
        y += 1;
    }

    let hint = if max_scroll > 0 {
        format!("esc closes · ↑↓ scrolls · {}/{}", scroll + 1, max_scroll + 1)
    } else {
        "esc closes".to_string()
    };
    put_line(
        f.buffer_mut(),
        inner.x,
        bottom.saturating_sub(1),
        inner.width,
        Line::from(vec![Span::styled(
            hint,
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.panel_bg)),
        )]),
    );
}

/// `42m`, `3h`, `2d` — how long ago, at the coarsest unit that is still true.
fn ago(millis: u64) -> String {
    let secs = millis / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Every digest section flattened into one styled list, in the order you would want to be
/// told: what is stuck, what finished, the board, then the traffic.
fn digest_lines(d: &Digest, w: usize, t: &Theme) -> Vec<Line<'static>> {
    let panel = Style::default().bg(color(t.ui.panel_bg));
    let mut out: Vec<Line<'static>> = Vec::new();

    let heading = |out: &mut Vec<Line<'static>>, text: &str| {
        if !out.is_empty() {
            out.push(Line::from(vec![Span::styled(String::new(), panel)]));
        }
        out.push(Line::from(vec![Span::styled(
            text.to_string(),
            panel.fg(color(t.ui.text_dim)).add_modifier(Modifier::BOLD),
        )]));
    };

    let agent_rows = |out: &mut Vec<Line<'static>>, rows: &[AgentLine], detail: Rgb| {
        for a in rows {
            out.push(Line::from(vec![
                Span::styled("  ".to_string(), panel),
                Span::styled(
                    format!("{} ", a.state.glyph()),
                    panel.fg(color(state_color(a.state, t))),
                ),
                Span::styled(
                    format!("{:<14} ", truncate(&a.name, 14)),
                    panel.fg(color(t.ui.text)),
                ),
                Span::styled(format!("{:<6} ", ago(a.elapsed * 1000)), panel.fg(color(t.ui.text_dim))),
                Span::styled(
                    a.activity.clone().unwrap_or_else(|| a.reason.clone()),
                    panel.fg(color(detail)),
                ),
            ]));
        }
    };

    if !d.needs_you.is_empty() {
        heading(&mut out, "needs you");
        agent_rows(&mut out, &d.needs_you, t.ui.warn);
    }
    if !d.finished.is_empty() {
        heading(&mut out, "finished");
        agent_rows(&mut out, &d.finished, t.ui.text_dim);
    }
    if !d.working.is_empty() {
        heading(&mut out, "still working");
        agent_rows(&mut out, &d.working, t.ui.text_dim);
    }

    // Before the board, because a firing is the reason some of the board's work exists.
    if !d.fired.is_empty() {
        heading(&mut out, "horde decided");
        for f in &d.fired {
            out.push(Line::from(vec![
                Span::styled("  ".to_string(), panel),
                Span::styled("▸ ".to_string(), panel.fg(color(t.ui.working))),
                Span::styled(truncate(f, w.saturating_sub(6)), panel.fg(color(t.ui.text))),
            ]));
        }
    }

    if !d.tasks_done.is_empty() || d.tasks_added > 0 || d.tasks_open + d.tasks_claimed > 0 {
        heading(&mut out, "board");
        for task in &d.tasks_done {
            let (glyph, gc) =
                if task.dropped { ("✕", t.ui.error) } else { ("●", t.ui.ok) };
            out.push(Line::from(vec![
                Span::styled("  ".to_string(), panel),
                Span::styled(format!("{glyph} "), panel.fg(color(gc))),
                Span::styled(format!("#{:<3} ", task.id), panel.fg(color(t.ui.text_faint))),
                Span::styled(
                    truncate(&task.text, w.saturating_sub(22)),
                    panel.fg(color(t.ui.text)),
                ),
                Span::styled(
                    task.owner.as_ref().map(|o| format!("  [{o}]")).unwrap_or_default(),
                    panel.fg(color(t.ui.text_faint)),
                ),
            ]));
            if let Some(r) = &task.result {
                for line in wrap_text(r, w.saturating_sub(10)) {
                    out.push(Line::from(vec![
                        Span::styled("       → ".to_string(), panel.fg(color(t.ui.text_faint))),
                        Span::styled(line, panel.fg(color(t.ui.ok))),
                    ]));
                }
            }
        }
        let mut standing = Vec::new();
        if d.tasks_added > 0 {
            standing.push(format!("{} added", d.tasks_added));
        }
        if d.tasks_open > 0 {
            standing.push(format!("{} open", d.tasks_open));
        }
        if d.tasks_claimed > 0 {
            standing.push(format!("{} claimed", d.tasks_claimed));
        }
        if !standing.is_empty() {
            out.push(Line::from(vec![
                Span::styled("  ".to_string(), panel),
                Span::styled(standing.join(", "), panel.fg(color(t.ui.text_dim))),
            ]));
        }
    }

    if !d.messages.is_empty() {
        let n = d.messages.len();
        heading(&mut out, &if n == 1 { "bus · 1 message".into() } else { format!("bus · {n} messages") });
        for m in &d.messages {
            let (mark, mc) = match m.delivery {
                Delivery::Delivered => ("✓", t.ui.ok),
                Delivery::Queued => ("⧗", t.ui.warn),
                Delivery::Dropped => ("✕", t.ui.error),
            };
            let route = match (m.expects_reply, m.reply_to, m.broadcast) {
                (_, Some(n), _) => format!("re #{n} {} → {}", m.from, m.to),
                (true, None, _) => format!("ask #{} {} → {}", m.id, m.from, m.to),
                (_, _, true) => format!("{} → all", m.from),
                _ => format!("{} → {}", m.from, m.to),
            };
            out.push(Line::from(vec![
                Span::styled("  ".to_string(), panel),
                Span::styled(format!("{mark} "), panel.fg(color(mc))),
                Span::styled(format!("{route}: "), panel.fg(color(t.ui.accent_alt))),
                Span::styled(
                    truncate(&m.body.split_whitespace().collect::<Vec<_>>().join(" "),
                             w.saturating_sub(route.chars().count() + 8)),
                    panel.fg(color(t.ui.text)),
                ),
            ]));
        }
    }

    if !d.gone.is_empty() {
        heading(&mut out, "exited");
        for name in &d.gone {
            out.push(Line::from(vec![
                Span::styled("  ".to_string(), panel),
                Span::styled("✕ ".to_string(), panel.fg(color(t.ui.error))),
                Span::styled(name.clone(), panel.fg(color(t.ui.text))),
            ]));
        }
    }
    if !d.warnings.is_empty() {
        heading(&mut out, "warnings");
        for warn in &d.warnings {
            for line in wrap_text(warn, w.saturating_sub(4)) {
                out.push(Line::from(vec![
                    Span::styled("  ! ".to_string(), panel.fg(color(t.ui.warn))),
                    Span::styled(line, panel.fg(color(t.ui.text))),
                ]));
            }
        }
    }

    out
}

fn state_color(s: AgentState, t: &Theme) -> Rgb {
    match s {
        AgentState::Blocked => t.ui.warn,
        AgentState::Working => t.ui.accent,
        AgentState::Done => t.ui.ok,
        _ => t.ui.text_dim,
    }
}

pub fn toasts(f: &mut Frame, area: TRect, app: &App) {
    let theme = app.cfg.theme.clone();
    if app.toasts.is_empty() {
        return;
    }

    let w = (area.width / 3).clamp(24, 46);
    let mut y = area.y + 1;

    for toast in app.toasts.iter().take(4) {
        let accent = match toast.level {
            NoticeLevel::Info => theme.ui.accent_alt,
            NoticeLevel::Warn => theme.ui.warn,
            NoticeLevel::Error => theme.ui.error,
        };
        let body = wrap_text(&toast.text, w.saturating_sub(4) as usize);
        let h = body.len() as u16 + 2;
        if y + h > area.y + area.height {
            break;
        }
        let rect = TRect::new(area.x + area.width.saturating_sub(w + 1), y, w, h);

        fill(f.buffer_mut(), rect, theme.ui.panel_bg);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color(accent)).bg(color(theme.ui.panel_bg)));
        let inner = block.inner(rect);
        block.render(rect, f.buffer_mut());

        for (i, line) in body.iter().enumerate() {
            put_line(
                f.buffer_mut(),
                inner.x,
                inner.y + i as u16,
                inner.width,
                Line::from(vec![Span::styled(
                    line.clone(),
                    Style::default().fg(color(theme.ui.text)).bg(color(theme.ui.panel_bg)),
                )]),
            );
        }
        y += h;
    }
}

/// One row in a picker.
#[derive(Debug, Clone)]
pub struct Item {
    pub label: String,
    pub kind: PickKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Keymap;
    use crate::proto::TaskLine;

    #[test]
    fn every_binding_gets_a_non_empty_description() {
        let km = Keymap::default();
        for (name, _, action) in km.described() {
            let d = describe_action(&name, &action);
            assert!(!d.trim().is_empty(), "{name} has no description");
        }
    }

    #[test]
    fn action_descriptions_prefer_prose_over_the_raw_name() {
        assert_eq!(describe_action("detach", &Action::Detach), "detach (agents keep running)");
        // Command actions fall back to a readable form of their name.
        assert_eq!(
            describe_action("split_right", &Action::Cmd(crate::proto::Cmd::SplitRight)),
            "split right"
        );
    }

    fn sample_digest() -> Digest {
        Digest {
            since: 0,
            now: 2_520_000, // 42m
            fresh: false,
            needs_you: vec![AgentLine {
                name: "reviewer".into(),
                state: AgentState::Blocked,
                elapsed: 720,
                activity: None,
                reason: "approval prompt".into(),
            }],
            finished: vec![AgentLine {
                name: "builder".into(),
                state: AgentState::Done,
                elapsed: 240,
                activity: Some("22 tools · 6 files".into()),
                reason: "hook".into(),
            }],
            working: vec![],
            gone: vec!["worker3".into()],
            warnings: vec![],
            fired: vec![],
            tasks_done: vec![TaskLine {
                id: 4,
                text: "write the bus tests".into(),
                owner: Some("builder".into()),
                result: Some("18 tests added, all passing".into()),
                dropped: false,
            }],
            tasks_added: 3,
            tasks_open: 2,
            tasks_claimed: 1,
            messages: vec![],
            turns: 2,
        }
    }

    fn digest_text(d: &Digest, w: usize) -> String {
        let t = Theme::horde();
        digest_lines(d, w, &t)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What horde decided on its own goes above the board, because a firing is the reason some
    /// of that work exists — and it is the section that makes arming triggers reviewable.
    #[test]
    fn what_horde_decided_is_reported_above_the_board_it_filled() {
        let mut d = sample_digest();
        d.fired = vec!["#1 put task #7 on the board: review yesterday's diff".into()];
        let out = digest_text(&d, 70);
        let decided = out.find("horde decided").expect("a firing must be reported");
        let board = out.find("board").expect("the board section should be present");
        assert!(decided < board, "the cause comes before the effect:\n{out}");
        assert!(out.contains("review yesterday's diff"), "{out}");

        // And a digest with no firings costs no heading.
        assert!(!digest_text(&sample_digest(), 70).contains("horde decided"));
    }

    #[test]
    fn the_overlay_leads_with_what_needs_a_human() {
        let out = digest_text(&sample_digest(), 70);
        let needs = out.find("needs you").expect("a blocked agent must be reported");
        let board = out.find("board").expect("the board section should be present");
        assert!(needs < board, "stuck work comes before finished work:\n{out}");
        assert!(out.contains("reviewer"), "{out}");
        assert!(out.contains("approval prompt"), "{out}");
    }

    #[test]
    fn task_results_are_shown_not_just_task_names() {
        // The result is the whole point of asking what happened.
        let out = digest_text(&sample_digest(), 70);
        assert!(out.contains("write the bus tests"), "{out}");
        assert!(out.contains("18 tests added, all passing"), "{out}");
        assert!(out.contains("3 added, 2 open, 1 claimed"), "{out}");
    }

    #[test]
    fn hook_activity_is_preferred_over_the_detection_reason() {
        let out = digest_text(&sample_digest(), 70);
        assert!(out.contains("22 tools · 6 files"), "{out}");
        assert!(!out.contains("hook"), "the reason should give way to real activity:\n{out}");
    }

    #[test]
    fn empty_sections_take_no_space() {
        let mut d = sample_digest();
        d.gone.clear();
        let out = digest_text(&d, 70);
        assert!(!out.contains("exited"), "{out}");
        assert!(!out.contains("warnings"), "{out}");
        // Match the heading, not the substring: a task called "write the bus tests" would
        // otherwise look like a bus section.
        assert!(
            !out.lines().any(|l| l.starts_with("bus ")),
            "no messages means no bus section:\n{out}"
        );
    }

    #[test]
    fn a_narrow_panel_still_produces_lines_and_never_panics() {
        for w in [20usize, 30, 46, 70, 120] {
            let out = digest_text(&sample_digest(), w);
            assert!(out.contains("needs you"), "width {w}: {out}");
        }
    }

    #[test]
    fn ago_reads_at_the_coarsest_true_unit() {
        assert_eq!(ago(0), "0s");
        assert_eq!(ago(59_000), "59s");
        assert_eq!(ago(2_520_000), "42m");
        assert_eq!(ago(7_200_000), "2h");
        assert_eq!(ago(180_000_000), "2d");
    }

    #[test]
    fn the_digest_binding_is_described_in_prose() {
        let d = describe_action("digest", &Action::Cmd(crate::proto::Cmd::RequestDigest));
        assert!(d.contains("while you were away"), "{d}");
    }

    // -- roster ------------------------------------------------------------

    fn roster_text(cards: &[Card]) -> String {
        cards.iter().flat_map(|c| c.lines.iter().cloned()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn the_roster_shows_every_space_with_its_agents_and_cwd() {
        let cards = roster_cards(&crate::client::roster::tests::snap(), 40);
        let out = roster_text(&cards);
        println!("\n{out}\n");
        for want in ["api-refactor", "docs", "builder", "reviewer", "writer", "/tmp"] {
            assert!(out.contains(want), "{want} missing from:\n{out}");
        }
        assert_eq!(cards.len(), 2, "one card per space");
    }

    /// Prose where the sidebar has to use glyphs — the whole reason a full-screen view earns
    /// its place is that it can afford to spell things out.
    #[test]
    fn the_roster_card_uses_prose_where_the_sidebar_uses_glyphs() {
        let cards = roster_cards(&crate::client::roster::tests::snap(), 40);
        let out = roster_text(&cards);
        assert!(out.contains("1 needs you"), "{out}");
        assert!(out.contains("1 working"), "{out}");
    }

    /// A project with nothing running is a fact worth showing, not a card worth skipping —
    /// "which of my six repos has nothing on it" is exactly what this view is for.
    #[test]
    fn a_space_with_no_agents_still_gets_a_card_saying_so() {
        let mut s = crate::client::roster::tests::snap();
        s.panes.retain(|p| p.space != 2);
        let cards = roster_cards(&s, 40);
        assert_eq!(cards.len(), 2, "the empty space keeps its card");
        let docs = cards
            .iter()
            .find(|c| c.focuses[0].1 == crate::client::roster::Focus::Group(2))
            .unwrap();
        assert!(docs.lines.iter().any(|l| l.contains("no agents")), "{:?}", docs.lines);
    }

    #[test]
    fn a_roster_card_carries_the_role_and_who_started_the_pane() {
        let mut s = crate::client::roster::tests::snap();
        let p = s.panes.iter_mut().find(|p| p.id == 1).unwrap();
        p.role = Some("reviewer".into());
        p.spawned_by = Some(3);
        let out = roster_text(&roster_cards(&s, 48));
        assert!(out.contains("[reviewer]"), "{out}");
        assert!(out.contains("horde"), "a machine-started pane says so: {out}");
    }

    /// Every agent line has to be landable, or the cursor could not reach it from this view.
    #[test]
    fn every_agent_line_is_something_the_cursor_can_land_on() {
        use crate::client::roster::Focus;
        let cards = roster_cards(&crate::client::roster::tests::snap(), 40);
        let agents: Vec<Focus> = cards
            .iter()
            .flat_map(|c| c.focuses.iter().map(|(_, f)| *f))
            .filter(|f| matches!(f, Focus::Agent(_)))
            .collect();
        assert_eq!(agents.len(), 3, "{agents:?}");
        for c in &cards {
            for (i, _) in &c.focuses {
                assert!(*i < c.lines.len(), "focus past the end of the card");
            }
        }
    }

    #[test]
    fn a_narrow_card_truncates_rather_than_overflowing() {
        let cards = roster_cards(&crate::client::roster::tests::snap(), 12);
        for c in &cards {
            for l in &c.lines {
                assert!(super::super::width(l) <= 12, "{l:?}");
            }
        }
    }

}

/// One project's card in the roster.
#[derive(Debug, Clone)]
pub struct Card {
    pub lines: Vec<String>,
    /// Which line each landable row is on, so the overlay can mark the cursor and act on it.
    /// The first is always the card's own space, which is what identifies the card.
    pub focuses: Vec<(usize, crate::client::roster::Focus)>,
}

/// The roster as data.
///
/// Pure, and separate from drawing it, for the same reason `digest_lines` is: it is what
/// makes the content assertable without standing up a terminal.
pub fn roster_cards(snap: &Snapshot, width: usize) -> Vec<Card> {
    use crate::client::roster::{collect_agents, Focus, Roll};
    let agents = collect_agents(snap);
    let mut out = Vec::new();
    for space in &snap.spaces {
        let mine: Vec<_> = agents.iter().filter(|a| a.space == space.id).collect();
        let mut roll = Roll::default();
        for a in &mine {
            roll.add(a.state);
        }

        let mut lines = vec![truncate(&space.name, width)];
        let mut focuses = vec![(0usize, Focus::Group(space.id))];
        lines.push(truncate(&roll.prose(), width));
        lines.push(truncate(&shorten_home(&space.cwd), width));

        for a in &mine {
            let Some(p) = snap.panes.iter().find(|p| p.id == a.pane) else { continue };
            let Some(info) = p.agent.as_ref() else { continue };
            let detail = match info.state {
                AgentState::Working => fmt_elapsed(info.elapsed),
                _ => info.state.label().to_string(),
            };
            let role = p.role.as_deref().map(|r| format!(" [{r}]")).unwrap_or_default();
            // `horde` marks a pane horde started rather than you — the same fact `agent.list`
            // exposes and nothing else in the UI has ever shown.
            let by = if p.spawned_by.is_some() { " · horde" } else { "" };
            focuses.push((lines.len(), Focus::Agent(a.pane)));
            lines.push(truncate(
                &format!("{} {}{role}  {detail}{by}", info.state.glyph(), info.name),
                width,
            ));
            // Only while it is actually doing something — a finished turn's counts are stale
            // trivia, the same reason the sidebar gates this.
            if info.state == AgentState::Working {
                if let Some(act) = info.activity.summary() {
                    lines.push(truncate(&format!("    {act}"), width));
                }
            }
        }
        // A blank line between cards, so a column reads as several cards rather than one list.
        lines.push(String::new());
        out.push(Card { lines, focuses });
    }
    out
}

/// The full-screen roster: every project, every agent, at a size that can afford detail.
pub fn roster(f: &mut Frame, area: TRect, app: &mut App) {
    let theme = app.cfg.theme.clone();
    let Some(snap) = app.snapshot.clone() else { return };
    let scroll = match app.mode {
        Mode::Roster { scroll } => scroll,
        _ => 0,
    };

    let outer = centered(area, area.width.saturating_sub(4), area.height.saturating_sub(2));
    let inner = panel(f, outer, "roster", &theme);
    if inner.width < 20 || inner.height < 4 {
        return;
    }
    let bottom = inner.y + inner.height;
    // Reserve the last row for the hint, which has to stay put at any scroll offset.
    let view_h = inner.height.saturating_sub(1) as usize;

    // Three columns at most: past that a card is too narrow to say anything the sidebar could
    // not. Below eighty columns, one.
    const CARD: u16 = 34;
    let cols = (inner.width / CARD).clamp(1, 3) as usize;
    let col_w = (inner.width as usize / cols).saturating_sub(1);
    let cards = roster_cards(&snap, col_w);

    // Fill the shortest column each time, so a session with one busy project and five quiet
    // ones does not leave two columns empty.
    let mut columns: Vec<Vec<(String, Option<crate::client::roster::Focus>)>> =
        vec![Vec::new(); cols];
    for card in &cards {
        let target = columns
            .iter()
            .enumerate()
            .min_by_key(|(i, c)| (c.len(), *i))
            .map(|(i, _)| i)
            .unwrap_or(0);
        for (i, line) in card.lines.iter().enumerate() {
            let focus = card.focuses.iter().find(|(li, _)| *li == i).map(|(_, f)| *f);
            columns[target].push((line.clone(), focus));
        }
    }

    let longest = columns.iter().map(|c| c.len()).max().unwrap_or(0);
    let scroll = scroll.min(longest.saturating_sub(view_h));
    app.roster_hits.clear();

    for (ci, column) in columns.iter().enumerate() {
        let x = inner.x + (ci * (col_w + 1)) as u16;
        let mut y = inner.y;
        for (line, focus) in column.iter().skip(scroll).take(view_h) {
            if y >= bottom.saturating_sub(1) {
                break;
            }
            let on_cursor = focus.is_some() && *focus == app.sidebar.cursor;
            let bg = if on_cursor { theme.ui.title_bg } else { theme.ui.panel_bg };
            let fg = match focus {
                // A card's title carries its project's colour, the same one the tab bar and
                // that project's pane borders use — so a card is identifiable before it is read.
                Some(crate::client::roster::Focus::Group(sp)) => theme.space_accent(
                    snap.spaces.iter().find(|s| s.id == *sp).map(|s| s.accent).unwrap_or(0),
                ),
                Some(_) => theme.ui.text_dim,
                None => theme.ui.text_faint,
            };
            let mut style = Style::default().fg(color(fg)).bg(color(bg));
            if matches!(focus, Some(crate::client::roster::Focus::Group(_))) {
                style = style.add_modifier(Modifier::BOLD);
            }
            put_line(
                f.buffer_mut(),
                x,
                y,
                col_w as u16,
                Line::from(vec![Span::styled(format!("{line:<col_w$}"), style)]),
            );
            if let Some(fc) = focus {
                app.roster_hits.push((y, x, col_w as u16, *fc));
            }
            y += 1;
        }
    }

    let hint = "enter jumps · j/k move · p pin · esc closes";
    put_line(
        f.buffer_mut(),
        inner.x,
        bottom - 1,
        inner.width,
        Line::from(vec![Span::styled(
            hint.to_string(),
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.panel_bg)),
        )]),
    );
}
