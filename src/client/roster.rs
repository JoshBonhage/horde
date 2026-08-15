//! What the sidebar shows, worked out once and drawn separately.
//!
//! The sidebar used to decide what to show while it was drawing it: the fit loop, the
//! overflow note and the row contents were one pass. That is fine for a flat list and stops
//! being fine the moment rows nest, scroll, or answer to a cursor — because then the
//! renderer is the only thing that knows what row 7 *is*, and every other reader (the click
//! handler, the key handler) has to re-derive it and drift out of step.
//!
//! So the panel body is built here as a flat `Vec<Row>` first, and drawn second. One list,
//! several readers, no parallel arithmetic. `overlays::settings` already does this locally
//! with its own flatten-then-window pass; this is the same idea promoted to a module because
//! more than one view now needs it.
//!
//! The client owns no session state — a `Snapshot` is replaced wholesale every frame — so
//! nothing here caches. It is a projection, recomputed per frame, cheap because the session
//! is small.

use crate::proto::{AgentState, Cmd, PaneId, Snapshot, SpaceId};

/// How much chrome the panel's width can pay for.
///
/// Derived once rather than scattered as `if inner_w < n` checks through the renderer, so
/// the degradation ladder is one thing that can be read, reasoned about, and tested at each
/// rung. `ui.sidebar_width` is clamped to 14–60, so every rung here is reachable and the
/// default of 24 lands on `Normal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Tight,
    Normal,
    Wide,
}

impl Density {
    /// `inner` is the panel width less the marker column and the right margin.
    pub fn of(inner: u16) -> Density {
        match inner {
            0..=17 => Density::Tight,
            18..=29 => Density::Normal,
            _ => Density::Wide,
        }
    }

    /// Columns a grouped agent is indented by.
    ///
    /// Zero when tight: at that width the indent costs more than it explains, and the header
    /// above is already carrying the grouping on its own.
    pub fn indent(&self) -> u16 {
        match self {
            Density::Tight => 0,
            _ => 2,
        }
    }

    /// How many state counts a group header's rollup may show.
    pub fn rollup_counts(&self) -> usize {
        match self {
            Density::Tight => 1,
            _ => 2,
        }
    }

    /// Whether there is room to say how much of the list is on screen.
    pub fn shows_counter(&self) -> bool {
        !matches!(self, Density::Tight)
    }
}

/// A group's agent states, counted.
///
/// Rendered two ways from one type: glyphs in the sidebar, where fourteen columns is a
/// realistic budget, and prose where there is room for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Roll {
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
    pub serving: usize,
}

impl Roll {
    pub fn add(&mut self, state: AgentState) {
        match state {
            AgentState::Blocked => self.blocked += 1,
            AgentState::Working => self.working += 1,
            AgentState::Done => self.done += 1,
            AgentState::Idle => self.idle += 1,
            AgentState::Serving => self.serving += 1,
            AgentState::Unknown => {}
        }
    }

