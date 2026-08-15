//! The start screen: what horde shows you before it shows you a terminal.
//!
//! An editor's greeter, except every line of it is live session state rather than a menu.
//! The wordmark is the only decoration; under it sit the agents waiting on you, the projects
//! you have open, and the ones you had open last time.
//!
//! It replaces the panes rather than floating over them, which makes it the one view in
//! horde that is not an overlay — see [`super::draw`]. Everything else follows the roster's
//! shape: a pure content function that can be asserted as text, a hit list recorded while
//! drawing so the mouse resolves to the same rows the keyboard walks.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{color, fill, logo, put_line, truncate};
use crate::proto::{AgentState, PaneId, RecentProject, Snapshot, SpaceId};
use crate::theme::Theme;

/// One line of the dashboard. Headers and hints are scenery; the rest can be selected.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header(String),
    /// An agent that is blocked or finished while you were away.
    Attention { pane: PaneId, name: String, state: AgentState, waited: String },
    /// A project with a space open on it right now.
    Live { space: SpaceId, name: String, accent: u8, facts: String, cwd: String },
    /// A project this session has opened before, not open now.
    Recent { cwd: String, name: String, when: String },
    Hint(String),
}

impl Row {
    /// Whether the cursor can land here. Headers and hints cannot be chosen.
    pub fn selectable(&self) -> bool {
        matches!(self, Row::Attention { .. } | Row::Live { .. } | Row::Recent { .. })
    }
}

/// How long ago, in the roughest useful units. "3d" beats a timestamp on a screen you are
/// scanning rather than reading.
fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// The dashboard's content, in order.
///
/// Pure so it can be asserted as strings: what the start screen *says* about a session is
/// worth a test, and standing up a terminal to find out is not.
pub fn rows(snap: &Snapshot, now: u64) -> Vec<Row> {
    let mut out = Vec::new();

    // 1. Anything waiting on a human comes first, because it is the only part of this screen
    //    that is costing you something while you read it.
    let mut waiting: Vec<&crate::proto::PaneInfo> = snap
        .panes
        .iter()
        .filter(|p| p.agent.as_ref().is_some_and(|a| a.state.needs_attention()))
        .collect();
    waiting.sort_by_key(|p| std::cmp::Reverse(p.agent.as_ref().map(|a| a.elapsed).unwrap_or(0)));
    if !waiting.is_empty() {
        out.push(Row::Header("needs you".into()));
        for p in waiting {
            let Some(a) = p.agent.as_ref() else { continue };
            let space = snap
                .spaces
                .iter()
                .find(|s| s.id == p.space)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            out.push(Row::Attention {
                pane: p.id,
                name: format!("{} · {}", a.name, space),
                state: a.state,
                waited: super::pane_widget::fmt_elapsed(a.elapsed),
            });
        }
    }

    // 2. Projects on screen now.
    if !snap.spaces.is_empty() {
        out.push(Row::Header("projects".into()));
        for s in &snap.spaces {
            let mut facts = Vec::new();
            if let Some(r) = &s.repo {
                facts.push(if r.dirty { format!("{}*", r.branch) } else { r.branch.clone() });
            }
            match s.agent_count {
                0 => {}
                1 => facts.push("1 agent".into()),
                n => facts.push(format!("{n} agents")),
            }
            if s.attention_count > 0 {
                facts.push(format!("◍{}", s.attention_count));
            }
            out.push(Row::Live {
                space: s.id,
                name: s.name.clone(),
                accent: s.accent,
                facts: facts.join("  "),
                cwd: s.cwd.clone(),
            });
        }
    }

    // 3. Projects from before, minus the ones already listed above.
    let cold: Vec<&RecentProject> = snap.recents.iter().filter(|r| !r.live).collect();
    if !cold.is_empty() {
        out.push(Row::Header("recent".into()));
        for r in cold {
            out.push(Row::Recent {
                cwd: r.cwd.clone(),
                name: r.name.clone(),
                when: ago(now, r.last_used),
            });
        }
    }

    // Two halves of the system, named as such. The multiplexer is one of them, not the
    // thing the other lives inside.
    out.push(Row::Hint("enter opens a project's files   w write a note   N notes".into()));
    out.push(Row::Hint("p projects   n new project   o roster   D digest   . settings".into()));
    out.push(Row::Hint("esc to the terminal   q detach".into()));
    out
}

