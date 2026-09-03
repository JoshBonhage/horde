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

/// One line of the dashboard. Headers are scenery; the rest can be selected.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header(String),
    /// An agent that is blocked or finished while you were away.
    Attention { pane: PaneId, name: String, state: AgentState, waited: String },
    /// A project with a space open on it right now.
    Live { space: SpaceId, name: String, accent: u8, facts: String, cwd: String },
    /// A project this session has opened before, not open now.
    Recent { cwd: String, name: String, when: String },
    /// Something the start screen can do, rather than somewhere it can take you.
    Action(Act),
}

impl Row {
    /// Whether the cursor can land here. Headers cannot be chosen.
    pub fn selectable(&self) -> bool {
        matches!(
            self,
            Row::Attention { .. } | Row::Live { .. } | Row::Recent { .. } | Row::Action(_)
        )
    }
}

/// The menu at the foot of the start screen.
///
/// Three to a line with the key beside each, wrapping onto as many lines as it takes — a
/// block you can take in at a glance, rather than the run of keys set as prose that this
/// replaced. Each entry is walkable with the cursor as well as typeable, so the menu teaches
/// the key while staying usable before you have learnt it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Projects,
    NewProject,
    WriteNote,
    Notes,
    Vault,
    Kanban,
    Roster,
    Digest,
    Settings,
    Keys,
    Terminal,
    Detach,
}

impl Act {
    /// In menu order: the note side and the multiplexer first, then the ways out.
    pub fn all() -> [Act; 12] {
        [
            Act::Projects,
            Act::NewProject,
            Act::WriteNote,
            Act::Notes,
            Act::Vault,
            Act::Kanban,
            Act::Roster,
            Act::Digest,
            Act::Settings,
            Act::Keys,
            Act::Terminal,
            Act::Detach,
        ]
    }

    /// The key that runs it without walking the list.
    pub fn key(&self) -> &'static str {
        match self {
            Act::Projects => "p",
            Act::NewProject => "n",
            Act::WriteNote => "w",
            Act::Notes => "N",
            Act::Vault => "V",
            // The same key the board answers to under the prefix, so the menu teaches the
            // shortcut rather than inventing a second one for the same place.
            Act::Kanban => "T",
            Act::Roster => "o",
            Act::Digest => "D",
            Act::Settings => ".",
            Act::Keys => "?",
            Act::Terminal => "esc",
            Act::Detach => "q",
        }
    }

    /// The typed key that reaches this entry, if there is one. Keeps the list and the key
    /// handler from drifting apart: the menu is the source of both.
    pub fn from_key(c: char) -> Option<Act> {
        Act::all().into_iter().find(|a| a.key().chars().eq(std::iter::once(c)))
    }

    /// Short by necessity: three of these share a line, so a label that needs a sentence
    /// would either truncate or push the columns apart.
    pub fn label(&self) -> &'static str {
        match self {
            Act::Projects => "Switch project",
            Act::NewProject => "New project",
            Act::WriteNote => "Write a note",
            Act::Notes => "Browse notes",
            Act::Vault => "Vault",
            // "Kanban", not "Tasks": the *task board* is the one agents pull work from, and
            // this is the other one. docs/kanban.md opens by separating them and the menu is
            // not the place to put them back together.
            Act::Kanban => "Kanban",
            Act::Roster => "Agent roster",
            Act::Digest => "Catch-up digest",
            Act::Settings => "Settings",
            Act::Keys => "Keys",
            Act::Terminal => "Terminal",
            Act::Detach => "Detach",
        }
    }
}

/// How many entries share a line.
///
/// Three, because the menu is a block you take in at a glance rather than a list you read:
/// ten single-file lines under the session made the screen mostly menu. Whatever does not
/// fit on the first line wraps to the next, so the shape holds however many entries there
/// come to be.
pub const MENU_COLS: usize = 3;

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

    // Everything above this is somewhere to go; everything below is something to do.
    out.push(Row::Header("actions".into()));
    out.extend(Act::all().into_iter().map(Row::Action));
    out
}

