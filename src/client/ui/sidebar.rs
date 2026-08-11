//! The left panel: spaces, their agents, and a standing count of what needs you.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::pane_widget::{fmt_elapsed, spinner_frame};
use super::{color, fill, put_line, truncate};
use crate::proto::{AgentState, PaneId, Snapshot, SpaceId};
use crate::theme::Theme;

/// A clickable row, so the client can map a mouse position back to what it represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Space(SpaceId),
    Pane(PaneId),
}

pub struct Sidebar<'a> {
    pub snap: &'a Snapshot,
    pub theme: &'a Theme,
    pub tick: usize,
    pub animate: bool,
    /// Filled in during render so clicks can be resolved without recomputing layout.
    pub hits: &'a mut Vec<(u16, Hit)>,
}

impl Widget for Sidebar<'_> {
    fn render(self, area: TRect, buf: &mut Buffer) {
        if area.width < 8 || area.height < 3 {
            return;
        }
        let t = self.theme;
        fill(buf, area, t.ui.panel_bg);
        self.hits.clear();

        let inner_w = area.width.saturating_sub(2);
        let mut y = area.y;
        let bottom = area.y + area.height;

        // Header
        put_line(
            buf,
            area.x + 1,
            y,
            inner_w,
            Line::from(vec![Span::styled(
                "horde",
                Style::default()
                    .fg(color(t.ui.accent))
                    .bg(color(t.ui.panel_bg))
                    .add_modifier(Modifier::BOLD),
            )]),
        );
        y += 1;
        rule(buf, area.x, y, area.width, t);
        y += 1;

        // Reserve room for the footer summary so it is never pushed off screen.
        let summary_rows = summary_lines(self.snap).len() as u16;
        let list_bottom = bottom.saturating_sub(summary_rows + 1);

        for space in &self.snap.spaces {
            if y >= list_bottom {
                break;
            }
            let focused = self.snap.focused_space == Some(space.id);

            // A space with something waiting takes the attention colour, so you can tell
            // from the collapsed row alone whether it is worth opening.
            let dot_color = if space.attention_count > 0 {
                t.ui.blocked
            } else if focused {
                t.ui.accent
            } else {
                t.ui.text_faint
            };
            let name_style = if focused {
                Style::default()
                    .fg(color(t.ui.text))
                    .bg(color(t.ui.panel_bg))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color(t.ui.text_dim)).bg(color(t.ui.panel_bg))
            };

            let badge = if space.agent_count > 0 { space.agent_count.to_string() } else { String::new() };
            let marker = if focused { "▎" } else { " " };
            let name_room = inner_w.saturating_sub(4 + badge.chars().count() as u16);

            let mut spans = vec![
                Span::styled(marker, Style::default().fg(color(t.ui.accent)).bg(color(t.ui.panel_bg))),
                Span::styled(
                    if space.attention_count > 0 { "● " } else { "○ " },
                    Style::default().fg(color(dot_color)).bg(color(t.ui.panel_bg)),
                ),
                Span::styled(truncate(&space.name, name_room as usize), name_style),
            ];
            if !badge.is_empty() {
                let pad = inner_w
                    .saturating_sub(3 + truncate(&space.name, name_room as usize).chars().count() as u16)
                    .saturating_sub(badge.chars().count() as u16);
                spans.push(Span::styled(
                    " ".repeat(pad as usize),
                    Style::default().bg(color(t.ui.panel_bg)),
                ));
                spans.push(Span::styled(
                    badge,
                    Style::default().fg(color(t.ui.text_faint)).bg(color(t.ui.panel_bg)),
                ));
            }
            put_line(buf, area.x, y, area.width, Line::from(spans));
            self.hits.push((y, Hit::Space(space.id)));
            y += 1;

            // Panes of this space, tab by tab, so ordering matches the tab bar.
            for &tid in &space.tabs {
                let Some(tab) = self.snap.tabs.iter().find(|t| t.id == tid) else { continue };
                for &pid in &tab.panes {
                    if y >= list_bottom {
                        break;
                    }
                    let Some(pane) = self.snap.panes.iter().find(|p| p.id == pid) else { continue };
                    let is_focused = self.snap.focused_pane == Some(pid);
                    let row_bg = if is_focused { t.ui.title_bg } else { t.ui.panel_bg };

                    let (glyph, gcolor, detail, dcolor) = match &pane.agent {
                        Some(a) => {
                            let g = match a.state {
                                AgentState::Working if self.animate => spinner_frame(self.tick),
                                _ => a.state.glyph(),
                            };
                            let c = match a.state {
                                AgentState::Working => t.ui.working,
                                AgentState::Blocked => t.ui.blocked,
                                AgentState::Done => t.ui.done,
                                AgentState::Idle => t.ui.idle,
                                AgentState::Unknown => t.ui.unknown,
                            };
                            let d = match a.state {
                                AgentState::Working => fmt_elapsed(a.elapsed),
                                AgentState::Blocked => "blocked".into(),
                                AgentState::Done => "done".into(),
                                _ => a.state.label().to_string(),
                            };
                            (g.to_string(), c, d, c)
                        }
                        // A plain shell still earns a row: it is part of the space.
                        None => ("·".to_string(), t.ui.text_faint, String::new(), t.ui.text_faint),
                    };

                    let label = pane
                        .agent
                        .as_ref()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| pane.title.clone());

                    let detail_w = detail.chars().count() as u16;
                    let label_room = inner_w.saturating_sub(4 + detail_w + 1);
                    let label = truncate(&label, label_room as usize);
                    let pad = inner_w
                        .saturating_sub(4 + label.chars().count() as u16 + detail_w);

                    let label_style = if is_focused {
                        Style::default()
                            .fg(color(t.ui.text))
                            .bg(color(row_bg))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(color(t.ui.text_dim)).bg(color(row_bg))
                    };

                    let spans = vec![
                        Span::styled("  ", Style::default().bg(color(row_bg))),
                        Span::styled(
                            format!("{glyph} "),
                            Style::default().fg(color(gcolor)).bg(color(row_bg)),
                        ),
                        Span::styled(label, label_style),
                        Span::styled(" ".repeat(pad as usize), Style::default().bg(color(row_bg))),
                        Span::styled(
                            detail,
                            Style::default().fg(color(dcolor)).bg(color(row_bg)),
                        ),
                    ];
                    put_line(buf, area.x, y, area.width, Line::from(spans));
                    self.hits.push((y, Hit::Pane(pid)));
                    y += 1;
                }
            }

            if y < list_bottom {
                y += 1; // blank line between spaces
            }
        }

        // Footer summary, bottom-aligned.
        let lines = summary_lines(self.snap);
        if !lines.is_empty() && bottom > summary_rows + 1 {
            let mut fy = bottom - summary_rows - 1;
            rule(buf, area.x, fy, area.width, t);
            fy += 1;
            for (glyph, count, label, which) in lines {
                let c = match which {
                    AgentState::Blocked => t.ui.blocked,
                    AgentState::Done => t.ui.done,
                    AgentState::Working => t.ui.working,
                    _ => t.ui.idle,
                };
                put_line(
                    buf,
                    area.x + 1,
                    fy,
                    inner_w,
                    Line::from(vec![
                        Span::styled(
                            format!("{glyph} "),
                            Style::default().fg(color(c)).bg(color(t.ui.panel_bg)),
                        ),
                        Span::styled(
                            format!("{count} {label}"),
                            Style::default().fg(color(t.ui.text_dim)).bg(color(t.ui.panel_bg)),
                        ),
                    ]),
                );
                fy += 1;
            }
        }
    }
}

