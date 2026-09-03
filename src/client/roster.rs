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

use crate::proto::{AgentClass, AgentState, Cmd, MemoryId, PaneId, Snapshot, SpaceId};

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

/// Which of the sidebar's two lists a row was built for.
///
/// The same builder makes both, because they are the same shape: projects with things
/// running under them. What differs is only which class of thing is collected and what an
/// empty result means, and neither is worth a second copy of the grouping, rail and pin
/// logic that would then have to be kept in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Things you talk to.
    Agents,
    /// Notes the project has saved for its agents: `.horde/memory/*.md`.
    Memory,
    /// Things that just run: dev servers, watchers, tunnels.
    Services,
}

impl Section {
    pub fn label(&self) -> &'static str {
        match self {
            Section::Agents => "AGENTS",
            Section::Memory => "MEMORY",
            Section::Services => "SERVICES",
        }
    }
}

/// The connector drawn down a row's indent column, tying it to the project it hangs from.
///
/// The tie is a *colour* first and a glyph second. Every rail is drawn in its project's
/// accent — the same hue as that project's dot up in SPACES — so the three servers under
/// `api-refactor` and the two agents under `api-refactor` are one visible strand running the
/// height of the panel, even though a rule and a section label sit between them. The glyphs
/// only say where in the strand you are; without the colour they would just be a tree, and a
/// tree drawn twice in two sections says nothing about how the two relate.
///
/// Guaranteed to be `None` at `Density::Tight`, where the indent is zero and there is no
/// column to draw in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rail {
    /// No connector: a top-level row, a pinned agent lifted out of its group, or a panel too
    /// narrow to spend a column on one.
    #[default]
    None,
    /// A row with more of its group below it.
    Branch,
    /// The last row of its group, which is where the strand ends.
    End,
    /// A continuation under a row that was not the last: the group carries on beneath.
    Through,
}

impl Rail {
    /// The two columns it occupies, or nothing at all.
    ///
    /// A connector is exactly as wide as `Density::indent`, and the renderer pads the rest of
    /// the indent out, so a row's label starts in the same column whether it has one or not —
    /// a tree whose names do not line up is harder to scan than no tree at all. `None` is
    /// empty rather than two spaces precisely so that a row with no indent to spend does not
    /// pay for a gutter it is not using.
    pub fn glyph(&self) -> &'static str {
        match self {
            Rail::None => "",
            Rail::Branch => "├─",
            Rail::End => "╰─",
            Rail::Through => "│ ",
        }
    }
}

/// What a group header counts.
///
/// Two shapes because two sections have genuinely different things to say. An agent or a
/// service has a *state*, and the header's job is to say how many are in each — glyph-coded,
/// because `◍2` is the one thing you want to see from across the room. A saved note has no
/// state at all; it either exists or it does not, so its header has one number to give and
/// pretending otherwise would mean inventing a state for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tally {
    States(Roll),
    Count(usize),
}

impl Tally {
    /// The states, for a header that has them, and an empty roll for one that does not.
    ///
    /// For readers that only care about agent counts and would otherwise each write the same
    /// `match`. The sidebar does not use it — it renders the two shapes differently, which is
    /// the point of the type.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn roll(&self) -> Roll {
        match self {
            Tally::States(r) => *r,
            Tally::Count(_) => Roll::default(),
        }
    }
}

/// One line of the sidebar's agent list, and the unit a cursor will step over.
#[derive(Debug, Clone, PartialEq)]
pub enum RowKind {
    /// A space's header, standing over its agents.
    Group { space: SpaceId, tally: Tally, collapsed: bool },
    Agent { pane: PaneId, space: SpaceId, pinned: bool },
    /// One of a project's saved notes.
    Memory { id: MemoryId, space: SpaceId },
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
    /// Where this row sits in its group's strand.
    pub rail: Rail,
    /// Whose accent colours the strand — the project this row hangs from.
    ///
    /// Carried on the row rather than re-derived by the renderer because `Activity` names
    /// only a pane, and asking the renderer to walk back up the list to find out which
    /// project that pane is in is exactly the parallel arithmetic this module exists to
    /// prevent.
    pub tint: Option<SpaceId>,
    /// Whether the cursor may land here.
    ///
    /// True for everything except a SERVICES group header, which is drawn — a project's
    /// servers still need a project to hang from — but is not a stop. Two headers for one
    /// project would be two cursor stops that do the identical thing when you press enter,
    /// and, worse, two rows answering to the same `Focus`: the cursor is named by identity
    /// rather than by index precisely so it survives the list being rebuilt, and a `Focus`
    /// that matches two rows would teleport it to the first of them on the next frame.
    stop: bool,
}

