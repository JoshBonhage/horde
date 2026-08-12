//! The left panel: two independent sections.
//!
//! **SPACES** lists projects only — no nested panes. **AGENTS** lists every agent in the
//! session, wherever it lives, with its state. Keeping them apart means the agent list is a
//! single flat thing you can scan rather than something you assemble by reading down a tree,
//! and an agent in another space stays as visible as one in front of you.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::pane_widget::{fmt_elapsed, spinner_frame};
use super::{color, fill, logo, put_line, truncate};
use crate::proto::{AgentState, PaneId, Rgb, Snapshot, SpaceId};
use crate::theme::Theme;

/// A clickable row, so the client can map a mouse position back to what it represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Space(SpaceId),
    Pane(PaneId),
}

/// Space rows to preserve when vertical room runs short.
const MIN_SPACE_ROWS: u16 = 2;

pub struct Sidebar<'a> {
    pub snap: &'a Snapshot,
    /// Open and claimed counts from the task board, when there is any work on it.
    pub board: Option<(usize, usize)>,
    pub theme: &'a Theme,
    pub tick: usize,
    pub animate: bool,
    /// Filled in during render so clicks can be resolved without recomputing layout.
    pub hits: &'a mut Vec<(u16, Hit)>,
}

/// One agent, flattened out of the space/tab tree.
struct AgentRow {
    pane: PaneId,
    name: String,
    state: AgentState,
    elapsed: u64,
    /// False when the agent lives in a space other than the focused one.
    here: bool,
    /// What it has been doing this turn, when hooks are installed to tell us.
    activity: Option<String>,
}