/// Where the menu starts: the header above the first action, or the end of the list.
pub fn menu_start(rows: &[Row]) -> usize {
    rows.iter()
        .position(|r| matches!(r, Row::Action(_)))
        .map(|i| i.saturating_sub(1))
        .unwrap_or(rows.len())
}

/// Lines the menu takes on screen: its header, plus one per row of [`MENU_COLS`] entries.
fn menu_lines(rows: &[Row]) -> usize {
    let n = rows.iter().filter(|r| matches!(r, Row::Action(_))).count();
    if n == 0 { 0 } else { 1 + n.div_ceil(MENU_COLS) }
}

/// Which of the rows above the menu are shown, on a screen too short for all of them.
///
/// The menu itself is never dropped — it is the part of this screen you came here to use,
/// and a menu that falls off the bottom when a session gets busy is a menu you cannot rely
/// on. The listing above it scrolls instead, keeping the cursor in view.
pub fn window(rows: &[Row], height: u16, cursor: Option<usize>) -> std::ops::Range<usize> {
    let start = menu_start(rows);
    let room = (height as usize).saturating_sub(menu_lines(rows));
    if start <= room {
        return 0..start;
    }
    // Scroll only as far as the cursor demands, so the top of the listing stays put while
    // it can.
    let first = match cursor {
        // A cursor down in the menu says nothing about where the listing should sit.
        Some(c) if c >= room && c < start => (c + 1 - room).min(start - room),
        _ => 0,
    };
    first..(first + room).min(start)
}

/// Indices of the rows a cursor may sit on.
pub fn selectable(rows: &[Row]) -> Vec<usize> {
    rows.iter().enumerate().filter(|(_, r)| r.selectable()).map(|(i, _)| i).collect()
}

/// Move the cursor, in cursor positions rather than rows.
///
/// The session above is a column and the menu below is a grid, so the same keypress means
/// different arithmetic depending on where the cursor is: down the listing is one step, down
/// the menu is a whole line of it. Getting that wrong is what makes a grid feel like a list
/// someone rearranged, so it lives here, next to the layout it has to agree with.
pub fn move_sel(rows: &[Row], sel: usize, dy: i32, dx: i32) -> usize {
    let picks = selectable(rows).len();
    if picks == 0 {
        return 0;
    }
    let last = picks - 1;
    // Where the menu's entries begin among the cursor positions.
    let menu = rows.iter().take(menu_start(rows)).filter(|r| r.selectable()).count();
    let sel = sel.min(last);

    if dx != 0 {
        // Sideways is a menu idea; in the single-column listing there is nowhere to go.
        if sel < menu {
            return sel;
        }
        return (sel as i32 + dx).clamp(menu as i32, last as i32) as usize;
    }
    match dy {
        d if d > 0 && sel + 1 < menu => sel + 1,
        // Off the bottom of the listing, into the menu's first entry.
        d if d > 0 && sel < menu => menu.min(last),
        d if d > 0 => (sel + MENU_COLS).min(last),
        d if d < 0 && sel > menu + MENU_COLS - 1 => sel - MENU_COLS,
        // Off the top of the menu, back onto the last thing in the listing.
        d if d < 0 && sel >= menu => menu.saturating_sub(1),
        d if d < 0 => sel.saturating_sub(1),
        _ => sel,
    }
}

/// A drawn row and the cells it occupies, so a click resolves to the row the keyboard
/// would have reached. The menu puts several rows on one line, which is why an `x` range
/// is part of this and not just a `y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub y: u16,
    pub x: std::ops::Range<u16>,
    pub row: usize,
}

