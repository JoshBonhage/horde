//! Overlays drawn in front of a dimmed frame: help, pickers, rename, and toasts.

use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};
use ratatui::Frame;

use super::{centered, color, fill, put_line, truncate, wrap_text};
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

pub fn rename(f: &mut Frame, area: TRect, app: &App) {
    let theme = app.cfg.theme.clone();
    let Mode::Rename { value, .. } = &app.mode else { return };

    let w = (area.width.saturating_sub(10)).min(50).max(20);
    let outer = centered(area, w, 5);
    let inner = panel(f, outer, "rename pane", &theme);
    let panel_bg = Style::default().bg(color(theme.ui.panel_bg));

    put_line(
        f.buffer_mut(),
        inner.x,
        inner.y + 1,
        inner.width,
        Line::from(vec![
            Span::styled("❯ ".to_string(), panel_bg.fg(color(theme.ui.accent))),
            Span::styled(value.clone(), panel_bg.fg(color(theme.ui.text))),
            Span::styled("▏".to_string(), panel_bg.fg(color(theme.ui.accent))),
        ]),
    );
    put_line(
        f.buffer_mut(),
        inner.x,
        inner.y + 3,
        inner.width,
        Line::from(vec![Span::styled(
            "enter saves · esc cancels · empty clears".to_string(),
            panel_bg.fg(color(theme.ui.text_faint)),
        )]),
    );
}

/// The settings panel: current values, changed in place.
pub fn settings(f: &mut Frame, area: TRect, app: &App) {
    let theme = app.cfg.theme.clone();
    let Mode::Settings { sel } = &app.mode else { return };
    let sel = *sel;
    let rows = settings::rows(&app.cfg);

    let w = (area.width.saturating_sub(8)).min(58).max(30);
    let h = (rows.len() as u16 + 5).min(area.height.saturating_sub(2));
    let outer = centered(area, w, h);
    let inner = panel(f, outer, "settings", &theme);
    let panel_bg = Style::default().bg(color(theme.ui.panel_bg));

    let mut y = inner.y;
    let bottom = inner.y + inner.height;
    // Value column is right-aligned into a fixed gutter so values line up as a column.
    let value_w = 18u16.min(inner.width / 2);
    let label_w = inner.width.saturating_sub(value_w + 3);

    for (i, r) in rows.iter().enumerate() {
        if y >= bottom.saturating_sub(1) {
            break;
        }
        match r.kind {
            Kind::Separator => {
                for x in 0..inner.width {
                    if let Some(c) = f.buffer_mut().cell_mut((inner.x + x, y)) {
                        c.set_symbol("─");
                        c.set_style(panel_bg.fg(color(theme.ui.border)));
                    }
                }
                y += 1;
                continue;
            }
            _ => {}
        }

        let selected = i == sel;
        let bg = if selected { theme.ui.title_bg } else { theme.ui.panel_bg };
        let row_bg = Style::default().bg(color(bg));

        let label_color = match r.kind {
            Kind::ReadOnly => theme.ui.text_faint,
            _ if selected => theme.ui.text,
            _ => theme.ui.text_dim,
        };
        let value_color = match r.kind {
            Kind::Action(_) => theme.ui.text_faint,
            Kind::ReadOnly => theme.ui.text_faint,
            _ => theme.ui.accent,
        };

        let label = truncate(&r.label, label_w as usize);
        let value = truncate(&r.value, value_w as usize);
        let pad = inner
            .width
            .saturating_sub(2 + label.chars().count() as u16 + value.chars().count() as u16);

        let mut label_style = row_bg.fg(color(label_color));
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
                Span::styled(format!("{label} "), label_style),
                Span::styled(" ".repeat(pad as usize), row_bg),
                Span::styled(value, row_bg.fg(color(value_color))),
            ]),
        );
        y += 1;
    }

    // Footer hint, pinned to the last line.
    let hint = match rows.get(sel).map(|r| &r.kind) {
        Some(Kind::Action(_)) => "enter runs · esc closes",
        _ => "←/→ change · ↑/↓ move · esc closes",
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
