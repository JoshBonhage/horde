//! The top tab bar and the bottom status bar.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::{color, fill, put_line, truncate};
use crate::client::Mode;
use crate::proto::{AgentState, Snapshot};
use crate::theme::Theme;

/// Space name on the left, its tabs after it, agent counts on the right.
pub struct TabBar<'a> {
    pub snap: &'a Snapshot,
    pub theme: &'a Theme,
}

impl Widget for TabBar<'_> {
    fn render(self, area: TRect, buf: &mut Buffer) {
        if area.width < 10 || area.height == 0 {
            return;
        }
        let t = self.theme;
        fill(buf, area, t.ui.panel_bg);
        let panel = Style::default().bg(color(t.ui.panel_bg));

        let mut spans: Vec<Span<'static>> = Vec::new();
        if let Some(space) = self.snap.spaces.iter().find(|s| Some(s.id) == self.snap.focused_space)
        {
            spans.push(Span::styled(
                format!(" {} ", truncate(&space.name, 24)),
                panel.fg(color(t.ui.accent)).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("› ".to_string(), panel.fg(color(t.ui.text_faint))));

            for (i, &tid) in space.tabs.iter().enumerate() {
                let Some(tab) = self.snap.tabs.iter().find(|t| t.id == tid) else { continue };
                let active = self.snap.focused_tab == Some(tid);

                // A tab holding something that needs attention is marked, so you can see it
                // without switching to that tab.
                let attention = tab.panes.iter().any(|p| {
                    self.snap
                        .panes
                        .iter()
                        .find(|x| x.id == *p)
                        .and_then(|x| x.agent.as_ref())
                        .is_some_and(|a| a.state.needs_attention())
                });

                let label = format!(" {} {} ", i + 1, truncate(&tab.name, 16));
                let style = if active {
                    Style::default()
                        .bg(color(t.ui.title_bg))
                        .fg(color(t.ui.text))
                        .add_modifier(Modifier::BOLD)
                } else {
                    panel.fg(color(t.ui.text_dim))
                };
                spans.push(Span::styled(label, style));
                if attention {
                    spans.push(Span::styled(
                        "◍".to_string(),
                        if active {
                            Style::default().bg(color(t.ui.title_bg)).fg(color(t.ui.blocked))
                        } else {
                            panel.fg(color(t.ui.blocked))
                        },
                    ));
                }
                spans.push(Span::styled(" ".to_string(), panel));
            }
        }

        put_line(buf, area.x, area.y, area.width, Line::from(spans));

        // Right-aligned totals across the whole session, not just this space.
        let counts = state_counts(self.snap);
        let mut right: Vec<Span<'static>> = Vec::new();
        for (state, n) in counts {
            if n == 0 {
                continue;
            }
            let c = match state {
                AgentState::Blocked => t.ui.blocked,
                AgentState::Working => t.ui.working,
                AgentState::Done => t.ui.done,
                _ => t.ui.idle,
            };
            right.push(Span::styled(format!("{}{} ", state.glyph(), n), panel.fg(color(c))));
        }
        let rw: usize = right.iter().map(|s| s.content.chars().count()).sum();
        if rw + 2 < area.width as usize {
            let x = area.x + area.width - rw as u16 - 1;
            put_line(buf, x, area.y, rw as u16 + 1, Line::from(right));
        }
    }
}

fn state_counts(snap: &Snapshot) -> [(AgentState, usize); 4] {
    let mut blocked = 0;
    let mut done = 0;
    let mut working = 0;
    let mut idle = 0;
    for p in &snap.panes {
        match p.agent.as_ref().map(|a| a.state) {
            Some(AgentState::Blocked) => blocked += 1,
            Some(AgentState::Done) => done += 1,
            Some(AgentState::Working) => working += 1,
            Some(_) => idle += 1,
            None => {}
        }
    }
    // Ordered by urgency so the eye lands on what matters first.
    [
        (AgentState::Blocked, blocked),
        (AgentState::Working, working),
        (AgentState::Done, done),
        (AgentState::Idle, idle),
    ]
}

pub struct StatusBar<'a> {
    pub snap: &'a Snapshot,
    pub theme: &'a Theme,
    pub mode: Mode,
    pub prefix: String,
}