impl Widget for Sidebar<'_> {
    fn render(self, area: TRect, buf: &mut Buffer) {
        if area.width < 8 || area.height < 6 {
            return;
        }
        let t = self.theme;
        fill(buf, area, t.ui.panel_bg);
        self.hits.clear();

        let inner_w = area.width.saturating_sub(2);
        let agents = collect_agents(self.snap);
        let mut summary = summary_lines(self.snap);
        // The board belongs with the other standing counts: it is the same question, "is
        // there anything outstanding". Unclaimed work is the number that matters — once
        // everything is claimed there is nothing for you to hand out, so say so instead.
        if let Some((open, claimed)) = self.board {
            match (open, claimed) {
                (0, 0) => {}
                (0, c) => summary.push(("◇", c, "tasks claimed", AgentState::Unknown)),
                (o, _) => summary.push(("◇", o, "tasks open", AgentState::Unknown)),
            }
        }
        // That horde is allowed to act on its own should never be something you have to
        // remember rather than see. Counted, not a bare word, because "which rules" is the
        // immediate next question and `horde trigger list` is the answer.
        if self.snap.triggers_armed > 0 {
            summary.push(("◈", self.snap.triggers_armed, "triggers armed", AgentState::Working));
        }

        // -- vertical budget, allocated from the bottom up -----------------
        // Footer and the agent list earn their space first; spaces get the remainder,
        // because a project list is short while the agent list is what you actually watch.
        let bottom = area.y + area.height;
        let footer_h = if summary.is_empty() { 0 } else { summary.len() as u16 + 1 };
        let logo_h = logo::height(area.width, area.height);
        let top = area.y + logo_h + 1; // wordmark + rule

        let available = bottom.saturating_sub(top).saturating_sub(footer_h);
        let space_need = self.snap.spaces.len() as u16 + 1; // label + rows
        // rule + label + rows, or just rule + label + "none yet" when empty.
        let agent_rows: u16 =
            agents.iter().map(|a| 1 + u16::from(a.activity.is_some())).sum();
        let agent_need = if agents.is_empty() { 3 } else { agent_rows + 2 };

        let (space_h, agent_h) = if space_need + agent_need <= available {
            (space_need, agent_need)
        } else {
            // Squeeze spaces first, but never below a couple of rows.
            let min_space = (MIN_SPACE_ROWS + 1).min(space_need);
            let for_agents = available.saturating_sub(min_space).min(agent_need);
            (available.saturating_sub(for_agents), for_agents)
        };

        // -- header --------------------------------------------------------
        let mut y = area.y;
        y += logo::draw(buf, area.x, y, area.width, area.height, t);
        rule(buf, area.x, y, area.width, t);
        y += 1;

        // -- SPACES --------------------------------------------------------
        if space_h > 0 {
            let end = y + space_h;
            section_label(buf, area.x + 1, y, inner_w, "SPACES", t);
            y += 1;
            for space in &self.snap.spaces {
                if y >= end {
                    break;
                }
                let focused = self.snap.focused_space == Some(space.id);
                let dot = if space.attention_count > 0 {
                    t.ui.blocked
                } else if focused {
                    t.ui.accent
                } else {
                    t.ui.text_faint
                };
                let badge = if space.agent_count > 0 {
                    space.agent_count.to_string()
                } else {
                    String::new()
                };
                row(
                    buf,
                    area.x,
                    y,
                    area.width,
                    RowSpec {
                        marker: focused,
                        glyph: if space.attention_count > 0 { "●" } else { "○" },
                        glyph_color: dot,
                        label: &space.name,
                        label_color: if focused { t.ui.text } else { t.ui.text_dim },
                        bold: focused,
                        detail: &badge,
                        detail_color: t.ui.text_faint,
                        bg: t.ui.panel_bg,
                    },
                    t,
                );
                self.hits.push((y, Hit::Space(space.id)));
                y += 1;
            }
            y = end;
        }

        // -- AGENTS --------------------------------------------------------
        if agent_h > 0 {
            rule(buf, area.x, y, area.width, t);
            y += 1;
            let end = y + agent_h.saturating_sub(1);
            section_label(buf, area.x + 1, y, inner_w, "AGENTS", t);
            y += 1;

            if agents.is_empty() {
                // Say how to get one rather than leaving an unexplained empty panel.
                if y < end + 1 {
                    put_line(
                        buf,
                        area.x + 2,
                        y,
                        inner_w,
                        Line::from(vec![Span::styled(
                            "none yet",
                            Style::default()
                                .fg(color(t.ui.text_faint))
                                .bg(color(t.ui.panel_bg)),
                        )]),
                    );
                }
            } else {
                // Fit as many agents as their rows allow. An agent with an activity line
                // needs two. The overflow note only costs a line when there is overflow, so
                // try the whole list first and only then make room for the note.
                let room = end.saturating_sub(y) as usize;
                let rows_for = |a: &AgentRow| 1 + usize::from(a.activity.is_some());
                let total: usize = agents.iter().map(rows_for).sum();

                let (visible, hidden) = if total <= room {
                    (&agents[..], 0)
                } else {
                    let budget = room.saturating_sub(1);
                    let mut used = 0usize;
                    let mut fit = 0usize;
                    for a in &agents {
                        let h = rows_for(a);
                        if used + h > budget {
                            break;
                        }
                        used += h;
                        fit += 1;
                    }
                    (&agents[..fit], agents.len() - fit)
                };

                for a in visible {
                    let (glyph, c) = state_look(a.state, t, self.tick, self.animate);
                    let is_focused = self.snap.focused_pane == Some(a.pane);
                    let detail = match a.state {
                        AgentState::Working => fmt_elapsed(a.elapsed),
                        _ => a.state.label().to_string(),
                    };
                    row(
                        buf,
                        area.x,
                        y,
                        area.width,
                        RowSpec {
                            marker: is_focused,
                            glyph: &glyph,
                            glyph_color: c,
                            label: &a.name,
                            // An agent elsewhere in the session reads dimmer, but is still here.
                            label_color: if !a.here {
                                t.ui.text_faint
                            } else if is_focused {
                                t.ui.text
                            } else {
                                t.ui.text_dim
                            },
                            bold: is_focused,
                            detail: &detail,
                            detail_color: c,
                            bg: if is_focused { t.ui.title_bg } else { t.ui.panel_bg },
                        },
                        t,
                    );
                    self.hits.push((y, Hit::Pane(a.pane)));
                    y += 1;

                    // What it is doing, indented under the name. Hooks only — screen
                    // detection cannot see tool calls.
                    if let Some(act) = &a.activity {
                        if y < end {
                            put_line(
                                buf,
                                area.x,
                                y,
                                area.width,
                                Line::from(vec![
                                    Span::styled(
                                        "    ",
                                        Style::default().bg(color(t.ui.panel_bg)),
                                    ),
                                    Span::styled(
                                        truncate(act, inner_w.saturating_sub(4) as usize),
                                        Style::default()
                                            .fg(color(t.ui.text_faint))
                                            .bg(color(t.ui.panel_bg)),
                                    ),
                                ]),
                            );
                            y += 1;
                        }
                    }
                }
                if hidden > 0 {
                    put_line(
                        buf,
                        area.x + 2,
                        y,
                        inner_w,
                        Line::from(vec![Span::styled(
                            format!("+{hidden} more"),
                            Style::default()
                                .fg(color(t.ui.text_faint))
                                .bg(color(t.ui.panel_bg)),
                        )]),
                    );
                }
            }
        }

        // -- footer --------------------------------------------------------
        if footer_h > 0 {
            let mut fy = bottom - footer_h;
            rule(buf, area.x, fy, area.width, t);
            fy += 1;
            for (glyph, count, label, which) in summary {
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
                            Style::default()
                                .fg(color(t.ui.text_dim))
                                .bg(color(t.ui.panel_bg)),
                        ),
                    ]),
                );
                fy += 1;
            }
        }
    }
}