    /// The non-zero counts, in urgency order, with the state each belongs to.
    ///
    /// Urgency order — blocked, working, done, idle, serving — matches the status bar's own
    /// count ordering. It matters here because `Density` truncates this list, so the order
    /// decides what survives a narrow panel: what you might have to act on, never what is
    /// merely resting, and least of all a dev server doing exactly what it always does.
    pub fn parts(&self) -> Vec<(AgentState, usize)> {
        [
            (AgentState::Blocked, self.blocked),
            (AgentState::Working, self.working),
            (AgentState::Done, self.done),
            (AgentState::Idle, self.idle),
            (AgentState::Serving, self.serving),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .collect()
    }

    /// The rollup as prose, for somewhere with room for it.
    ///
    /// Two renderings of one type: the sidebar has fourteen columns and gets glyphs, the
    /// roster has thirty-odd and gets words. The prose form is what the sidebar cannot afford.
    pub fn prose(&self) -> String {
        let parts: Vec<String> = self
            .parts()
            .into_iter()
            .map(|(s, n)| {
                let word = match s {
                    // "1 needs you" rather than "1 blocked": the label people act on.
                    AgentState::Blocked => "needs you",
                    other => other.label(),
                };
                format!("{n} {word}")
            })
            .collect();
        if parts.is_empty() {
            return "no agents".into();
        }
        parts.join(" · ")
    }

    /// Glyph-and-count pairs for the sidebar, capped by the panel's width.
    ///
    /// Counts above nine render as `9+` so the whole rollup stays within six columns however
    /// many agents a project holds — a header that grew with its group would push the space
    /// name out of a panel that has none to spare.
    pub fn compact(&self, d: Density) -> Vec<(AgentState, String)> {
        self.parts()
            .into_iter()
            .take(d.rollup_counts())
            .map(|(s, n)| (s, if n > 9 { "9+".to_string() } else { n.to_string() }))
            .collect()
    }
}

/// One line of the sidebar's agent list, and the unit a cursor will step over.
#[derive(Debug, Clone, PartialEq)]
pub enum RowKind {
    /// A space's header, standing over its agents.
    Group { space: SpaceId, roll: Roll, collapsed: bool },
    Agent { pane: PaneId, space: SpaceId, pinned: bool },
    /// The indented "12 tools · 1 failed" line under a working agent. Not a row you can
    /// land on: it describes the row above rather than naming anything of its own.
    Activity(PaneId),
    /// Why the list is empty, so a panel with nothing in it still says something.
    Empty(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: RowKind,
    pub indent: u16,
}

impl Row {
    fn new(kind: RowKind) -> Row {
        Row { kind, indent: 0 }
    }

    fn indented(kind: RowKind, indent: u16) -> Row {
        Row { kind, indent }
    }

    /// What the cursor would be on here, if it can land here at all.
    pub fn focus(&self) -> Option<Focus> {
        match self.kind {
            RowKind::Group { space, .. } => Some(Focus::Group(space)),
            RowKind::Agent { pane, .. } => Some(Focus::Agent(pane)),
            RowKind::Activity(_) | RowKind::Empty(_) => None,
        }
    }
}

/// Where the cursor sits.
///
/// Named by *identity*, never by index: a snapshot is replaced wholesale every frame, so an
/// index into last frame's list points at a different agent the moment a space closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Group(SpaceId),
    Agent(PaneId),
}

impl Focus {
    /// What pressing enter on this does.
    pub fn activate(&self) -> Cmd {
        match self {
            // A group header stands for its project, so entering it goes there — the same
            // thing clicking the space row does.
            Focus::Group(s) => Cmd::FocusSpace(*s),
            Focus::Agent(p) => Cmd::FocusPane(*p),
        }
    }
}

/// A filter over the agent list.
///
/// A lens never changes what exists, only what is shown — which is what makes it safe to
/// leave on. The footer counts stay session-wide and the heading names the active lens,
/// because a filtered list that does not say it is filtered reads as a broken one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Lens {
    #[default]
    All,
    /// Blocked or done — the ones you can actually act on.
    NeedsYou,
    Working,
    /// The focused space only.
    Here,
    /// Agents whose role matches. The reason roles exist: this is "every reviewer, across all
    /// six projects", which the space tree cannot express at all.
    Role(String),
}

impl Lens {
    /// The next lens the cycle key moves to.
    ///
    /// Ends back at `All`, so the key is always also the way out. A filter you can enter but
    /// have to remember a second key to leave is a trap.
    pub fn cycle(&self) -> Lens {
        match self {
            Lens::All => Lens::NeedsYou,
            Lens::NeedsYou => Lens::Working,
            Lens::Working => Lens::Here,
            // A named role is somewhere you went deliberately, so one press returns you.
            Lens::Here | Lens::Role(_) => Lens::All,
        }
    }

    /// Shown on the AGENTS heading. Empty for `All`, so an unfiltered panel spends no columns.
    pub fn label(&self) -> String {
        match self {
            Lens::All => String::new(),
            Lens::NeedsYou => "needs you".into(),
            Lens::Working => "working".into(),
            Lens::Here => "here".into(),
            Lens::Role(r) => r.clone(),
        }
    }