impl Widget for StatusBar<'_> {
    fn render(self, area: TRect, buf: &mut Buffer) {
        if area.width < 10 || area.height == 0 {
            return;
        }
        let t = self.theme;
        fill(buf, area, t.ui.panel_bg);
        let panel = Style::default().bg(color(t.ui.panel_bg));

        let mut left: Vec<Span<'static>> = Vec::new();

        // The mode chip is the most important thing on this bar: without it there is no
        // feedback that the prefix key registered.
        match &self.mode {
            Mode::Prefix => left.push(Span::styled(
                " PREFIX ".to_string(),
                Style::default()
                    .bg(color(t.ui.accent))
                    .fg(color(t.ui.bg))
                    .add_modifier(Modifier::BOLD),
            )),
            Mode::Help => left.push(chip(" HELP ", t.ui.accent_alt, t)),
            Mode::Palette { .. } => left.push(chip(" COMMAND ", t.ui.accent_alt, t)),
            Mode::SpaceSwitcher { .. } => left.push(chip(" SPACE ", t.ui.accent_alt, t)),
            Mode::Prompt { .. } => left.push(chip(" INPUT ", t.ui.accent_alt, t)),
            Mode::Menu { .. } => left.push(chip(" MENU ", t.ui.accent_alt, t)),
            Mode::Settings { .. } => left.push(chip(" SETTINGS ", t.ui.accent_alt, t)),
            Mode::Terminal => left.push(Span::styled(
                format!(" {} ", self.prefix),
                panel.fg(color(t.ui.text_faint)),
            )),
        }

        left.push(Span::styled(" ".to_string(), panel));

        let pane_count = self
            .snap
            .focused_tab
            .and_then(|tid| self.snap.tabs.iter().find(|t| t.id == tid))
            .map(|t| t.panes.len())
            .unwrap_or(0);
        let agents = self.snap.panes.iter().filter(|p| p.agent.is_some()).count();
        let attention = self
            .snap
            .panes
            .iter()
            .filter(|p| p.agent.as_ref().is_some_and(|a| a.state.needs_attention()))
            .count();

        let mut summary = format!("{pane_count} panes");
        if agents > 0 {
            summary.push_str(&format!(" · {agents} agents"));
        }
        left.push(Span::styled(summary, panel.fg(color(t.ui.text_dim))));

        if attention > 0 {
            left.push(Span::styled(
                format!(" · {attention} needs you"),
                panel.fg(color(t.ui.blocked)).add_modifier(Modifier::BOLD),
            ));
        }
        if self.mode == Mode::Prefix {
            left.push(Span::styled(
                "   ? help  | split  - stack  hjkl move  z zoom  d detach".to_string(),
                panel.fg(color(t.ui.text_faint)),
            ));
        }
        put_line(buf, area.x, area.y, area.width, Line::from(left));

        // Right: the focused pane's directory, shortened with ~.
        if let Some(pane) =
            self.snap.panes.iter().find(|p| Some(p.id) == self.snap.focused_pane)
        {
            let cwd = shorten_home(&pane.cwd);
            let cwd = truncate(&cwd, (area.width / 3) as usize);
            let w = cwd.chars().count() as u16;
            if w + 4 < area.width {
                put_line(
                    buf,
                    area.x + area.width - w - 1,
                    area.y,
                    w + 1,
                    Line::from(vec![Span::styled(cwd, panel.fg(color(t.ui.text_faint)))]),
                );
            }
        }
    }
}

fn chip(text: &str, bg: crate::proto::Rgb, t: &Theme) -> Span<'static> {
    Span::styled(
        text.to_string(),
        Style::default().bg(color(bg)).fg(color(t.ui.bg)).add_modifier(Modifier::BOLD),
    )
}