/// Indices of the rows a cursor may sit on.
pub fn selectable(rows: &[Row]) -> Vec<usize> {
    rows.iter().enumerate().filter(|(_, r)| r.selectable()).map(|(i, _)| i).collect()
}

/// Draw the whole screen. Returns the hit list: `(y, row index)` for mouse resolution.
pub fn draw(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    rows_in: &[Row],
    sel: usize,
) -> Vec<(u16, usize)> {
    fill(buf, area, theme.ui.bg);
    let mut hits = Vec::new();
    if area.height < 6 || area.width < 30 {
        return hits;
    }

    // Content sits in a centred column so a wide terminal does not stretch the list across
    // two feet of desk.
    let w = area.width.min(72);
    let x = area.x + (area.width - w) / 2;

    // Centre the whole block vertically. A greeter pinned to the top of a tall terminal
    // leaves a third of the screen empty below it and reads as a page that failed to load.
    let body = rows_in.len() as u16 + 2;
    let banner_room = area.height.saturating_sub(body);
    let banner_h = if banner_room >= 4 { logo::height(w, banner_room) + 2 } else { 0 };
    let total = body + banner_h;
    let mut y = area.y + area.height.saturating_sub(total) / 2;
    if banner_h > 0 {
        y += logo::draw(buf, x, y, w, banner_room, theme);
        put_line(
            buf,
            x,
            y,
            w,
            Line::from(Span::styled(
                format!("{:^w$}", "the terminal your agents live in", w = w as usize),
                Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
            )),
        );
        y += 2;
    }

    let sel_rows = selectable(rows_in);
    let cursor_row = sel_rows.get(sel).copied();

    for (i, row) in rows_in.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let selected = cursor_row == Some(i);
        let bg = if selected { theme.ui.selection } else { theme.ui.bg };
        let base = Style::default().bg(color(bg));

        let spans: Vec<Span<'static>> = match row {
            Row::Header(t) => vec![Span::styled(
                format!("{} ", t.to_uppercase()),
                Style::default()
                    .fg(color(theme.ui.text_faint))
                    .bg(color(theme.ui.bg))
                    .add_modifier(Modifier::BOLD),
            )],
            Row::Attention { name, state, waited, .. } => vec![
                Span::styled(format!("  {} ", state.glyph()), base.fg(color(theme.ui.blocked))),
                Span::styled(truncate(name, 34), base.fg(color(theme.ui.text))),
                Span::styled(format!("  waiting {waited}"), base.fg(color(theme.ui.text_dim))),
            ],
            Row::Live { name, accent, facts, cwd, .. } => vec![
                Span::styled("  ● ".to_string(), base.fg(color(theme.space_accent(*accent)))),
                Span::styled(format!("{:<16}", truncate(name, 16)), base.fg(color(theme.ui.text))),
                Span::styled(format!("{facts:<22}"), base.fg(color(theme.ui.text_dim))),
                Span::styled(
                    super::statusbar::shorten_home(cwd),
                    base.fg(color(theme.ui.text_faint)),
                ),
            ],
            // A hollow mark and "resume" rather than a count: the row says what enter will
            // do, so nothing is created by a keystroke that looked like navigation.
            Row::Recent { name, when, cwd } => vec![
                Span::styled("  ○ ".to_string(), base.fg(color(theme.ui.text_faint))),
                Span::styled(format!("{:<16}", truncate(name, 16)), base.fg(color(theme.ui.text_dim))),
                Span::styled(format!("{:<10}", "resume"), base.fg(color(theme.ui.text_faint))),
                Span::styled(format!("{when:<11}"), base.fg(color(theme.ui.text_faint))),
                Span::styled(
                    super::statusbar::shorten_home(cwd),
                    base.fg(color(theme.ui.text_faint)),
                ),
            ],
            Row::Hint(t) => vec![Span::styled(
                format!("  {t}"),
                Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
            )],
        };

        if selected {
            fill(buf, TRect { x, y, width: w, height: 1 }, theme.ui.selection);
        }
        put_line(buf, x, y, w, Line::from(spans));
        if row.selectable() {
            hits.push((y, i));
        }
        y += 1;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with two projects, one blocked agent, and one project remembered but closed.
    fn snap() -> Snapshot {
        let mut s = crate::client::roster::tests::snap();
        s.recents = vec![
            RecentProject { name: "horde".into(), cwd: "/home/j/dev/horde".into(), last_used: 0, live: true },
            RecentProject { name: "blog".into(), cwd: "/home/j/dev/blog".into(), last_used: 0, live: false },
        ];
        s
    }

    /// Whatever is waiting on a human goes first. It is the only part of this screen that
    /// costs you something while you read the rest of it.
    #[test]
    fn the_start_screen_leads_with_what_is_waiting_on_you() {
        let rows = rows(&snap(), 0);
        let first = rows.iter().find(|r| matches!(r, Row::Header(_)));
        assert_eq!(first, Some(&Row::Header("needs you".into())), "{rows:?}");
        assert!(
            matches!(rows.get(1), Some(Row::Attention { .. })),
            "the header is followed by the agent itself: {rows:?}"
        );
    }

    /// A project already on screen is offered as somewhere to *go*, never as something to
    /// reopen — otherwise enter on the wrong row starts a second copy of what you are using.
    #[test]
    fn a_project_already_open_is_never_listed_as_one_to_resume() {
        let rows = rows(&snap(), 0);
        let resumable: Vec<&Row> = rows.iter().filter(|r| matches!(r, Row::Recent { .. })).collect();
        assert_eq!(resumable.len(), 1, "only the closed one: {resumable:?}");
        assert!(
            matches!(resumable[0], Row::Recent { name, .. } if name == "blog"),
            "{resumable:?}"
        );
    }

    /// The cursor walks only rows that do something, so `j` never parks on a heading and
    /// enter never has to decide what a heading means.
    #[test]
    fn the_cursor_lands_only_on_rows_that_do_something() {
        let rows = rows(&snap(), 0);
        for i in selectable(&rows) {
            assert!(rows[i].selectable(), "row {i} is not actionable: {:?}", rows[i]);
        }
        assert!(rows.iter().any(|r| r.selectable()), "there is something to select");
        assert!(
            !rows.iter().filter(|r| r.selectable()).any(|r| matches!(r, Row::Header(_) | Row::Hint(_))),
            "no heading or hint is ever selectable"
        );
    }

    /// The greeter is centred and every section is present and in order. Pinned as text
    /// because "does the start screen still say what it is for" is worth knowing on a diff.
    #[test]
    fn the_greeter_draws_centred_with_its_sections_in_order() {
        let s = snap();
        let rows = rows(&s, 3 * 86_400_000);
        let area = TRect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), &rows, 0);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();

        let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle}:\n{text}"));
        assert!(at("NEEDS YOU") < at("PROJECTS"), "waiting agents come first");
        assert!(at("PROJECTS") < at("RECENT"), "open projects before closed ones");
        assert!(text.contains("resume"), "a closed project says what enter will do");
        assert!(text.contains("3d ago"), "and when it was last open");

        let painted: Vec<u16> = (0..area.height)
            .filter(|y| (0..area.width).any(|x| buf[(x, *y)].symbol() != " "))
            .collect();
        let (top, bottom) = (painted[0], area.height - 1 - painted[painted.len() - 1]);
        assert!(top.abs_diff(bottom) <= 2, "vertically centred, got {top} above and {bottom} below");
    }
}