impl Row {
    fn new(kind: RowKind) -> Row {
        Row { kind, indent: 0, rail: Rail::None, tint: None, stop: true }
    }

    fn indented(kind: RowKind, indent: u16) -> Row {
        Row { kind, indent, rail: Rail::None, tint: None, stop: true }
    }

    /// Drawn, but not somewhere the cursor stops.
    fn silent(mut self) -> Row {
        self.stop = false;
        self
    }

    fn tinted(mut self, space: SpaceId) -> Row {
        self.tint = Some(space);
        self
    }

    /// Give the row a connector, unless the panel is too narrow to have a column for one.
    fn railed(mut self, rail: Rail) -> Row {
        if self.indent > 0 {
            self.rail = rail;
        }
        self
    }

    /// What the cursor would be on here, if it can land here at all.
    pub fn focus(&self) -> Option<Focus> {
        if !self.stop {
            return None;
        }
        match self.kind {
            RowKind::Group { space, .. } => Some(Focus::Group(space)),
            RowKind::Agent { pane, .. } => Some(Focus::Agent(pane)),
            RowKind::Memory { id, .. } => Some(Focus::Memory(id)),
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
    Memory(MemoryId),
}

impl Focus {
    /// What pressing enter on this does, given where you are.
    ///
    /// `None` where there is nothing sensible to do — pressing enter on a note with no agent
    /// focused. The caller says so rather than this guessing at a target, because handing a
    /// note to the wrong agent is a message you cannot take back out of its context.
    pub fn activate(&self, snap: &Snapshot) -> Option<Cmd> {
        match self {
            // A group header stands for its project, so entering it goes there — the same
            // thing clicking the space row does.
            Focus::Group(s) => Some(Cmd::FocusSpace(*s)),
            Focus::Agent(p) => Some(Cmd::FocusPane(*p)),
            // A note is not somewhere you go, it is something you hand over. The keyboard's
            // answer to the drag: give it to whoever you are looking at.
            Focus::Memory(id) => {
                let to = snap.focused_pane.filter(|p| {
                    snap.panes
                        .iter()
                        .find(|x| x.id == *p)
                        .and_then(|x| x.agent.as_ref())
                        .is_some_and(|a| a.class == AgentClass::Agent)
                })?;
                Some(Cmd::GiveMemory { memory: *id, to })
            }
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
                Focus::Memory(m) => snap.memories.iter().any(|x| x.id == m),
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
    /// Which of the two sections this belongs in. Read from the snapshot rather than
    /// inferred from the state: a dev server that cannot bind its port is `blocked`, and
    /// sorting on state would file it under the agents on its worst day.
    pub class: AgentClass,
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
                    class: a.class,
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

/// Every section, concatenated in the order the cursor walks them.
///
/// The panel draws them as separate blocks with rules between, but the cursor, the key
/// handler and the roster overlay all want one list — and it has to be *the same* list the
/// panel drew, or `j` walks rows that are not on screen. Building it from the same function
/// three times is what guarantees that; deriving it separately is the parallel arithmetic
/// this module exists to prevent.
pub fn cursor_rows(snap: &Snapshot, d: Density, lens: &Lens) -> Vec<Row> {
    let mut out = Vec::new();
    for section in [Section::Agents, Section::Memory, Section::Services] {
        out.extend(filtered_rows(snap, d, lens, section));
    }
    out
}

/// One thing to be grouped under its project, before the grouping is worked out.
///
/// The three sections hold three unrelated kinds of thing — a pane you talk to, a pane that
/// runs, a file on disk — and share every bit of what the sidebar *does* with them: group
/// under a header, hang on the project's strand, lift the pinned ones out, end the strand on
/// the last row. Reducing all three to this is what lets that logic exist once.
struct Item {
    space: SpaceId,
    kind: RowKind,
    /// Its state, for the header's rollup. `None` for a note, which has none.
    state: Option<AgentState>,
    /// A second line to draw beneath it, indented further.
    second: Option<RowKind>,
    pinned: bool,
}

/// One section as a flat list: a header per space that has anything in it, then its rows,
/// filtered by the active lens.
///
/// A space with nothing in this section gets no header. The SPACES list above already names
/// every project; repeating the empty ones here would spend the panel's scarcest resource —
/// rows — on saying nothing twice.
///
/// The lens filters the two agent-shaped sections, and that is deliberate. A service is a
/// thing in the session like any other, so `needs you` has to be able to show a dev server
/// that cannot bind its port — that is precisely a thing you have to go and fix. It follows
/// that `working` empties the SERVICES list, which is also right: "show me what is mid-turn"
/// should not list three processes that will never finish.
///
/// It does **not** filter MEMORY, and that is the one exception worth stating. A lens is a
/// question about what your agents are doing, and a saved note is not doing anything — it is
/// the context you reach for *while* answering that question, so hiding it exactly when you
/// have narrowed down to one blocked agent would take it away at the only moment it is
/// wanted.
pub fn filtered_rows(snap: &Snapshot, d: Density, lens: &Lens, section: Section) -> Vec<Row> {
    let items = match section {
        Section::Memory => memory_items(snap),
        Section::Agents | Section::Services => {
            let want = if section == Section::Agents {
                AgentClass::Agent
            } else {
                AgentClass::Service
            };
            let all: Vec<AgentRow> =
                collect_agents(snap).into_iter().filter(|a| a.class == want).collect();
            if all.is_empty() {
                return empty(section);
            }
            let kept: Vec<AgentRow> =
                all.into_iter().filter(|a| lens.matches(a, snap)).collect();
            if kept.is_empty() {
                return match section {
                    // An empty *filtered* list is a different fact from an empty session, and
                    // has to say which one it is or it reads as everything having stopped.
                    Section::Agents => vec![Row::new(RowKind::Empty("no agents match"))],
                    _ => Vec::new(),
                };
            }
            kept.into_iter()
                .map(|a| Item {
                    space: a.space,
                    kind: RowKind::Agent { pane: a.pane, space: a.space, pinned: a.pinned },
                    state: Some(a.state),
                    second: a.has_activity.then_some(RowKind::Activity(a.pane)),
                    pinned: a.pinned,
                })
                .collect()
        }
    };
    if items.is_empty() {
        return empty(section);
    }
    grouped(snap, d, section, items)
}

/// What an empty section shows.
///
/// Only AGENTS says anything. The others go entirely, chrome included: most sessions never
/// run a dev server or save a note, and a rule and a label announcing that would be three
/// rows spent on a non-event — and when a *lens* emptied them, the AGENTS label above is
/// already saying the list is filtered, three rows further up.
fn empty(section: Section) -> Vec<Row> {
    match section {
        // Say how to get one rather than leaving an unexplained empty panel.
        Section::Agents => vec![Row::new(RowKind::Empty("none yet"))],
        Section::Memory | Section::Services => Vec::new(),
    }
}

/// The saved notes, in the order the daemon listed them — newest first within each project.
fn memory_items(snap: &Snapshot) -> Vec<Item> {
    snap.memories
        .iter()
        .map(|m| Item {
            space: m.space,
            kind: RowKind::Memory { id: m.id, space: m.space },
            state: None,
            second: None,
            pinned: false,
        })
        .collect()
}

/// Group items under a header per project and hang them on that project's strand.
fn grouped(snap: &Snapshot, d: Density, section: Section, items: Vec<Item>) -> Vec<Row> {
    let mut out = Vec::new();

    // Pinned rows lift to the top, out of their groups. The one sanctioned exception to the
    // stable ordering above — and it is not really an exception, because *you* moved these.
    // Rows that reorder themselves are worse than rows you scan; rows you put somewhere are
    // not, since you know where you put them.
    //
    // They keep their project's tint and lose their rail: the colour still says which project
    // it belongs to, while the missing connector says it is not sitting under that project's
    // header any more. Drawing a `├─` here would promise a group above it that is not there.
    for it in items.iter().filter(|i| i.pinned) {
        out.push(Row::new(it.kind.clone()).tinted(it.space));
        if let Some(second) = it.second.clone() {
            out.push(Row::indented(second, 2).tinted(it.space));
        }
    }

    for space in &snap.spaces {
        let mine: Vec<&Item> = items.iter().filter(|i| i.space == space.id).collect();
        if mine.is_empty() {
            continue;
        }
        // The tally counts everything in the space, pinned rows included. Pinning moves a
        // row; it does not change what is running in a project, which is what the header
        // answers.
        let tally = match section {
            Section::Memory => Tally::Count(mine.len()),
            _ => {
                let mut roll = Roll::default();
                for it in &mine {
                    if let Some(state) = it.state {
                        roll.add(state);
                    }
                }
                Tally::States(roll)
            }
        };
        let head = Row::new(RowKind::Group { space: space.id, tally, collapsed: space.collapsed })
            .tinted(space.id);
        out.push(match section {
            Section::Agents => head,
            // Only the first header for a project is a cursor stop — see `Row::stop`.
            _ => head.silent(),
        });
        if space.collapsed {
            continue;
        }
        let body: Vec<&Item> = mine.into_iter().filter(|i| !i.pinned).collect();
        for (i, it) in body.iter().enumerate() {
            // `╰─` only on the row the strand actually ends on. Measured against the body of
            // the group rather than the whole space, because a pinned row is drawn above the
            // header and is not on this strand at all.
            let last = i + 1 == body.len();
            out.push(
                Row::indented(it.kind.clone(), d.indent())
                    .tinted(it.space)
                    .railed(if last { Rail::End } else { Rail::Branch }),
            );
            if let Some(second) = it.second.clone() {
                // The strand has to run *past* a two-line row, or a working agent's activity
                // line would break the connector to everything under it.
                out.push(
                    Row::indented(second, d.indent() + 2)
                        .tinted(it.space)
                        .railed(if last { Rail::None } else { Rail::Through }),
                );
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
                endpoint: None,
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
            cards_due: 0,
            triggers_armed: 0,
            recents: Vec::new(),
            memories: Vec::new(),
        }
    }

    /// A service pane, so the two sections can be told apart in a test rather than by eye.
    pub fn service(id: u32, space: u32, tab: u32, name: &str, state: AgentState) -> PaneInfo {
        let mut p = pane(id, space, tab, Some((name, state)));
        p.agent.as_mut().unwrap().class = AgentClass::Service;
        p
    }

    /// `snap()` plus two dev servers in `api-refactor` and one in `docs`.
    pub fn served() -> Snapshot {
        let mut s = snap();
        s.tabs[0].panes.extend([5, 6]);
        s.tabs[1].panes.push(7);
        s.panes.push(service(5, 1, 1, "vite", AgentState::Serving));
        s.panes.push(service(6, 1, 1, "tsc", AgentState::Serving));
        s.panes.push(service(7, 2, 2, "hugo", AgentState::Serving));
        s
    }

    /// The whole reason the section exists: a project running one agent and three
    /// `npm run dev` panes must not read as a project running four agents.
    #[test]
    fn a_dev_server_is_not_in_the_agent_list() {
        let rows = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Agents);
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::Agent { pane: 5..=7, .. })),
            "{rows:?}"
        );
        let services = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Services);
        let panes: Vec<PaneId> = services
            .iter()
            .filter_map(|r| match r.kind {
                RowKind::Agent { pane, .. } => Some(pane),
                _ => None,
            })
            .collect();
        assert_eq!(panes, vec![5, 6, 7], "{services:?}");
    }

    /// Each section rolls up its own list. A header that counted the whole space would put
    /// the servers back into the agent count by the back door.
    #[test]
    fn a_section_header_counts_only_what_is_under_it() {
        let rows = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Agents);
        let RowKind::Group { tally, .. } = rows[0].kind else { panic!("{:?}", rows[0]) };
        let roll = tally.roll();
        assert_eq!(roll, Roll { blocked: 1, working: 1, done: 0, idle: 0, serving: 0 });

        let rows = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Services);
        let RowKind::Group { tally, .. } = rows[0].kind else { panic!("{:?}", rows[0]) };
        let roll = tally.roll();
        assert_eq!(roll, Roll { blocked: 0, working: 0, done: 0, idle: 0, serving: 2 });
    }

    /// Most sessions never run a dev server, and a rule and a label announcing that would be
    /// three rows spent on a non-event.
    #[test]
    fn no_services_at_all_is_no_section_rather_than_an_empty_one() {
        let rows = filtered_rows(&snap(), Density::Normal, &Lens::All, Section::Services);
        assert!(rows.is_empty(), "{rows:?}");
    }

    /// The AGENTS label three rows up is already saying the list is filtered, so an empty
    /// SERVICES list under it needs no explanation of its own.
    #[test]
    fn a_lens_that_matches_no_service_hides_the_section() {
        let rows = filtered_rows(&served(), Density::Normal, &Lens::Working, Section::Services);
        assert!(rows.is_empty(), "{rows:?}");
    }

    /// A dev server that cannot bind its port is exactly the thing `needs you` is for.
    #[test]
    fn a_blocked_service_survives_the_needs_you_lens() {
        let mut s = served();
        s.panes.iter_mut().find(|p| p.id == 5).unwrap().agent.as_mut().unwrap().state =
            AgentState::Blocked;
        let rows = filtered_rows(&s, Density::Normal, &Lens::NeedsYou, Section::Services);
        assert!(rows.iter().any(|r| matches!(r.kind, RowKind::Agent { pane: 5, .. })), "{rows:?}");
        assert!(!rows.iter().any(|r| matches!(r.kind, RowKind::Agent { pane: 6, .. })), "{rows:?}");
    }

    #[test]
    fn the_strand_ends_on_the_last_row_of_its_group() {
        let rows = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Services);
        let rails: Vec<Rail> = rows.iter().map(|r| r.rail).collect();
        // header, vite, tsc, header, hugo
        assert_eq!(
            rails,
            vec![Rail::None, Rail::Branch, Rail::End, Rail::None, Rail::End],
            "{rows:?}"
        );
    }

    /// An agent with a second line must not break the connector to everything below it.
    #[test]
    fn the_strand_runs_past_a_two_line_row() {
        let mut s = snap();
        let a = s.panes.iter_mut().find(|p| p.id == 1).unwrap().agent.as_mut().unwrap();
        a.activity.tools = 12;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        // header, builder, builder's activity, reviewer, ...
        assert!(matches!(rows[2].kind, RowKind::Activity(1)), "{rows:?}");
        assert_eq!(rows[1].rail, Rail::Branch);
        assert_eq!(rows[2].rail, Rail::Through, "the strand has to reach the reviewer below");
        assert_eq!(rows[3].rail, Rail::End);
    }

    /// The last row's activity line has nothing below it, so the strand stops rather than
    /// trailing off into the section rule.
    #[test]
    fn the_strand_stops_under_the_last_row() {
        let mut s = snap();
        let a = s.panes.iter_mut().find(|p| p.id == 2).unwrap().agent.as_mut().unwrap();
        a.state = AgentState::Working;
        a.activity.tools = 3;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        let at = rows.iter().position(|r| matches!(r.kind, RowKind::Activity(2))).unwrap();
        assert_eq!(rows[at - 1].rail, Rail::End);
        assert_eq!(rows[at].rail, Rail::None, "{rows:?}");
    }

    /// A pinned row is drawn above the header, so a connector would promise a group above it
    /// that is not there. The colour stays: it still belongs to that project.
    #[test]
    fn a_pinned_row_keeps_its_colour_and_loses_its_connector() {
        let mut s = snap();
        s.panes.iter_mut().find(|p| p.id == 2).unwrap().pinned = true;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        assert!(matches!(rows[0].kind, RowKind::Agent { pane: 2, pinned: true, .. }), "{rows:?}");
        assert_eq!(rows[0].rail, Rail::None);
        assert_eq!(rows[0].tint, Some(1));
    }

    /// At the width where the indent yields, so does the connector: there is no column left
    /// to draw it in.
    #[test]
    fn a_tight_panel_draws_no_connector() {
        let rows = filtered_rows(&snap(), Density::Tight, &Lens::All, Section::Agents);
        assert!(rows.iter().all(|r| r.rail == Rail::None), "{rows:?}");
        let wide = filtered_rows(&snap(), Density::Wide, &Lens::All, Section::Agents);
        assert!(wide.iter().any(|r| r.rail != Rail::None), "{wide:?}");
    }

    /// Every row on a project's strand has to name the same project, or the renderer cannot
    /// colour them alike — which is the entire point of the connector.
    #[test]
    fn both_sections_tint_a_project_the_same_way() {
        let agents = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Agents);
        let services = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Services);
        let tint = |rows: &[Row], pane: PaneId| {
            rows.iter()
                .find(|r| matches!(r.kind, RowKind::Agent { pane: p, .. } if p == pane))
                .unwrap()
                .tint
        };
        // builder and vite are both api-refactor's, in two different sections.
        assert_eq!(tint(&agents, 1), Some(1));
        assert_eq!(tint(&services, 5), Some(1));
        assert_eq!(tint(&services, 7), Some(2), "hugo is docs'");
    }

    /// `served()` plus two notes in `api-refactor` and one in `docs`.
    pub fn noted() -> Snapshot {
        let mut s = served();
        let note = |id, space, name: &str| crate::proto::MemoryInfo {
            id,
            space,
            name: name.into(),
            title: format!("about {name}"),
            bytes: 100,
            age: id * 60,
        };
        s.memories = vec![note(1, 1, "api-shape"), note(2, 1, "ruled-out"), note(3, 2, "voice")];
        s
    }

    #[test]
    fn notes_are_grouped_under_their_project_like_everything_else() {
        let rows = filtered_rows(&noted(), Density::Normal, &Lens::All, Section::Memory);
        let kinds: Vec<&RowKind> = rows.iter().map(|r| &r.kind).collect();
        assert!(matches!(kinds[0], RowKind::Group { space: 1, .. }), "{kinds:?}");
        assert!(matches!(kinds[1], RowKind::Memory { id: 1, space: 1 }));
        assert!(matches!(kinds[2], RowKind::Memory { id: 2, space: 1 }));
        assert!(matches!(kinds[3], RowKind::Group { space: 2, .. }));
        assert!(matches!(kinds[4], RowKind::Memory { id: 3, space: 2 }));
    }

    /// A note has no state, so counting it as one would mean inventing a state for a file.
    #[test]
    fn a_memory_header_counts_rather_than_rolling_up_states() {
        let rows = filtered_rows(&noted(), Density::Normal, &Lens::All, Section::Memory);
        let RowKind::Group { tally, .. } = rows[0].kind else { panic!("{:?}", rows[0]) };
        assert_eq!(tally, Tally::Count(2));
    }

    /// Notes hang on the same coloured strand as the agents they are for, in a different
    /// section — which is the whole reason the connector exists.
    #[test]
    fn a_note_is_on_the_same_strand_as_its_project_s_agents() {
        let snap = noted();
        let agents = filtered_rows(&snap, Density::Normal, &Lens::All, Section::Agents);
        let notes = filtered_rows(&snap, Density::Normal, &Lens::All, Section::Memory);
        let builder = agents
            .iter()
            .find(|r| matches!(r.kind, RowKind::Agent { pane: 1, .. }))
            .unwrap();
        let api_shape =
            notes.iter().find(|r| matches!(r.kind, RowKind::Memory { id: 1, .. })).unwrap();
        assert_eq!(builder.tint, api_shape.tint);
        assert_eq!(notes.last().unwrap().rail, Rail::End);
    }

    /// A lens is a question about what your agents are doing. A note is not doing anything —
    /// it is the context you reach for *while* answering that question, so narrowing down to
    /// one blocked agent must not take it away at the moment it is wanted.
    #[test]
    fn a_lens_never_hides_the_notes() {
        for lens in [Lens::NeedsYou, Lens::Working, Lens::Role("nobody".into())] {
            let rows = filtered_rows(&noted(), Density::Normal, &lens, Section::Memory);
            let n = rows.iter().filter(|r| matches!(r.kind, RowKind::Memory { .. })).count();
            assert_eq!(n, 3, "{lens:?} hid the notes: {rows:?}");
        }
    }

    /// Most projects have never saved one, and a rule and a label saying so would be three
    /// rows spent on a non-event.
    #[test]
    fn a_project_with_no_notes_gets_no_memory_section() {
        let rows = filtered_rows(&served(), Density::Normal, &Lens::All, Section::Memory);
        assert!(rows.is_empty(), "{rows:?}");
    }

    /// Enter on a note hands it to whoever you are looking at — and declines rather than
    /// guessing when that is not an agent, because a note in the wrong agent's context cannot
    /// be taken back out.
    #[test]
    fn a_note_is_handed_to_the_focused_agent_or_to_nobody() {
        let mut s = noted();
        s.focused_pane = Some(1); // builder
        assert_eq!(
            Focus::Memory(1).activate(&s),
            Some(Cmd::GiveMemory { memory: 1, to: 1 })
        );
        s.focused_pane = Some(3); // a bare shell
        assert_eq!(Focus::Memory(1).activate(&s), None);
        s.focused_pane = Some(5); // vite, a service
        assert_eq!(Focus::Memory(1).activate(&s), None, "a server has no conversation");
    }

    /// The cursor is named by identity so it survives the list being rebuilt. A note that has
    /// been deleted must release it rather than holding it on a row that is gone.
    #[test]
    fn a_deleted_note_releases_the_cursor() {
        let mut st = SidebarState { cursor: Some(Focus::Memory(2)), ..Default::default() };
        let mut s = noted();
        st.prune(&s);
        assert_eq!(st.cursor, Some(Focus::Memory(2)));
        s.memories.retain(|m| m.id != 2);
        st.prune(&s);
        assert_eq!(st.cursor, None);
    }

    /// The list the cursor walks runs agents first, then services, so `G` lands on the last
    /// server rather than the last agent — the panel and the key handler agree about what is
    /// below what.
    #[test]
    fn the_cursor_walks_out_of_the_agents_and_into_the_servers() {
        let rows = cursor_rows(&served(), Density::Normal, &Lens::All);
        let mut st = SidebarState::default();
        st.jump(&rows, true);
        assert_eq!(st.cursor, Some(Focus::Agent(7)), "{rows:?}");
        // And back up through the services header, which is drawn but is not a stop.
        st.step(&rows, -1);
        assert_eq!(st.cursor, Some(Focus::Agent(6)));
    }

    /// Two headers for one project would be two rows answering to the same `Focus`, and the
    /// cursor — which is named by identity so it survives the list being rebuilt — would
    /// teleport to the first of them on the next frame.
    #[test]
    fn the_cursor_never_lands_on_a_services_header() {
        let rows = cursor_rows(&served(), Density::Normal, &Lens::All);
        let mut seen = Vec::new();
        for f in rows.iter().filter_map(Row::focus) {
            assert!(!seen.contains(&f), "{f:?} answers to two rows: {rows:?}");
            seen.push(f);
        }
        assert!(seen.contains(&Focus::Agent(5)), "a server is still a stop: {seen:?}");
    }

    #[test]
    fn every_agent_sits_under_a_header_for_its_own_space() {
        let rows = filtered_rows(&snap(), Density::Normal, &Lens::All, Section::Agents);
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
        let rows = filtered_rows(&snap(), Density::Normal, &Lens::All, Section::Agents);
        let RowKind::Group { tally, .. } = rows[0].kind else { panic!("{:?}", rows[0]) };
        let roll = tally.roll();
        assert_eq!(roll, Roll { blocked: 1, working: 1, done: 0, idle: 0, serving: 0 });
    }

    /// A space with no agents already appears in SPACES; a second empty row here would spend
    /// the panel's scarcest resource saying nothing twice.
    #[test]
    fn a_space_with_no_agents_gets_no_header() {
        let mut s = snap();
        s.panes.retain(|p| p.space != 2);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::Group { space: 2, .. })),
            "{rows:?}"
        );
    }

    #[test]
    fn no_agents_at_all_says_so_rather_than_returning_nothing() {
        let mut s = snap();
        s.panes.iter_mut().for_each(|p| p.agent = None);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Empty("none yet")));
    }

    /// Narrow panels drop the indent: at that width it costs more than it explains, and the
    /// header above still carries the grouping.
    #[test]
    fn a_tight_panel_groups_without_indenting() {
        let wide = filtered_rows(&snap(), Density::Wide, &Lens::All, Section::Agents);
        let tight = filtered_rows(&snap(), Density::Tight, &Lens::All, Section::Agents);
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
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
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
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
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
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        let RowKind::Group { tally, .. } = rows[0].kind else { panic!() };
        let roll = tally.roll();
        assert_eq!(roll, Roll { blocked: 1, working: 1, done: 0, idle: 0, serving: 0 });
    }

    #[test]
    fn a_pinned_agent_is_lifted_out_of_its_group() {
        let mut s = snap();
        s.panes.iter_mut().find(|p| p.id == 4).unwrap().pinned = true;
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
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
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        let g = rows
            .iter()
            .find_map(|r| match r.kind {
                RowKind::Group { space: 1, tally, .. } => Some(tally.roll()),
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
        filtered_rows(&snap(), Density::Normal, &Lens::All, Section::Agents)
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
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
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
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        let mut st = SidebarState::default();
        st.resolve(&rows);
        st.step(&rows, 3); // onto docs' header
        assert_eq!(st.cursor, Some(Focus::Group(2)));

        s.spaces.retain(|x| x.id != 2);
        s.panes.retain(|p| p.space != 2);
        st.prune(&s);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        let at = st.resolve(&rows).expect("the cursor must land somewhere live");
        assert!(rows[at].focus().is_some());
        // Near where it was, not back at the top.
        assert_eq!(st.cursor, Some(Focus::Agent(2)), "{rows:?}");
    }

    #[test]
    fn the_cursor_survives_its_agent_exiting() {
        let mut s = snap();
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        let mut st = SidebarState::default();
        st.cursor = Some(Focus::Agent(2));
        st.resolve(&rows);

        s.panes.retain(|p| p.id != 2);
        st.prune(&s);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        assert!(st.resolve(&rows).is_some());
        assert_ne!(st.cursor, Some(Focus::Agent(2)));
    }

    /// An empty list has nothing to point at, and must say so rather than panicking on an
    /// index into it.
    #[test]
    fn a_cursor_on_an_empty_list_resolves_to_nothing() {
        let mut s = snap();
        s.panes.iter_mut().for_each(|p| p.agent = None);
        let rows = filtered_rows(&s, Density::Normal, &Lens::All, Section::Agents);
        let mut st = SidebarState::default();
        assert_eq!(st.resolve(&rows), None);
        st.step(&rows, 1);
        st.jump(&rows, true);
        assert_eq!(st.cursor, None);
    }

    #[test]
    fn enter_goes_to_the_space_for_a_header_and_the_pane_for_an_agent() {
        assert_eq!(Focus::Group(3).activate(&snap()), Some(Cmd::FocusSpace(3)));
        assert_eq!(Focus::Agent(7).activate(&snap()), Some(Cmd::FocusPane(7)));
    }

    fn lensed(s: &Snapshot, l: Lens) -> Vec<Row> {
        filtered_rows(s, Density::Normal, &l, Section::Agents)
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