fn shorten_home(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) => {
            let h = home.to_string_lossy();
            match path.strip_prefix(h.as_ref()) {
                Some(rest) => format!("~{rest}"),
                None => path.to_string(),
            }
        }
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{AgentInfo, PaneInfo, Rect, SpaceInfo, TabInfo, ViewState};

    fn snap(states: &[Option<AgentState>], tabs: usize) -> Snapshot {
        let panes: Vec<PaneInfo> = states
            .iter()
            .enumerate()
            .map(|(i, s)| PaneInfo {
                id: i as u32 + 1,
                tab: 1,
                space: 1,
                title: "p".into(),
                cwd: "/tmp/project".into(),
                cell: Rect::default(),
                content: Rect::default(),
                cols: 80,
                rows: 24,
                agent: s.map(|st| AgentInfo {
                    kind: "claude".into(),
                    name: "builder".into(),
                    state: st,
                    elapsed: 5,
                    authority: "screen".into(),
                    reason: "t".into(),
                    activity: Default::default(),
                }),
                exited: false,
                scroll_offset: 0,
                wants_mouse: false,
                bracketed_paste: false,
            })
            .collect();
        let tab_list: Vec<TabInfo> = (1..=tabs)
            .map(|i| TabInfo {
                id: i as u32,
                space: 1,
                name: format!("tab{i}"),
                panes: if i == 1 { panes.iter().map(|p| p.id).collect() } else { vec![] },
                focused_pane: Some(1),
            })
            .collect();
        Snapshot {
            protocol: 1,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            spaces: vec![SpaceInfo {
                id: 1,
                name: "api".into(),
                cwd: "/tmp/project".into(),
                tabs: tab_list.iter().map(|t| t.id).collect(),
                focused_tab: Some(1),
                agent_count: panes.iter().filter(|p| p.agent.is_some()).count(),
                attention_count: 0,
            }],
            tabs: tab_list,
            panes,
            focused_space: Some(1),
            focused_tab: Some(1),
            focused_pane: Some(1),
            view: ViewState::default(),
            sidebar: Rect::default(),
            bus: Rect::default(),
            status: Rect::default(),
            tabbar: Rect::default(),
            tasks_open: 0,
            tasks_claimed: 0,
        }
    }

    fn text(buf: &Buffer, w: u16) -> String {
        (0..w).map(|x| buf.cell((x, 0)).unwrap().symbol()).collect()
    }

    fn render_tabs(snap: &Snapshot, w: u16) -> Buffer {
        let area = TRect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        TabBar { snap, theme: &theme }.render(area, &mut buf);
        buf
    }

    fn render_status(snap: &Snapshot, mode: Mode, w: u16) -> Buffer {
        let area = TRect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        StatusBar { snap, theme: &theme, mode, prefix: "ctrl+b".into() }.render(area, &mut buf);
        buf
    }

    #[test]
    fn tab_bar_shows_space_and_tabs() {
        let s = snap(&[Some(AgentState::Working)], 3);
        let out = text(&render_tabs(&s, 80), 80);
        assert!(out.contains("api"), "{out}");
        assert!(out.contains("1 tab1"), "{out}");
        assert!(out.contains("3 tab3"), "{out}");
    }

    #[test]
    fn tab_bar_shows_right_aligned_state_counts() {
        let s = snap(&[Some(AgentState::Blocked), Some(AgentState::Working)], 1);
        let out = text(&render_tabs(&s, 80), 80);
        assert!(out.contains("◍1"), "{out}");
        assert!(out.contains("◐1"), "{out}");
    }

    #[test]
    fn state_counts_are_ordered_by_urgency() {
        let s = snap(
            &[
                Some(AgentState::Idle),
                Some(AgentState::Done),
                Some(AgentState::Working),
                Some(AgentState::Blocked),
            ],
            1,
        );
        let counts = state_counts(&s);
        assert_eq!(counts[0].0, AgentState::Blocked);
        assert_eq!(counts[1].0, AgentState::Working);
        assert!(counts.iter().all(|(_, n)| *n == 1));
    }

    #[test]
    fn prefix_mode_is_unmistakable_in_the_status_bar() {
        let s = snap(&[None], 1);
        let out = text(&render_status(&s, Mode::Prefix, 100), 100);
        assert!(out.contains("PREFIX"), "{out}");
        // And it hints at what to press next.
        assert!(out.contains("z zoom"), "{out}");
    }

    #[test]
    fn terminal_mode_shows_the_prefix_key_rather_than_a_chip() {
        let s = snap(&[None], 1);
        let out = text(&render_status(&s, Mode::Terminal, 100), 100);
        assert!(out.contains("ctrl+b"), "{out}");
        assert!(!out.contains("PREFIX"), "{out}");
    }

    #[test]
    fn status_bar_counts_panes_and_agents() {
        let s = snap(&[Some(AgentState::Idle), None, Some(AgentState::Working)], 1);
        let out = text(&render_status(&s, Mode::Terminal, 100), 100);
        assert!(out.contains("3 panes"), "{out}");
        assert!(out.contains("2 agents"), "{out}");
    }

    #[test]
    fn attention_is_called_out_in_words() {
        let s = snap(&[Some(AgentState::Blocked)], 1);
        let out = text(&render_status(&s, Mode::Terminal, 100), 100);
        assert!(out.contains("1 needs you"), "{out}");
    }

    #[test]
    fn cwd_is_shown_shortened() {
        let s = snap(&[None], 1);
        let out = text(&render_status(&s, Mode::Terminal, 100), 100);
        assert!(out.contains("project"), "{out}");
    }

    #[test]
    fn narrow_bars_render_nothing_rather_than_panicking() {
        let s = snap(&[Some(AgentState::Blocked)], 4);
        assert_eq!(text(&render_tabs(&s, 8), 8).trim(), "");
        assert_eq!(text(&render_status(&s, Mode::Prefix, 8), 8).trim(), "");
    }

    #[test]
    fn bars_never_write_past_their_width() {
        let s = snap(&[Some(AgentState::Blocked); 6], 8);
        for w in [12u16, 20, 40, 60, 80, 200] {
            let out = text(&render_tabs(&s, w), w);
            assert_eq!(out.chars().count(), w as usize, "tab bar at {w}");
            let out = text(&render_status(&s, Mode::Prefix, w), w);
            assert_eq!(out.chars().count(), w as usize, "status bar at {w}");
        }
    }

    #[test]
    fn shorten_home_replaces_the_home_prefix() {
        if let Some(home) = dirs::home_dir() {
            let p = format!("{}/dev/horde", home.to_string_lossy());
            assert_eq!(shorten_home(&p), "~/dev/horde");
        }
        assert_eq!(shorten_home("/etc/hosts"), "/etc/hosts");
    }
}