/// Counts worth standing space at the bottom of the sidebar. Only non-zero rows appear, so
/// a quiet session shows nothing rather than three zeroes.
fn summary_lines(snap: &Snapshot) -> Vec<(&'static str, usize, &'static str, AgentState)> {
    let mut blocked = 0;
    let mut done = 0;
    let mut working = 0;
    for p in &snap.panes {
        match p.agent.as_ref().map(|a| a.state) {
            Some(AgentState::Blocked) => blocked += 1,
            Some(AgentState::Done) => done += 1,
            Some(AgentState::Working) => working += 1,
            _ => {}
        }
    }
    let mut out = Vec::new();
    if blocked > 0 {
        out.push((AgentState::Blocked.glyph(), blocked, "needs you", AgentState::Blocked));
    }
    if done > 0 {
        out.push((AgentState::Done.glyph(), done, "done", AgentState::Done));
    }
    if working > 0 {
        out.push((AgentState::Working.glyph(), working, "working", AgentState::Working));
    }
    out
}

fn rule(buf: &mut Buffer, x: u16, y: u16, w: u16, t: &Theme) {
    let style = Style::default().fg(color(t.ui.border)).bg(color(t.ui.panel_bg));
    for i in 0..w {
        if let Some(c) = buf.cell_mut((x + i, y)) {
            c.set_symbol("─");
            c.set_style(style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{AgentInfo, PaneInfo, Rect, SpaceInfo, TabInfo, ViewState};

    fn agent(state: AgentState) -> AgentInfo {
        AgentInfo {
            kind: "claude".into(),
            name: "builder".into(),
            state,
            elapsed: 138,
            authority: "screen".into(),
            reason: "t".into(),
        }
    }

    fn snap(states: &[Option<AgentState>]) -> Snapshot {
        let panes: Vec<PaneInfo> = states
            .iter()
            .enumerate()
            .map(|(i, s)| PaneInfo {
                id: i as u32 + 1,
                tab: 1,
                space: 1,
                title: format!("pane{}", i + 1),
                cwd: "/tmp".into(),
                cell: Rect::default(),
                content: Rect::default(),
                cols: 80,
                rows: 24,
                agent: s.map(agent),
                exited: false,
                scroll_offset: 0,
                wants_mouse: false,
                bracketed_paste: false,
            })
            .collect();
        let attention = panes
            .iter()
            .filter(|p| p.agent.as_ref().is_some_and(|a| a.state.needs_attention()))
            .count();
        let agent_count = panes.iter().filter(|p| p.agent.is_some()).count();
        Snapshot {
            protocol: 1,
            spaces: vec![SpaceInfo {
                id: 1,
                name: "api-refactor".into(),
                cwd: "/tmp".into(),
                tabs: vec![1],
                focused_tab: Some(1),
                agent_count,
                attention_count: attention,
            }],
            tabs: vec![TabInfo {
                id: 1,
                space: 1,
                name: "1".into(),
                panes: panes.iter().map(|p| p.id).collect(),
                focused_pane: Some(1),
            }],
            panes,
            focused_space: Some(1),
            focused_tab: Some(1),
            focused_pane: Some(1),
            view: ViewState::default(),
            sidebar: Rect::default(),
            bus: Rect::default(),
            status: Rect::default(),
            tabbar: Rect::default(),
        }
    }

    fn render(snap: &Snapshot, w: u16, h: u16) -> (Buffer, Vec<(u16, Hit)>) {
        let area = TRect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        let mut hits = Vec::new();
        Sidebar { snap, theme: &theme, tick: 0, animate: false, hits: &mut hits }
            .render(area, &mut buf);
        (buf, hits)
    }

    fn text(buf: &Buffer, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_header_space_and_agents() {
        let s = snap(&[Some(AgentState::Working), Some(AgentState::Blocked)]);
        let (buf, _) = render(&s, 24, 14);
        let out = text(&buf, 24, 14);
        assert!(out.contains("horde"), "{out}");
        assert!(out.contains("api-refactor"), "{out}");
        assert!(out.contains("builder"), "{out}");
    }

    #[test]
    fn summary_counts_only_non_zero_states() {
        let s = snap(&[Some(AgentState::Blocked), Some(AgentState::Working)]);
        let lines = summary_lines(&s);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].1, 1);
        assert_eq!(lines[0].2, "needs you");
        assert_eq!(lines[1].2, "working");

        // A quiet session shows nothing rather than a row of zeroes.
        let quiet = snap(&[None, None]);
        assert!(summary_lines(&quiet).is_empty());
    }

    #[test]
    fn summary_appears_in_the_rendered_footer() {
        let s = snap(&[Some(AgentState::Blocked)]);
        let (buf, _) = render(&s, 24, 14);
        assert!(text(&buf, 24, 14).contains("needs you"));
    }

    #[test]
    fn hit_rows_map_back_to_spaces_and_panes() {
        let s = snap(&[Some(AgentState::Idle), None]);
        let (_, hits) = render(&s, 24, 14);
        assert!(hits.iter().any(|(_, h)| *h == Hit::Space(1)));
        assert!(hits.iter().any(|(_, h)| *h == Hit::Pane(1)));
        assert!(hits.iter().any(|(_, h)| *h == Hit::Pane(2)));
        // Rows must be distinct or clicks would be ambiguous.
        let ys: Vec<u16> = hits.iter().map(|(y, _)| *y).collect();
        let mut sorted = ys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ys.len(), sorted.len(), "duplicate hit rows: {hits:?}");
    }

    #[test]
    fn long_names_are_truncated_rather_than_wrapped() {
        let mut s = snap(&[Some(AgentState::Idle)]);
        s.spaces[0].name = "an-extremely-long-space-name-that-will-not-fit".into();
        let (buf, _) = render(&s, 20, 12);
        let out = text(&buf, 20, 12);
        // Every rendered line must respect the panel width exactly.
        for line in out.lines() {
            assert!(line.chars().count() <= 20, "line overflows: {line:?}");
        }
        assert!(out.contains('…'), "expected an ellipsis: {out}");
    }

    #[test]
    fn narrow_or_short_areas_render_nothing_rather_than_panicking() {
        let s = snap(&[Some(AgentState::Idle)]);
        let (buf, hits) = render(&s, 6, 10);
        assert_eq!(text(&buf, 6, 10).trim(), "");
        assert!(hits.is_empty());

        let (buf, _) = render(&s, 24, 2);
        assert_eq!(text(&buf, 24, 2).trim(), "");
    }

    #[test]
    fn many_agents_do_not_overrun_the_footer() {
        let states: Vec<Option<AgentState>> =
            (0..40).map(|_| Some(AgentState::Blocked)).collect();
        let s = snap(&states);
        let (buf, _) = render(&s, 24, 12);
        // The summary must still be visible despite the list being far too long.
        assert!(text(&buf, 24, 12).contains("needs you"));
    }
}