/// Every agent in the session, in space then tab then pane order.
///
/// Stable ordering beats sorting by urgency: rows that jump around under you are worse than
/// rows you have to scan, and colour already carries the urgency.
fn collect_agents(snap: &Snapshot) -> Vec<AgentRow> {
    let mut out = Vec::new();
    for space in &snap.spaces {
        for &tid in &space.tabs {
            let Some(tab) = snap.tabs.iter().find(|t| t.id == tid) else { continue };
            for &pid in &tab.panes {
                let Some(pane) = snap.panes.iter().find(|p| p.id == pid) else { continue };
                let Some(a) = pane.agent.as_ref() else { continue };
                out.push(AgentRow {
                    pane: pid,
                    name: a.name.clone(),
                    state: a.state,
                    elapsed: a.elapsed,
                    here: snap.focused_space == Some(space.id),
                    // Only while it is actually doing something; a finished turn's counts
                    // would be stale trivia.
                    activity: (a.state == AgentState::Working)
                        .then(|| a.activity.summary())
                        .flatten(),
                });
            }
        }
    }
    out
}

fn state_look(state: AgentState, t: &Theme, tick: usize, animate: bool) -> (String, Rgb) {
    let glyph = match state {
        AgentState::Working if animate => spinner_frame(tick).to_string(),
        _ => state.glyph().to_string(),
    };
    let c = match state {
        AgentState::Working => t.ui.working,
        AgentState::Blocked => t.ui.blocked,
        AgentState::Done => t.ui.done,
        AgentState::Idle => t.ui.idle,
        AgentState::Unknown => t.ui.unknown,
    };
    (glyph, c)
}

struct RowSpec<'a> {
    marker: bool,
    glyph: &'a str,
    glyph_color: Rgb,
    label: &'a str,
    label_color: Rgb,
    bold: bool,
    detail: &'a str,
    detail_color: Rgb,
    bg: Rgb,
}

/// One sidebar row: focus marker, glyph, label, right-aligned detail.
fn row(buf: &mut Buffer, x: u16, y: u16, w: u16, spec: RowSpec<'_>, t: &Theme) {
    let inner = w.saturating_sub(2);
    let detail_w = spec.detail.chars().count() as u16;
    // 1 marker + 1 glyph + 1 space, then the label, then the detail flush right.
    let label_room = inner.saturating_sub(3 + detail_w + 1);
    let label = truncate(spec.label, label_room as usize);
    let pad = inner.saturating_sub(3 + label.chars().count() as u16 + detail_w);

    let bg = Style::default().bg(color(spec.bg));
    let mut label_style = bg.fg(color(spec.label_color));
    if spec.bold {
        label_style = label_style.add_modifier(Modifier::BOLD);
    }

    put_line(
        buf,
        x,
        y,
        w,
        Line::from(vec![
            Span::styled(if spec.marker { "▎" } else { " " }, bg.fg(color(t.ui.accent))),
            Span::styled(format!("{} ", spec.glyph), bg.fg(color(spec.glyph_color))),
            Span::styled(label, label_style),
            Span::styled(" ".repeat(pad as usize), bg),
            Span::styled(spec.detail.to_string(), bg.fg(color(spec.detail_color))),
        ]),
    );
}