/// The block the wordmark occupies — its slot, plus the tagline and the blank under it.
///
/// `None` when the greeter fell back to the plain word, which is the shape it uses when
/// there is no room to spare, and therefore no room for a passer-by either. Recomputed here
/// rather than returned from [`draw`] so the client can ask "is there a stage" without
/// drawing a frame; a test keeps the two in step.
pub fn stage(area: TRect, rows_in: &[Row]) -> Option<TRect> {
    if area.height < 6 || area.width < 30 {
        return None;
    }
    let w = area.width.min(72);
    let x = area.x + (area.width - w) / 2;
    let shown = window(rows_in, area.height.saturating_sub(2), None);
    let lines = shown.len() + menu_lines(rows_in);
    let banner_room = area.height.saturating_sub(lines as u16 + 2);
    if banner_room < 4 || logo::height(w, banner_room) < 2 {
        return None;
    }
    let banner_h = logo::height(w, banner_room) + 2;
    let painted = lines as u16 + banner_h.saturating_sub(1);
    let y = area.y + area.height.saturating_sub(painted) / 2;
    Some(TRect { x, y, width: w, height: banner_h })
}

/// Draw the whole screen. Returns where each row landed, for mouse resolution.
pub fn draw(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    rows_in: &[Row],
    sel: usize,
    // Seconds into a crossing of the wordmark, or `None` when nothing is walking. Seconds
    // rather than a column, so a resize is a re-render rather than a rescheduling.
    walk: Option<f64>,
) -> Vec<Hit> {
    fill(buf, area, theme.ui.bg);
    let mut hits = Vec::new();
    if area.height < 6 || area.width < 30 {
        return hits;
    }

    // Content sits in a centred column so a wide terminal does not stretch the list across
    // two feet of desk.
    let w = area.width.min(72);
    let x = area.x + (area.width - w) / 2;

    let sel_rows = selectable(rows_in);
    let cursor_row = sel_rows.get(sel).copied();
    // On a short screen the listing gives way before the menu does.
    let shown = window(rows_in, area.height.saturating_sub(2), cursor_row);
    let lines = shown.len() + menu_lines(rows_in);

    // Centre the whole block vertically. A greeter pinned to the top of a tall terminal
    // leaves a third of the screen empty below it and reads as a page that failed to load.
    let body = lines as u16 + 2;
    let banner_room = area.height.saturating_sub(body);
    let banner_h = if banner_room >= 4 { logo::height(w, banner_room) + 2 } else { 0 };
    // Centred on what is actually painted, which is the banner block less its trailing
    // blank line — a menu long enough to matter makes an approximation here visible.
    let painted = lines as u16 + banner_h.saturating_sub(1);
    let mut y = area.y + area.height.saturating_sub(painted) / 2;
    let banner_top = y;
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
        // In front of the letters rather than beside them: it is drawn after the wordmark
        // and the tagline, over the top of both, and the whole block is repainted every
        // frame — so what it walks over comes back on its own.
        if let Some(at) = walk {
            super::zombie::draw(
                buf,
                TRect { x, y: banner_top, width: w, height: banner_h },
                theme,
                at,
            );
        }
    }

    // The session listing, a row to a line, and the menu's heading. The menu itself is a
    // grid rather than a column, so it is drawn after this rather than in it.
    let start = menu_start(rows_in);
    let heading = start..start + usize::from(menu_lines(rows_in) > 0);
    for i in shown.chain(heading) {
        let row = &rows_in[i];
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
            // Drawn as a grid below, not as a line here.
            Row::Action(_) => continue,
        };

        if selected {
            fill(buf, TRect { x, y, width: w, height: 1 }, theme.ui.selection);
        }
        put_line(buf, x, y, w, Line::from(spans));
        if row.selectable() {
            hits.push(Hit { y, x: x..x + w, row: i });
        }
        y += 1;
    }

    // The menu: [`MENU_COLS`] entries to a line, each one a key in its own right-aligned
    // column so the keys line up down the block rather than hiding inside the labels.
    let cell = w / MENU_COLS as u16;
    for (n, i) in (start..rows_in.len()).filter(|i| matches!(rows_in[*i], Row::Action(_))).enumerate()
    {
        let Row::Action(a) = &rows_in[i] else { continue };
        let (col, row_n) = (n % MENU_COLS, n / MENU_COLS);
        let cy = y + row_n as u16;
        if cy >= area.y + area.height {
            break;
        }
        let cx = x + col as u16 * cell;
        let selected = cursor_row == Some(i);
        if selected {
            fill(buf, TRect { x: cx, y: cy, width: cell, height: 1 }, theme.ui.selection);
        }
        let base = Style::default().bg(color(if selected { theme.ui.selection } else { theme.ui.bg }));
        put_line(
            buf,
            cx,
            cy,
            cell,
            Line::from(vec![
                Span::styled(format!("  {:>3}  ", a.key()), base.fg(color(theme.ui.accent))),
                Span::styled(
                    truncate(a.label(), cell.saturating_sub(7) as usize),
                    base.fg(color(theme.ui.text)),
                ),
            ]),
        );
        hits.push(Hit { y: cy, x: cx..cx + cell, row: i });
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
            !rows.iter().filter(|r| r.selectable()).any(|r| matches!(r, Row::Header(_))),
            "no heading is ever selectable"
        );
    }

    /// The keys are a block, not a sentence: every one of them reachable with the cursor as
    /// well as by its key.
    #[test]
    fn the_menu_lists_every_action_once_and_all_of_it_walkable() {
        let rows = rows(&snap(), 0);
        let listed: Vec<Act> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Action(a) => Some(*a),
                _ => None,
            })
            .collect();
        assert_eq!(listed, Act::all().to_vec(), "every entry, once, in order: {rows:?}");
        assert!(
            rows.iter().filter(|r| matches!(r, Row::Action(_))).all(|r| r.selectable()),
            "an action you can see but not select is a key you had to already know"
        );
    }

    /// Down the listing is one row; down the menu is a whole line of it, and sideways is
    /// only a thing the menu has. Every entry has to stay reachable either way.
    #[test]
    fn the_cursor_walks_a_column_above_and_a_grid_below() {
        let rows = rows(&snap(), 0);
        let picks = selectable(&rows).len();
        let menu = rows.iter().take(menu_start(&rows)).filter(|r| r.selectable()).count();
        assert!(menu > 0 && picks > menu, "the fixture has both halves");

        // The listing is a column: down is one row, and sideways is nothing.
        assert_eq!(move_sel(&rows, 0, 1, 0), 1);
        assert_eq!(move_sel(&rows, 0, 0, 1), 0, "there is no second column up there");
        // Off the bottom of it lands on the menu's first entry, and back up returns.
        assert_eq!(move_sel(&rows, menu - 1, 1, 0), menu);
        assert_eq!(move_sel(&rows, menu, -1, 0), menu - 1);
        // Inside the menu, down is a line of three and sideways is one entry.
        assert_eq!(move_sel(&rows, menu, 1, 0), menu + MENU_COLS);
        assert_eq!(move_sel(&rows, menu + MENU_COLS, -1, 0), menu);
        assert_eq!(move_sel(&rows, menu, 0, 1), menu + 1);
        assert_eq!(move_sel(&rows, menu, 0, -1), menu, "and stops at the left edge");
        // Nothing walks off either end.
        assert_eq!(move_sel(&rows, 0, -1, 0), 0);
        assert_eq!(move_sel(&rows, picks - 1, 1, 0), picks - 1);
        assert_eq!(move_sel(&rows, picks - 1, 0, 1), picks - 1);

        // And every entry is reachable from the top by pressing down and right.
        let mut seen = std::collections::HashSet::from([0usize]);
        for start in 0..picks {
            for (dy, dx) in [(1, 0), (0, 1)] {
                seen.insert(move_sel(&rows, start, dy, dx));
            }
        }
        assert_eq!(seen.len(), picks, "unreachable cursor positions: {seen:?}");
    }

    /// Two entries sharing a key would make one of them unreachable by typing.
    #[test]
    fn every_menu_key_is_distinct_and_resolves_back_to_its_entry() {
        let mut seen = std::collections::HashSet::new();
        for a in Act::all() {
            assert!(seen.insert(a.key()), "{:?} reuses {}", a, a.key());
            assert!(!a.label().is_empty(), "{a:?} has no label");
            let mut chars = a.key().chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                assert_eq!(Act::from_key(c), Some(a), "typing {c} must reach {a:?}");
            }
        }
        assert_eq!(Act::from_key('e'), None, "an unbound key reaches nothing");
        assert_eq!(Act::from_key('s'), None, "and so does a near miss");
    }

    /// A busy session must not push the menu off the bottom of a short terminal — that is
    /// the part of the screen you came here to use.
    #[test]
    fn a_short_screen_drops_the_listing_before_the_menu() {
        let mut s = snap();
        s.recents = (0..12)
            .map(|i| RecentProject {
                name: format!("p{i}"),
                cwd: format!("/home/j/dev/p{i}"),
                last_used: 0,
                live: false,
            })
            .collect();
        let rows = rows(&s, 0);
        let menu = menu_start(&rows);
        let shown = window(&rows, 14, None);
        assert!(shown.len() < menu, "the listing had to give way: {shown:?} of {menu}");
        assert_eq!(shown.len() + menu_lines(&rows), 14, "and the menu keeps its room");
        assert_eq!(menu_lines(&rows), 1 + Act::all().len().div_ceil(MENU_COLS), "header and grid");

        // And when the cursor is below the fold, the listing scrolls to it.
        let last = menu - 1;
        let scrolled = window(&rows, 14, Some(last));
        assert!(scrolled.contains(&last), "cursor {last} is off screen: {scrolled:?}");
    }

    /// Render the greeter and hand back the buffer, so two frames can be compared cell for
    /// cell — symbol *and* style, which is what "the wordmark is untouched" has to mean.
    fn render(w: u16, h: u16, walk: Option<f64>) -> Buffer {
        let s = snap();
        let rows = rows(&s, 0);
        let area = TRect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), &rows, 0, walk);
        buf
    }

    /// The load-bearing test for something that walks in *front* of the letters: it may
    /// cover them while it is there, and it must leave nothing behind when it goes.
    #[test]
    fn the_wordmark_survives_a_zombie_walking_over_it() {
        let (w, h) = (120, 40);
        let before = render(w, h, None);
        let during = render(w, h, Some(12.0));
        assert_ne!(before, during, "nothing moved, so this test proves nothing");

        // Everything it disturbs is inside the block the wordmark was given.
        let s = snap();
        let stage = stage(TRect::new(0, 0, w, h), &rows(&s, 0)).expect("a tall terminal has one");
        for y in 0..h {
            for x in 0..w {
                if before[(x, y)] != during[(x, y)] {
                    assert!(
                        (stage.y..stage.y + stage.height).contains(&y)
                            && (stage.x..stage.x + stage.width).contains(&x),
                        "cell {x},{y} changed, outside the stage {stage:?}"
                    );
                }
            }
        }

        // And once it has gone the greeter is the same picture it was, to the byte.
        assert_eq!(before, render(w, h, None), "it left a mark behind");
    }

    /// The stage has to be where the banner actually is, or the passer-by walks through the
    /// project list. Two copies of centring arithmetic agree only until one of them moves.
    #[test]
    fn the_stage_is_where_the_wordmark_landed() {
        for (w, h) in [(120, 40), (100, 30)] {
            let s = snap();
            let rows = rows(&s, 0);
            let stage = stage(TRect::new(0, 0, w, h), &rows).expect("{w}x{h} has a banner");
            let buf = render(w, h, None);
            let painted: Vec<u16> = (0..h)
                .filter(|y| (0..w).any(|x| buf[(x, *y)].symbol() != " "))
                .collect();
            assert_eq!(
                painted.first().copied(),
                Some(stage.y + 1),
                "{w}x{h}: the banner starts a row into its slot, under the pad row"
            );
            assert!(stage.height >= 6, "{w}x{h}: {stage:?} is too short to hold anybody");
        }
    }

    /// A terminal with no room for a banner has no room for a passer-by either.
    #[test]
    fn a_greeter_with_no_wordmark_never_animates() {
        let (w, h) = (80, 24);
        let s = snap();
        assert_eq!(stage(TRect::new(0, 0, w, h), &rows(&s, 0)), None, "no banner at {w}x{h}");
        assert_eq!(render(w, h, Some(12.0)), render(w, h, None), "yet something was drawn");
    }

    /// Print the greeter mid-crossing, in colour.
    ///
    /// `cargo test the_greeter_prints -- --nocapture` is how the passer-by gets looked at in
    /// its actual setting — over the letters, at the size the terminal really gives it —
    /// without rebuilding horde and waiting up to a minute for a crossing.
    #[test]
    fn the_greeter_prints() {
        for (w, h, at) in [(120u16, 40u16, 11.0), (100, 30, 13.0)] {
            let buf = render(w, h, Some(at));
            println!("\n  {w}x{h}, {at}s into a crossing");
            for y in 0..h {
                let mut line = String::new();
                for x in 0..w {
                    let st = buf[(x, y)].style();
                    let esc = |c: Option<ratatui::style::Color>, base: u8| match c {
                        Some(ratatui::style::Color::Rgb(r, g, b)) => {
                            format!("\x1b[{base};2;{r};{g};{b}m")
                        }
                        _ => String::new(),
                    };
                    line.push_str(&esc(st.fg, 38));
                    line.push_str(&esc(st.bg, 48));
                    line.push_str(buf[(x, y)].symbol());
                }
                println!("  {line}\x1b[0m");
            }
        }
    }

    /// Nothing scrolls while everything fits.
    #[test]
    fn a_tall_screen_shows_the_whole_listing() {
        let rows = rows(&snap(), 0);
        let menu = menu_start(&rows);
        assert_eq!(window(&rows, 40, Some(0)), 0..menu);
        assert_eq!(window(&rows, 40, Some(menu + 3)), 0..menu, "a cursor in the menu never scrolls it");
    }

    /// The greeter is centred and every section is present and in order. Pinned as text
    /// because "does the start screen still say what it is for" is worth knowing on a diff.
    #[test]
    fn the_greeter_draws_centred_with_its_sections_in_order() {
        let s = snap();
        let rows = rows(&s, 3 * 86_400_000);
        let area = TRect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), &rows, 0, None);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();

        let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle}:\n{text}"));
        assert!(at("NEEDS YOU") < at("PROJECTS"), "waiting agents come first");
        assert!(at("PROJECTS") < at("RECENT"), "open projects before closed ones");
        assert!(text.contains("resume"), "a closed project says what enter will do");
        assert!(text.contains("3d ago"), "and when it was last open");
        assert!(at("RECENT") < at("ACTIONS"), "the menu sits under the session, not in it");

        // Each action on its own line, key in a column of its own.
        for a in Act::all() {
            let line = format!("{:>3}  {}", a.key(), a.label());
            assert!(text.contains(&line), "missing menu line {line:?}:\n{text}");
        }

        let painted: Vec<u16> = (0..area.height)
            .filter(|y| (0..area.width).any(|x| buf[(x, *y)].symbol() != " "))
            .collect();
        let (top, bottom) = (painted[0], area.height - 1 - painted[painted.len() - 1]);
        assert!(top.abs_diff(bottom) <= 2, "vertically centred, got {top} above and {bottom} below");
    }
}
