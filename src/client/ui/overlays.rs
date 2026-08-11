//! Overlays drawn in front of a dimmed frame: help, pickers, rename, and toasts.

use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};
use ratatui::Frame;

use super::{centered, color, fill, put_line, truncate, wrap_text};
use crate::client::menu::Act;
use crate::client::settings::{self, Kind};
use crate::client::{App, Mode, PickKind};
use crate::config::{Action, Trigger};
use crate::proto::NoticeLevel;
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

        let mut label_style = row_bg.fg(color(if selected {
            theme.ui.text
        } else {
            theme.ui.text_dim
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
}