fn section_label(buf: &mut Buffer, x: u16, y: u16, w: u16, text: &str, t: &Theme) {
    put_line(
        buf,
        x,
        y,
        w,
        Line::from(vec![Span::styled(
            text.to_string(),
            Style::default()
                .fg(color(t.ui.text_faint))
                .bg(color(t.ui.panel_bg))
                .add_modifier(Modifier::BOLD),
        )]),
    );
}

/// Counts worth standing space at the bottom. Only non-zero rows appear, so a quiet session
/// shows nothing rather than three zeroes.
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

    fn pane(id: u32, space: u32, tab: u32, agent: Option<(&str, AgentState)>) -> PaneInfo {
        PaneInfo {
            id,
            tab,
            space,
            title: format!("pane{id}"),
            cwd: "/tmp".into(),
            cell: Rect::default(),
            content: Rect::default(),
            cols: 80,
            rows: 24,
            agent: agent.map(|(n, s)| AgentInfo {
                kind: "claude".into(),
                name: n.into(),
                state: s,
                elapsed: 138,
                authority: "hook".into(),
                reason: "t".into(),
                activity: Default::default(),
            }),
            exited: false,
            scroll_offset: 0,
            wants_mouse: false,
            bracketed_paste: false,
        }
    }

    /// Two spaces: `api-refactor` with builder + reviewer + a shell, `docs` with writer.
    fn snap() -> Snapshot {
        let panes = vec![
            pane(1, 1, 1, Some(("builder", AgentState::Working))),
            pane(2, 1, 1, Some(("reviewer", AgentState::Blocked))),
            pane(3, 1, 1, None),
            pane(4, 2, 2, Some(("writer", AgentState::Idle))),
        ];
        Snapshot {
            protocol: 1,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            spaces: vec![
                SpaceInfo {
                    id: 1,
                    name: "api-refactor".into(),
                    cwd: "/tmp".into(),
                    tabs: vec![1],
                    focused_tab: Some(1),
                    agent_count: 2,
                    attention_count: 1,
                },
                SpaceInfo {
                    id: 2,
                    name: "docs".into(),
                    cwd: "/tmp".into(),
                    tabs: vec![2],
                    focused_tab: Some(2),
                    agent_count: 1,
                    attention_count: 0,
                },
            ],
            tabs: vec![
                TabInfo {
                    id: 1,
                    space: 1,
                    name: "1".into(),
                    panes: vec![1, 2, 3],
                    focused_pane: Some(1),
                },
                TabInfo { id: 2, space: 2, name: "1".into(), panes: vec![4], focused_pane: Some(4) },
            ],
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
            triggers_armed: 0,
        }
    }

    fn render(s: &Snapshot, w: u16, h: u16) -> (String, Vec<(u16, Hit)>) {
        render_board(s, w, h, None)
    }

    fn render_board(
        s: &Snapshot,
        w: u16,
        h: u16,
        board: Option<(usize, usize)>,
    ) -> (String, Vec<(u16, Hit)>) {
        let area = TRect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        let mut hits = Vec::new();
        Sidebar { snap: s, theme: &theme, tick: 0, animate: false, hits: &mut hits, board }
            .render(area, &mut buf);
        let text = (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (text, hits)
    }

    #[test]
    fn spaces_and_agents_are_separate_labelled_sections() {
        let (out, _) = render(&snap(), 24, 20);
        println!("\n{out}\n");
        assert!(out.contains("SPACES"), "{out}");
        assert!(out.contains("AGENTS"), "{out}");
        assert!(
            out.find("SPACES").unwrap() < out.find("AGENTS").unwrap(),
            "AGENTS must sit below SPACES"
        );
    }

    #[test]
    fn spaces_section_lists_only_spaces_never_panes() {
        let (out, _) = render(&snap(), 24, 20);
        let spaces_block = &out[out.find("SPACES").unwrap()..out.find("AGENTS").unwrap()];
        assert!(spaces_block.contains("api-refactor"));
        assert!(spaces_block.contains("docs"));
        // Agent and pane names belong to the other section.
        assert!(!spaces_block.contains("builder"), "{spaces_block}");
        assert!(!spaces_block.contains("pane3"), "{spaces_block}");
    }

    #[test]
    fn agents_section_lists_every_agent_with_its_state() {
        let (out, _) = render(&snap(), 24, 20);
        let agents_block = &out[out.find("AGENTS").unwrap()..];
        for name in ["builder", "reviewer", "writer"] {
            assert!(agents_block.contains(name), "{name} missing from:\n{agents_block}");
        }
        // Each row carries its state: elapsed while working, the label otherwise.
        assert!(agents_block.contains("2m18s"), "{agents_block}");
        assert!(agents_block.contains("blocked"), "{agents_block}");
        assert!(agents_block.contains("idle"), "{agents_block}");
    }

    #[test]
    fn agents_from_other_spaces_still_appear() {
        // `writer` lives in `docs` while `api-refactor` is focused.
        let (out, hits) = render(&snap(), 24, 20);
        assert!(out.contains("writer"));
        assert!(hits.iter().any(|(_, h)| *h == Hit::Pane(4)));
    }

    #[test]
    fn shell_panes_are_not_listed_as_agents() {
        let (out, hits) = render(&snap(), 24, 20);
        assert!(!out.contains("pane3"), "{out}");
        assert!(!hits.iter().any(|(_, h)| *h == Hit::Pane(3)));
    }

    #[test]
    fn hit_rows_are_unique_and_cover_both_sections() {
        let (_, hits) = render(&snap(), 24, 20);
        assert!(hits.iter().any(|(_, h)| *h == Hit::Space(1)));
        assert!(hits.iter().any(|(_, h)| *h == Hit::Pane(1)));
        let ys: Vec<u16> = hits.iter().map(|(y, _)| *y).collect();
        let mut u = ys.clone();
        u.sort_unstable();
        u.dedup();
        assert_eq!(ys.len(), u.len(), "overlapping rows: {hits:?}");
    }

    #[test]
    fn empty_agent_list_says_so_rather_than_showing_a_blank_panel() {
        let mut s = snap();
        s.panes.iter_mut().for_each(|p| p.agent = None);
        s.spaces.iter_mut().for_each(|sp| {
            sp.agent_count = 0;
            sp.attention_count = 0;
        });
        let (out, _) = render(&s, 24, 20);
        assert!(out.contains("AGENTS"), "{out}");
        assert!(out.contains("none yet"), "{out}");
    }

    /// Activity comes only from lifecycle hooks; screen detection cannot see a tool call.
    /// It is shown on a second line under the agent, and only while it is working — a
    /// finished turn's counts would be stale trivia.
    #[test]
    fn a_working_agent_shows_what_it_is_doing_underneath() {
        let mut s = snap();
        for p in s.panes.iter_mut() {
            if let Some(a) = p.agent.as_mut() {
                a.activity = crate::proto::Activity {
                    tools: 12,
                    files: 3,
                    errors: 1,
                    turns: 2,
                    last_tool: Some("Edit".into()),
                };
            }
        }
        let (out, hits) = render(&s, 26, 22);
        println!("\n{out}\n");
        assert!(out.contains("12 tools"), "{out}");
        // Failures outrank the file count in a 20-column gutter: one is actionable.
        assert!(out.contains("1 failed"), "{out}");
        assert!(!out.contains("3 files"), "the file count should yield to the failure: {out}");

        // The detail line is not clickable; only the agent row itself is.
        let pane_hits = hits.iter().filter(|(_, h)| matches!(h, Hit::Pane(_))).count();
        assert_eq!(pane_hits, 3, "one hit row per agent, not per rendered line");

        // Only the working agent gets a line; the blocked and idle ones do not.
        let agents_block = &out[out.find("AGENTS").unwrap()..];
        assert_eq!(agents_block.matches("12 tools").count(), 1, "{agents_block}");
    }

    #[test]
    fn the_activity_summary_shows_files_when_nothing_failed() {
        use crate::proto::Activity;
        let a = Activity { tools: 9, files: 2, errors: 0, turns: 1, last_tool: None };
        assert_eq!(a.summary().as_deref(), Some("9 tools · 2 files"));
        let a = Activity { tools: 9, files: 2, errors: 3, turns: 1, last_tool: None };
        assert_eq!(a.summary().as_deref(), Some("9 tools · 3 failed"));
        let a = Activity { tools: 4, files: 0, errors: 0, turns: 1, last_tool: None };
        assert_eq!(a.summary().as_deref(), Some("4 tools"));
        // Nothing recorded means no line at all, rather than a row of zeroes.
        assert_eq!(Activity::default().summary(), None);
    }

    #[test]
    fn footer_summary_survives_a_long_agent_list() {
        let mut s = snap();
        let panes: Vec<PaneInfo> =
            (0..40u32).map(|i| pane(100 + i, 1, 1, Some(("a", AgentState::Blocked)))).collect();
        s.tabs[0].panes = panes.iter().map(|p| p.id).collect();
        s.tabs[1].panes = vec![];
        s.panes = panes;
        let (out, _) = render(&s, 24, 20);
        assert!(out.contains("needs you"), "footer must survive:\n{out}");
        // And the overflow is stated rather than silently dropped.
        assert!(out.contains("more"), "{out}");
    }

    #[test]
    fn nothing_ever_writes_past_the_panel_width() {
        let mut s = snap();
        s.spaces[0].name = "an-absurdly-long-space-name-that-cannot-fit".into();
        for w in [10u16, 14, 18, 24, 40] {
            let (out, _) = render(&s, w, 20);
            for line in out.lines() {
                assert_eq!(line.chars().count(), w as usize, "width {w}: {line:?}");
            }
        }
    }

    #[test]
    fn tiny_areas_render_nothing_rather_than_panicking() {
        let (out, hits) = render(&snap(), 6, 20);
        assert_eq!(out.trim(), "");
        assert!(hits.is_empty());
        let (out, _) = render(&snap(), 24, 4);
        assert_eq!(out.trim(), "");
    }

    #[test]
    fn summary_counts_only_non_zero_states() {
        let lines = summary_lines(&snap());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].2, "needs you");
        assert_eq!(lines[1].2, "working");
    }

    #[test]
    fn outstanding_board_work_appears_in_the_footer() {
        let (out, _) = render_board(&snap(), 26, 22, Some((3, 1)));
        assert!(out.contains("3 tasks open"), "unclaimed work is the headline:\n{out}");
    }

    /// That horde may act on its own has to be visible, not remembered.
    #[test]
    fn armed_triggers_show_in_the_footer() {
        let mut s = snap();
        s.triggers_armed = 2;
        let (out, _) = render_board(&s, 26, 22, None);
        assert!(out.contains("2 triggers armed"), "{out}");
    }

    /// And with the master switch off the daemon reports zero, so the row costs nothing.
    #[test]
    fn a_disarmed_session_shows_no_trigger_row() {
        let (out, _) = render(&snap(), 26, 22);
        assert!(!out.contains("armed"), "{out}");
    }

    #[test]
    fn a_fully_claimed_board_says_claimed_rather_than_zero_open() {
        // "0 tasks open" would read as an empty board when three agents are mid-task.
        let (out, _) = render_board(&snap(), 26, 22, Some((0, 3)));
        assert!(out.contains("3 tasks claimed"), "{out}");
        assert!(!out.contains("0 tasks"), "{out}");
    }

    #[test]
    fn an_empty_board_costs_no_footer_line() {
        let (with, _) = render_board(&snap(), 26, 22, Some((0, 0)));
        let (without, _) = render_board(&snap(), 26, 22, None);
        assert_eq!(with, without);
        assert!(!with.contains("task"), "{with}");
    }
}