    fn matches(&self, a: &AgentRow, snap: &Snapshot) -> bool {
        match self {
            Lens::All => true,
            Lens::NeedsYou => a.state.needs_attention(),
            Lens::Working => a.state == AgentState::Working,
            Lens::Here => snap.focused_space == Some(a.space),
            Lens::Role(want) => snap
                .panes
                .iter()
                .find(|p| p.id == a.pane)
                .and_then(|p| p.role.as_deref())
                .is_some_and(|r| r == want),
        }
    }
}

/// Sidebar view state the client owns.
///
/// Only what is genuinely ephemeral lives here. A collapse and a pin are *decisions* and live
/// in the daemon, so they survive a detach; where the cursor happens to be does not deserve
/// that, and would be wrong to restore into a session that has moved on.
#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    /// `None` until the panel is first given the keyboard, so an untouched sidebar shows no
    /// cursor rather than implying one row is special.
    pub cursor: Option<Focus>,
    /// Where the cursor was last resolved to, so it can stay put when the row under it goes.
    pub at: usize,
    /// First row of the agent list on screen.
    pub scroll: usize,
    /// What the list is filtered to.
    pub lens: Lens,
}

impl SidebarState {
    /// Point the cursor at a live row, and report which one.
    ///
    /// On a miss — the space closed, the agent exited — the remembered index is clamped into
    /// the new list and the nearest selectable row taken. The cursor stays where it was rather
    /// than jumping home, which is what a file manager does and what you expect.
    pub fn resolve(&mut self, rows: &[Row]) -> Option<usize> {
        let selectable = |i: &usize| rows.get(*i).is_some_and(|r| r.focus().is_some());

        if let Some(want) = self.cursor {
            if let Some(i) = rows.iter().position(|r| r.focus() == Some(want)) {
                self.at = i;
                return Some(i);
            }
        }
        // Nearest surviving row to where we were, searching outward.
        let start = self.at.min(rows.len().saturating_sub(1));
        let found = (start..rows.len())
            .find(selectable)
            .or_else(|| (0..=start).rev().find(selectable))?;
        self.at = found;
        self.cursor = rows[found].focus();
        Some(found)
    }

    /// Move `delta` selectable rows, stopping at the ends rather than wrapping.
    ///
    /// Wrapping is right for a menu you opened on purpose and wrong for a list you are
    /// scanning: holding `j` should come to rest at the bottom, not start again at the top.
    pub fn step(&mut self, rows: &[Row], delta: i32) {
        let Some(from) = self.resolve(rows) else { return };
        let mut i = from as i64;
        let mut left = delta.abs();
        let dir = delta.signum() as i64;
        while left > 0 {
            let next = i + dir;
            if next < 0 || next as usize >= rows.len() {
                break;
            }
            i = next;
            if rows[i as usize].focus().is_some() {
                left -= 1;
            }
        }
        if let Some(f) = rows[i as usize].focus() {
            self.cursor = Some(f);
            self.at = i as usize;
        }
    }

    /// Jump to the first or last selectable row.
    pub fn jump(&mut self, rows: &[Row], last: bool) {
        let found = if last {
            rows.iter().rposition(|r| r.focus().is_some())
        } else {
            rows.iter().position(|r| r.focus().is_some())
        };
        if let Some(i) = found {
            self.at = i;
            self.cursor = rows[i].focus();
        }
    }

    /// Forget a cursor, pin or scroll that points at something no longer in the session.
    pub fn prune(&mut self, snap: &Snapshot) {
        if let Some(f) = self.cursor {
            let alive = match f {
                Focus::Group(s) => snap.spaces.iter().any(|x| x.id == s),
                Focus::Agent(p) => snap.panes.iter().any(|x| x.id == p),
            };
            if !alive {
                // Keep `at` — it is what lets `resolve` put the cursor back near where the
                // vanished row was rather than at the top.
                self.cursor = None;
            }
        }
    }
}

/// One agent, located in the space/tab/pane tree.
///
/// Identity and shape only — the name, elapsed time and activity text are read straight from
/// the snapshot when a row is drawn, so nothing here is a copy that could go stale, and a
/// frame costs no string allocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRow {
    pub pane: PaneId,
    pub space: SpaceId,
    pub state: AgentState,
    /// Whether it has a second line to draw beneath it.
    pub has_activity: bool,
    pub pinned: bool,
}

/// Every agent in the session, in space then tab then pane order.
///
/// Stable ordering beats sorting by urgency: rows that jump around under you are worse than
/// rows you have to scan, and colour already carries the urgency.
pub fn collect_agents(snap: &Snapshot) -> Vec<AgentRow> {
    let mut out = Vec::new();
    for space in &snap.spaces {
        for &tid in &space.tabs {
            let Some(tab) = snap.tabs.iter().find(|t| t.id == tid) else { continue };
            for &pid in &tab.panes {
                let Some(pane) = snap.panes.iter().find(|p| p.id == pid) else { continue };
                let Some(a) = pane.agent.as_ref() else { continue };
                out.push(AgentRow {
                    pane: pid,
                    space: space.id,
                    state: a.state,
                    pinned: pane.pinned,
                    // Only while it is actually doing something; a finished turn's counts
                    // would be stale trivia.
                    has_activity: a.state == AgentState::Working
                        && a.activity.summary().is_some(),
                });
            }
        }
    }
    out
}

/// The AGENTS section as a flat list: a header per space that has agents, then its agents,
/// filtered by the active lens.
///
/// A space with no agents gets no header. The SPACES section above already lists every
/// project; repeating the empty ones here would spend the panel's scarcest resource — rows —
/// on saying nothing twice.
pub fn filtered_rows(snap: &Snapshot, d: Density, lens: &Lens) -> Vec<Row> {
    let all = collect_agents(snap);
    if all.is_empty() {
        // Say how to get one rather than leaving an unexplained empty panel.
        return vec![Row::new(RowKind::Empty("none yet"))];
    }
    let agents: Vec<AgentRow> = all.into_iter().filter(|a| lens.matches(a, snap)).collect();
    if agents.is_empty() {
        // An empty *filtered* list is a different fact from an empty session, and has to say
        // which one it is or it reads as everything having stopped.
        return vec![Row::new(RowKind::Empty("no agents match"))];
    }

    let mut out = Vec::new();

    // Pinned agents lift to the top, out of their groups. The one sanctioned exception to the
    // stable ordering above — and it is not really an exception, because *you* moved these.
    // Rows that reorder themselves are worse than rows you scan; rows you put somewhere are
    // not, since you know where you put them.
    for a in agents.iter().filter(|a| a.pinned) {
        out.push(Row::new(RowKind::Agent { pane: a.pane, space: a.space, pinned: true }));
        if a.has_activity {
            out.push(Row::indented(RowKind::Activity(a.pane), 2));
        }
    }

    for space in &snap.spaces {
        let mine: Vec<&AgentRow> = agents.iter().filter(|a| a.space == space.id).collect();
        if mine.is_empty() {
            continue;
        }
        // The rollup counts every agent in the space, pinned ones included. Pinning moves a
        // row; it does not change what is running in a project, which is what the header
        // answers.
        let mut roll = Roll::default();
        for a in &mine {
            roll.add(a.state);
        }
        out.push(Row::new(RowKind::Group {
            space: space.id,
            roll,
            collapsed: space.collapsed,
        }));
        if space.collapsed {
            continue;
        }
        for a in mine.into_iter().filter(|a| !a.pinned) {
            out.push(Row::indented(
                RowKind::Agent { pane: a.pane, space: a.space, pinned: false },
                d.indent(),
            ));
            if a.has_activity {
                out.push(Row::indented(RowKind::Activity(a.pane), d.indent() + 2));
            }
        }
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::proto::{AgentInfo, PaneInfo, Rect, SpaceInfo, TabInfo, ViewState};

    pub fn pane(id: u32, space: u32, tab: u32, agent: Option<(&str, AgentState)>) -> PaneInfo {
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
                class: Default::default(),
                state: s,
                elapsed: 138,
                authority: "hook".into(),
                reason: "t".into(),
                activity: Default::default(),
                question: None,
            }),
            spawned_by: None,
            exited: false,
            scroll_offset: 0,
            wants_mouse: false,
            bracketed_paste: false,
            role: None,
            pinned: false,
            board: false,
            repo: None,
        }
    }

    /// Two spaces: `api-refactor` with builder + reviewer + a shell, `docs` with writer.
    pub fn snap() -> Snapshot {
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
                    accent: 0,
                    collapsed: false,
                    repo: None,
                    notes: None,
            lsp: Vec::new(),
                },
                SpaceInfo {
                    id: 2,
                    name: "docs".into(),
                    cwd: "/tmp".into(),
                    tabs: vec![2],
                    focused_tab: Some(2),
                    agent_count: 1,
                    attention_count: 0,
                    accent: 1,
                    collapsed: false,
                    repo: None,
                    notes: None,
            lsp: Vec::new(),
                },
            ],
            tabs: vec![
                TabInfo { id: 1, space: 1, name: "1".into(), panes: vec![1, 2, 3], focused_pane: Some(1) },
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
            recents: Vec::new(),
        }
    }

    #[test]
    fn every_agent_sits_under_a_header_for_its_own_space() {
        let rows = filtered_rows(&snap(), Density::Normal, &Lens::All);
        let kinds: Vec<&RowKind> = rows.iter().map(|r| &r.kind).collect();
        // api-refactor's header, then both of its agents, then docs' header, then writer.
        assert!(matches!(kinds[0], RowKind::Group { space: 1, .. }), "{kinds:?}");
        assert!(matches!(kinds[1], RowKind::Agent { pane: 1, space: 1, .. }));
        assert!(matches!(kinds[2], RowKind::Agent { pane: 2, space: 1, .. }));
        assert!(matches!(kinds[3], RowKind::Group { space: 2, .. }));
        assert!(matches!(kinds[4], RowKind::Agent { pane: 4, space: 2, .. }));
        assert_eq!(kinds.len(), 5, "the shell pane is not an agent: {kinds:?}");
    }

    #[test]
    fn a_group_header_rolls_up_the_states_of_its_agents() {
        let rows = filtered_rows(&snap(), Density::Normal, &Lens::All);
        let RowKind::Group { roll, .. } = rows[0].kind else { panic!("{:?}", rows[0]) };
        assert_eq!(roll, Roll { blocked: 1, working: 1, done: 0, idle: 0, serving: 0 });
    }

    /// A space with no agents already appears in SPACES; a second empty row here would spend
    /// the panel's scarcest resource saying nothing twice.
    #[test]
    fn a_space_with_no_agents_gets_no_header() {
        let mut s = snap();
        s.panes.retain(|p| p.space != 2);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::Group { space: 2, .. })),
            "{rows:?}"
        );
    }

    #[test]
    fn no_agents_at_all_says_so_rather_than_returning_nothing() {
        let mut s = snap();
        s.panes.iter_mut().for_each(|p| p.agent = None);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Empty("none yet")));
    }

    /// Narrow panels drop the indent: at that width it costs more than it explains, and the
    /// header above still carries the grouping.
    #[test]
    fn a_tight_panel_groups_without_indenting() {
        let wide = filtered_rows(&snap(), Density::Wide, &Lens::All);
        let tight = filtered_rows(&snap(), Density::Tight, &Lens::All);
        assert_eq!(wide[1].indent, 2);
        assert_eq!(tight[1].indent, 0);
        // The grouping itself is unchanged — only the indent yields.
        assert_eq!(wide.len(), tight.len());
    }

    #[test]
    fn the_rollup_orders_counts_by_urgency_and_a_tight_panel_keeps_the_first() {
        let roll = Roll { blocked: 1, working: 2, done: 3, idle: 4, serving: 0 };
        let order: Vec<AgentState> = roll.parts().into_iter().map(|(s, _)| s).collect();
        assert_eq!(
            order,
            vec![AgentState::Blocked, AgentState::Working, AgentState::Done, AgentState::Idle]
        );
        assert_eq!(roll.compact(Density::Normal).len(), 2);
        let tight = roll.compact(Density::Tight);
        assert_eq!(tight.len(), 1);
        assert_eq!(tight[0].0, AgentState::Blocked, "urgency survives the squeeze");
    }

    /// However many agents a project holds, its header stays within six columns — a rollup
    /// that grew with its group would push the space name out of a panel that has none spare.
    #[test]
    fn a_large_count_is_abbreviated_rather_than_widening_the_header() {
        let roll = Roll { blocked: 12, working: 40, done: 0, idle: 0, serving: 0 };
        let parts = roll.compact(Density::Normal);
        assert_eq!(parts[0].1, "9+");
        assert_eq!(parts[1].1, "9+");
    }

    #[test]
    fn an_agent_only_gets_an_activity_line_while_it_is_working() {
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
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        let activity: Vec<&Row> =
            rows.iter().filter(|r| matches!(r.kind, RowKind::Activity(_))).collect();
        // Only `builder` is working; the blocked and idle agents get nothing.
        assert_eq!(activity.len(), 1, "{rows:?}");
        assert!(matches!(activity[0].kind, RowKind::Activity(1)));
        // Indented past its agent, so it reads as belonging to the row above.
        assert_eq!(activity[0].indent, 4);
    }

    #[test]
    fn the_density_ladder_covers_every_reachable_sidebar_width() {
        // `ui.sidebar_width` is clamped 14..=60, and `inner` is two less than that.
        assert_eq!(Density::of(12), Density::Tight);
        assert_eq!(Density::of(17), Density::Tight);
        assert_eq!(Density::of(18), Density::Normal);
        assert_eq!(Density::of(29), Density::Normal);
        assert_eq!(Density::of(30), Density::Wide);
        assert_eq!(Density::of(58), Density::Wide);
    }

    #[test]
    fn a_collapsed_space_keeps_its_header_and_hides_its_agents() {
        let mut s = snap();
        s.spaces[0].collapsed = true;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        assert!(
            matches!(rows[0].kind, RowKind::Group { space: 1, collapsed: true, .. }),
            "the header stays, or the project vanishes rather than folds: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::Agent { space: 1, .. })),
            "{rows:?}"
        );
        // The other space is untouched.
        assert!(rows.iter().any(|r| matches!(r.kind, RowKind::Agent { pane: 4, .. })));
    }

    /// A folded group still reports what is inside it — that is the entire point of folding
    /// it rather than closing it.
    #[test]
    fn a_collapsed_group_still_rolls_up_what_it_hides() {
        let mut s = snap();
        s.spaces[0].collapsed = true;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        let RowKind::Group { roll, .. } = rows[0].kind else { panic!() };
        assert_eq!(roll, Roll { blocked: 1, working: 1, done: 0, idle: 0, serving: 0 });
    }

    #[test]
    fn a_pinned_agent_is_lifted_out_of_its_group() {
        let mut s = snap();
        s.panes.iter_mut().find(|p| p.id == 4).unwrap().pinned = true;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        assert!(
            matches!(rows[0].kind, RowKind::Agent { pane: 4, pinned: true, .. }),
            "pinned rows come first: {rows:?}"
        );
        // Exactly once — pinning moves a row, it does not clone one.
        let n = rows.iter().filter(|r| matches!(r.kind, RowKind::Agent { pane: 4, .. })).count();
        assert_eq!(n, 1, "{rows:?}");
    }

    /// Pinning moves a row; it does not change what is running in a project, which is what
    /// the header answers.
    #[test]
    fn a_pinned_agent_is_still_counted_by_the_group_it_left() {
        let mut s = snap();
        s.panes.iter_mut().find(|p| p.id == 1).unwrap().pinned = true;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        let g = rows
            .iter()
            .find_map(|r| match r.kind {
                RowKind::Group { space: 1, roll, .. } => Some(roll),
                _ => None,
            })
            .expect("api-refactor still has a header");
        assert_eq!(
            g,
            Roll { blocked: 1, working: 1, done: 0, idle: 0, serving: 0 },
            "the rollup counts the pinned agent too"
        );
    }

    fn rows() -> Vec<Row> {
        filtered_rows(&snap(), Density::Normal, &Lens::All)
    }

    #[test]
    fn the_cursor_steps_over_selectable_rows_and_stops_at_the_ends() {
        let rows = rows();
        let mut st = SidebarState::default();
        // Resolving with no cursor lands on the first selectable row.
        assert_eq!(st.resolve(&rows), Some(0));
        assert_eq!(st.cursor, Some(Focus::Group(1)));

        st.step(&rows, 1);
        assert_eq!(st.cursor, Some(Focus::Agent(1)));
        st.step(&rows, 3);
        assert_eq!(st.cursor, Some(Focus::Agent(4)), "across the group boundary");

        // Holding `j` at the bottom stops there rather than wrapping to the top: wrapping is
        // right for a menu you opened and wrong for a list you are scanning.
        st.step(&rows, 10);
        assert_eq!(st.cursor, Some(Focus::Agent(4)));
        st.step(&rows, -100);
        assert_eq!(st.cursor, Some(Focus::Group(1)));
    }

    #[test]
    fn an_activity_line_is_not_a_row_the_cursor_can_land_on() {
        let mut s = snap();
        for p in s.panes.iter_mut() {
            if let Some(a) = p.agent.as_mut() {
                a.activity =
                    crate::proto::Activity { tools: 3, files: 0, errors: 0, turns: 1, last_tool: None };
            }
        }
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        assert!(rows.iter().any(|r| matches!(r.kind, RowKind::Activity(_))));
        let mut st = SidebarState::default();
        // Walk the whole list and check every landing spot. An activity line describes the
        // row above rather than naming anything of its own, so stopping on one would leave
        // `enter` with nothing to do.
        for _ in 0..rows.len() + 2 {
            let at = st.resolve(&rows).expect("a live row");
            assert!(
                !matches!(rows[at].kind, RowKind::Activity(_)),
                "cursor landed on an activity line: {:?}",
                rows[at]
            );
            st.step(&rows, 1);
        }
    }

    #[test]
    fn jump_reaches_the_first_and_last_rows() {
        let rows = rows();
        let mut st = SidebarState::default();
        st.jump(&rows, true);
        assert_eq!(st.cursor, Some(Focus::Agent(4)));
        st.jump(&rows, false);
        assert_eq!(st.cursor, Some(Focus::Group(1)));
    }

    /// The direct test of snapshot-replacement resilience: a snapshot is swapped wholesale
    /// every frame, so the cursor is named by identity and has to survive its row vanishing.
    #[test]
    fn the_cursor_survives_the_space_it_was_on_disappearing() {
        let mut s = snap();
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        let mut st = SidebarState::default();
        st.resolve(&rows);
        st.step(&rows, 3); // onto docs' header
        assert_eq!(st.cursor, Some(Focus::Group(2)));

        s.spaces.retain(|x| x.id != 2);
        s.panes.retain(|p| p.space != 2);
        st.prune(&s);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        let at = st.resolve(&rows).expect("the cursor must land somewhere live");
        assert!(rows[at].focus().is_some());
        // Near where it was, not back at the top.
        assert_eq!(st.cursor, Some(Focus::Agent(2)), "{rows:?}");
    }

    #[test]
    fn the_cursor_survives_its_agent_exiting() {
        let mut s = snap();
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        let mut st = SidebarState::default();
        st.cursor = Some(Focus::Agent(2));
        st.resolve(&rows);

        s.panes.retain(|p| p.id != 2);
        st.prune(&s);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        assert!(st.resolve(&rows).is_some());
        assert_ne!(st.cursor, Some(Focus::Agent(2)));
    }

    /// An empty list has nothing to point at, and must say so rather than panicking on an
    /// index into it.
    #[test]
    fn a_cursor_on_an_empty_list_resolves_to_nothing() {
        let mut s = snap();
        s.panes.iter_mut().for_each(|p| p.agent = None);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All);
        let mut st = SidebarState::default();
        assert_eq!(st.resolve(&rows), None);
        st.step(&rows, 1);
        st.jump(&rows, true);
        assert_eq!(st.cursor, None);
    }

    #[test]
    fn enter_goes_to_the_space_for_a_header_and_the_pane_for_an_agent() {
        assert_eq!(Focus::Group(3).activate(), Cmd::FocusSpace(3));
        assert_eq!(Focus::Agent(7).activate(), Cmd::FocusPane(7));
    }

    fn lensed(s: &Snapshot, l: Lens) -> Vec<Row> {
        filtered_rows(s, Density::Normal, &l)
    }

    #[test]
    fn a_lens_hides_agents_that_do_not_match() {
        let rows = lensed(&snap(), Lens::NeedsYou);
        let panes: Vec<PaneId> = rows
            .iter()
            .filter_map(|r| match r.kind {
                RowKind::Agent { pane, .. } => Some(pane),
                _ => None,
            })
            .collect();
        // Only `reviewer` is blocked; the working and idle ones are filtered out.
        assert_eq!(panes, vec![2]);
    }

    /// A group with nothing left in it should not keep a header — an empty project heading is
    /// worse than no heading, because it reads as a project whose agents all stopped.
    #[test]
    fn a_lens_that_empties_a_group_drops_its_header_too() {
        let rows = lensed(&snap(), Lens::NeedsYou);
        assert!(rows.iter().any(|r| matches!(r.kind, RowKind::Group { space: 1, .. })));
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::Group { space: 2, .. })),
            "docs has no blocked agents: {rows:?}"
        );
    }

    /// An empty *filtered* list is a different fact from an empty session, and has to say
    /// which one it is or it reads as everything having stopped.
    #[test]
    fn a_lens_matching_nothing_says_so_rather_than_looking_empty() {
        let rows = lensed(&snap(), Lens::Role("nobody".into()));
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Empty("no agents match")), "{rows:?}");

        // And that is distinct from having no agents at all.
        let mut s = snap();
        s.panes.iter_mut().for_each(|p| p.agent = None);
        assert!(matches!(lensed(&s, Lens::All)[0].kind, RowKind::Empty("none yet")));
    }

    /// The reason roles exist: "every reviewer, across all six projects" is a question the
    /// space tree cannot express at all.
    #[test]
    fn a_role_lens_gathers_the_same_job_from_every_project() {
        let mut s = snap();
        s.panes.iter_mut().find(|p| p.id == 2).unwrap().role = Some("reviewer".into());
        s.panes.iter_mut().find(|p| p.id == 4).unwrap().role = Some("reviewer".into());
        let rows = lensed(&s, Lens::Role("reviewer".into()));
        let panes: Vec<PaneId> = rows
            .iter()
            .filter_map(|r| match r.kind {
                RowKind::Agent { pane, .. } => Some(pane),
                _ => None,
            })
            .collect();
        assert_eq!(panes, vec![2, 4], "two projects, one role: {rows:?}");
    }

    #[test]
    fn the_here_lens_keeps_only_the_focused_space() {
        let rows = lensed(&snap(), Lens::Here);
        assert!(rows.iter().all(|r| !matches!(r.kind, RowKind::Agent { space: 2, .. })));
        assert!(rows.iter().any(|r| matches!(r.kind, RowKind::Agent { space: 1, .. })));
    }

    /// A filter you can enter but need a second key to leave is a trap, so the cycle key is
    /// always also the way out.
    #[test]
    fn the_lens_cycle_returns_to_all() {
        let mut l = Lens::All;
        for _ in 0..8 {
            l = l.cycle();
            if l == Lens::All {
                return;
            }
        }
        panic!("the cycle never comes back to All");
    }

    #[test]
    fn only_an_unfiltered_list_spends_no_columns_naming_its_lens() {
        assert_eq!(Lens::All.label(), "");
        assert_eq!(Lens::NeedsYou.label(), "needs you");
        assert_eq!(Lens::Role("reviewer".into()).label(), "reviewer");
    }

    /// Two renderings of one type: the sidebar has fourteen columns and gets glyphs, the
    /// roster has thirty-odd and gets words.
    #[test]
    fn the_rollup_has_a_prose_form_for_where_there_is_room() {
        let r = Roll { blocked: 1, working: 2, done: 0, idle: 0, serving: 0 };
        assert_eq!(r.prose(), "1 needs you · 2 working");
        assert_eq!(Roll::default().prose(), "no agents");
    }
}
