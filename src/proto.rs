//! Wire types shared by the daemon and the client.
//!
//! Two channels live on one socket:
//!
//! * **Control** — newline-delimited JSON (`Request`/`Response`). Used by the CLI and by
//!   agents, so it stays readable and debuggable with `nc`.
//! * **Render** — once a connection sends the `attach` method it stops speaking JSON and
//!   switches to length-prefixed `postcard` frames (`ClientFrame`/`ServerFrame`) in both
//!   directions. Shipping cell grids as JSON at 60fps is wasteful.

use serde::{Deserialize, Serialize};

pub type SpaceId = u32;
pub type TabId = u32;
pub type PaneId = u32;

/// Bumped when the wire format changes incompatibly. The client refuses to attach to a
/// daemon that disagrees, which is the whole reason both halves ship in one binary.
/// Bumped whenever a client and daemon of different builds can no longer understand each other.
///
/// Postcard is positional on both counts — struct fields by order, enum variants by index — so
/// *adding* a field to `Snapshot`, `PaneInfo`, or `Digest`, or a variant anywhere in `Cmd` or
/// `ServerFrame`, is a wire-format change even though `serde(default)` makes it look additive.
/// The attach handshake compares this number over newline JSON, before either side switches to
/// postcard, which is why it can report the mismatch instead of failing to parse it.
pub const PROTOCOL_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Control channel
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: impl Into<String>, result: serde_json::Value) -> Self {
        Self { id: id.into(), result: Some(result), error: None }
    }

    pub fn err(id: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            result: None,
            error: Some(RpcError { code: code.to_string(), message: message.into() }),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// A rectangle in the client's terminal coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Shrink by `n` on every side. Saturates rather than underflowing, so a 1x1 rect
    /// inset by 1 yields a zero-size rect instead of wrapping.
    pub fn inset(&self, n: u16) -> Rect {
        Rect {
            x: self.x.saturating_add(n),
            y: self.y.saturating_add(n),
            w: self.w.saturating_sub(n * 2),
            h: self.h.saturating_sub(n * 2),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

// ---------------------------------------------------------------------------
// Cell data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

pub mod attrs {
    pub const BOLD: u8 = 1 << 0;
    pub const DIM: u8 = 1 << 1;
    pub const ITALIC: u8 = 1 << 2;
    pub const UNDERLINE: u8 = 1 << 3;
    // No REVERSE bit: the daemon resolves inverse video by swapping fg/bg before the
    // run goes on the wire, so the client never has to know about it.
    pub const STRIKEOUT: u8 = 1 << 5;
    pub const HIDDEN: u8 = 1 << 6;
}

/// A run of cells sharing one style. Terminal rows are overwhelmingly long stretches of
/// one style, so run-length encoding by style keeps frames small without any real cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub text: String,
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Row {
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowUpdate {
    pub y: u16,
    pub row: Row,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CursorPos {
    pub x: u16,
    pub y: u16,
    /// False when the pane hides its cursor, or when the pane isn't focused.
    pub visible: bool,
}

// ---------------------------------------------------------------------------
// Session shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Working,
    Blocked,
    Done,
    Idle,
    Unknown,
}

impl AgentState {
    /// The glyph shown in the sidebar and pane title. Deliberately plain Unicode
    /// geometrics — no Nerd Font dependency, so nothing renders as a replacement box.
    pub fn glyph(&self) -> &'static str {
        match self {
            AgentState::Working => "◐",
            AgentState::Blocked => "◍",
            AgentState::Done => "●",
            AgentState::Idle => "○",
            AgentState::Unknown => "◌",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
            AgentState::Unknown => "unknown",
        }
    }

    /// Whether this state should pull the user's eye.
    pub fn needs_attention(&self) -> bool {
        matches!(self, AgentState::Blocked | AgentState::Done)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub tab: TabId,
    pub space: SpaceId,
    /// User-assigned name, else the agent name, else the command.
    pub title: String,
    pub cwd: String,
    /// Cell rect including the pane's border. Content is this inset by 1.
    pub cell: Rect,
    pub content: Rect,
    pub cols: u16,
    pub rows: u16,
    /// Present only when an agent was detected in this pane.
    pub agent: Option<AgentInfo>,
    /// The trigger that started this pane, when horde started it rather than you.
    #[serde(default)]
    pub spawned_by: Option<u64>,
    pub exited: bool,
    /// Rows of scrollback currently scrolled above the live view. 0 = at the bottom.
    pub scroll_offset: usize,
    /// The program asked for mouse reporting, so the client should forward mouse events
    /// instead of using them for horde's own UI.
    pub wants_mouse: bool,
    /// The program enabled bracketed paste, so a multi-line paste must be wrapped rather
    /// than submitted line by line.
    pub bracketed_paste: bool,
}

/// What an agent has actually been doing, counted from its lifecycle hooks.
///
/// Screen detection can see *that* an agent is busy; only the hooks can see what it is busy
/// with. Counters reset when a new turn starts, so these describe the turn in progress (or
/// the one that just finished), not the whole session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// Tool calls in this turn.
    pub tools: u32,
    /// Distinct files touched in this turn.
    pub files: u32,
    /// Tool calls that failed.
    pub errors: u32,
    /// Turns completed since the agent started.
    pub turns: u32,
    /// The tool most recently started, for a live "what is it doing" readout.
    pub last_tool: Option<String>,
}

impl Activity {
    /// Compact one-line summary, or None when nothing has been recorded.
    ///
    /// A sidebar is about 20 columns wide, so this shows two facts, not three. Failures
    /// outrank the file count: one is something you may need to act on, the other is
    /// texture.
    pub fn summary(&self) -> Option<String> {
        if self.tools == 0 && self.files == 0 {
            return None;
        }
        let tools = format!("{} tools", self.tools);
        Some(match (self.errors, self.files) {
            (0, 0) => tools,
            (0, f) => format!("{tools} · {f} files"),
            (e, _) => format!("{tools} · {e} failed"),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Detected kind, e.g. "claude".
    pub kind: String,
    /// Addressable name used by `horde send`. Defaults to the kind, uniquified.
    pub name: String,
    pub state: AgentState,
    /// Seconds in the current state.
    pub elapsed: u64,
    /// Which tier decided the state: "hook" or "screen".
    pub authority: String,
    /// Why the state was chosen — the matched rule name, or a fallback reason.
    pub reason: String,
    /// Counted from lifecycle hooks; empty when no integration is installed.
    #[serde(default)]
    pub activity: Activity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: TabId,
    pub space: SpaceId,
    pub name: String,
    pub panes: Vec<PaneId>,
    pub focused_pane: Option<PaneId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub id: SpaceId,
    pub name: String,
    pub cwd: String,
    pub tabs: Vec<TabId>,
    pub focused_tab: Option<TabId>,
    /// Cheap rollup so the sidebar doesn't have to walk every pane.
    pub agent_count: usize,
    pub attention_count: usize,
}

/// Everything the client needs to draw a frame apart from cell contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol: u32,
    /// Build version of the daemon. The client compares this against its own: a mismatch
    /// means a daemon from an older binary is still running, which is easy to cause (the
    /// daemon outlives every client and survives rebuilds) and confusing to diagnose.
    pub daemon_version: String,
    pub spaces: Vec<SpaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
    pub focused_space: Option<SpaceId>,
    pub focused_tab: Option<TabId>,
    pub focused_pane: Option<PaneId>,
    pub view: ViewState,
    /// Chrome rects the daemon laid out, so the client never computes geometry.
    pub sidebar: Rect,
    pub bus: Rect,
    pub status: Rect,
    pub tabbar: Rect,
    /// Open and claimed task counts, so the sidebar can show the board without another call.
    #[serde(default)]
    pub tasks_open: usize,
    #[serde(default)]
    pub tasks_claimed: usize,
    /// Triggers that could fire right now — enabled, with the master switch on.
    ///
    /// Zero when `unattended` is off, however many rules exist: the question the sidebar is
    /// answering is "can this thing act on its own", and a disarmed switch means no.
    #[serde(default)]
    pub triggers_armed: usize,
}

/// Panel visibility. Lives in the daemon so geometry has exactly one owner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ViewState {
    pub sidebar_open: bool,
    pub bus_open: bool,
    pub sidebar_width: u16,
    pub bus_width: u16,
    pub zoom: Option<PaneId>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            sidebar_open: true,
            bus_open: false,
            sidebar_width: 24,
            bus_width: 30,
            zoom: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Bus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Held because the target was mid-stream; will flush when it goes idle.
    Queued,
    Delivered,
    /// Target vanished before the message could be flushed.
    Dropped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    /// Unix millis.
    pub ts: u64,
    pub from: String,
    pub to: String,
    pub body: String,
    pub delivery: Delivery,
    pub broadcast: bool,
    /// The sender is blocked waiting for a reply, so the recipient is told exactly how to
    /// send one. This is what turns delegation into a call rather than a hope.
    #[serde(default)]
    pub expects_reply: bool,
    /// Set on a reply, naming the request it answers.
    #[serde(default)]
    pub reply_to: Option<u64>,
}

impl Message {
    /// How this message should be introduced to its recipient.
    pub fn kind(&self) -> MsgKind {
        match (self.expects_reply, self.reply_to) {
            (_, Some(_)) => MsgKind::Reply,
            (true, None) => MsgKind::Request,
            _ => MsgKind::Plain,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Plain,
    Request,
    Reply,
}

// ---------------------------------------------------------------------------
// Render channel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientFrame {
    /// Raw keystrokes for the focused pane's PTY.
    Input { pane: PaneId, bytes: Vec<u8> },
    /// The client's whole terminal size changed. The daemon re-lays out and resizes PTYs.
    Resize { cols: u16, rows: u16 },
    Focus { pane: PaneId },
    Command(Cmd),
    Ping,
    Detach,
}

/// Actions a client can ask for. Distinct from the JSON control methods because these are
/// interactive and fire far more often.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Cmd {
    SplitRight,
    SplitDown,
    ClosePane,
    FocusDir(Dir),
    Resize { dir: Dir, cells: u16 },
    ToggleZoom,
    SwapDir(Dir),
    NewTab,
    NextTab,
    PrevTab,
    GotoTab(usize),
    CloseTab,
    NewSpace { name: Option<String> },
    FocusSpace(SpaceId),
    NextSpace,
    PrevSpace,
    ToggleSidebar,
    ToggleBus,
    /// Jump to the next agent that is blocked or done.
    JumpAttention,
    Scroll { pane: PaneId, lines: i32 },
    ScrollBottom { pane: PaneId },
    FocusPane(PaneId),
    RenamePane { pane: PaneId, name: String },
    SpawnAgent { cmd: String, name: Option<String>, split: Option<Dir> },
    ApplyLayout { preset: String },
    RenameSpace { space: SpaceId, name: String },
    RenameTab { tab: TabId, name: String },
    CloseSpace(SpaceId),
    FocusTab(TabId),
    /// New tab in a specific space, rather than the focused one.
    NewTabIn(SpaceId),
    /// Ask for a digest. The daemon answers with [`ServerFrame::Digest`] to the asking client
    /// only — unlike every other command, this one has a result rather than an effect.
    RequestDigest,
    /// Make every program on screen repaint at the size it actually has.
    Redraw,
}

// Add new `Cmd` variants at the end of the enum, never in the middle. Frames travel as postcard,
// which identifies a variant by its *index* — inserting one silently renumbers everything below
// it, so a client one build ahead of the daemon sends `FocusPane` and the daemon decodes some
// other variant, reads a field that is not there, and drops the connection. Found the hard way:
// clicking a pane killed the session while every keybinding above the inserted variant still
// worked.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn is_horizontal(&self) -> bool {
        matches!(self, Dir::Left | Dir::Right)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerFrame {
    /// Session shape changed (or the client just attached). Always precedes the first Rows.
    Snapshot(Box<Snapshot>),
    /// Damaged rows for one pane. On attach the daemon sends every row.
    Rows { pane: PaneId, rows: Vec<RowUpdate>, cursor: Option<CursorPos> },
    Event(Event),
    /// Answer to [`Cmd::RequestDigest`], for the overlay.
    Digest(Box<Digest>),
    /// Protocol mismatch or daemon shutting down.
    Bye { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    AgentStateChanged { pane: PaneId, name: String, from: AgentState, to: AgentState },
    BusMessage(Message),
    PaneExited { pane: PaneId, status: i32 },
    Notice { level: NoticeLevel, text: String },
}


// -- digest ---------------------------------------------------------------
// "What happened while I was away", assembled by the daemon and rendered by both the CLI and
// the TUI overlay. The assembly lives in `daemon::digest`; only the shape is shared.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Digest {
    /// Window start, unix millis.
    pub since: u64,
    pub now: u64,
    /// True when the window is "since the daemon started" because no digest has been read
    /// yet. Without this a fresh session reads as "nothing new since you last looked", which
    /// claims a read that never happened.
    pub fresh: bool,
    /// Agents wanting a human right now, most urgent thing in the whole digest.
    pub needs_you: Vec<AgentLine>,
    /// Agents that finished a turn while you were not looking.
    pub finished: Vec<AgentLine>,
    /// Still going.
    pub working: Vec<AgentLine>,
    /// Panes that exited during the window.
    pub gone: Vec<String>,
    /// Warnings the daemon raised while nobody was watching them.
    pub warnings: Vec<String>,
    /// Triggers that fired in the window, and what each did.
    ///
    /// The answer to "what did this thing decide to do while I was gone", which is the question
    /// an unattended daemon has to be able to answer to be worth arming.
    #[serde(default)]
    pub fired: Vec<String>,
    pub tasks_done: Vec<TaskLine>,
    pub tasks_added: usize,
    pub tasks_open: usize,
    pub tasks_claimed: usize,
    /// Messages routed in the window, newest last.
    pub messages: Vec<Message>,
    /// Turns any agent completed in the window, counted from the journal so it still counts
    /// the work of an agent that has since exited.
    pub turns: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLine {
    pub name: String,
    pub state: AgentState,
    /// Seconds in the current state.
    pub elapsed: u64,
    /// Whatever the hooks recorded for the current turn, when installed.
    pub activity: Option<String>,
    /// Matched rule or hook reason, so a surprising line can be explained.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLine {
    pub id: u64,
    pub text: String,
    pub owner: Option<String>,
    pub result: Option<String>,
    /// True for a task that was dropped rather than completed.
    pub dropped: bool,
}

impl Digest {
    /// True when there is genuinely nothing to report, so callers can stay quiet instead of
    /// printing an empty report.
    pub fn is_empty(&self) -> bool {
        self.needs_you.is_empty()
            && self.finished.is_empty()
            && self.gone.is_empty()
            && self.warnings.is_empty()
            && self.tasks_done.is_empty()
            && self.messages.is_empty()
            && self.fired.is_empty()
            && self.tasks_added == 0
    }

    /// One line for a toast or status bar, or None when there is nothing worth saying.
    ///
    /// Ordered by what would make you act: a blocked agent first, then finished work, then
    /// traffic. Only the first two facts are shown — a toast nobody can read at a glance is
    /// no better than no toast.
    pub fn headline(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        match self.needs_you.len() {
            0 => {}
            1 => parts.push("1 agent needs you".into()),
            n => parts.push(format!("{n} agents need you")),
        }
        if !self.finished.is_empty() {
            parts.push(format!("{} finished", self.finished.len()));
        }
        if !self.tasks_done.is_empty() {
            parts.push(format!("{} done", plural(self.tasks_done.len(), "task")));
        }
        if !self.messages.is_empty() {
            parts.push(plural(self.messages.len(), "message"));
        }
        if !self.gone.is_empty() {
            parts.push(format!("{} exited", self.gone.len()));
        }
        // Firings are deliberately not here. A notification should carry what the work came to,
        // not that a schedule went off — the finished tasks and the blocked agents above are the
        // outcomes, and this line is only two facts wide.
        if parts.is_empty() {
            return None;
        }
        parts.truncate(2);
        Some(format!("while you were away: {}", parts.join(", ")))
    }
}

/// `1 task` / `3 tasks`. A toast that says "1 messages" reads as a bug in the tool.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    fn line(name: &str, state: AgentState, elapsed: u64) -> AgentLine {
        AgentLine {
            name: name.into(),
            state,
            elapsed,
            activity: None,
            reason: "test".into(),
        }
    }

    fn empty() -> Digest {
        Digest {
            since: 0,
            now: 1000,
            fresh: false,
            needs_you: vec![],
            finished: vec![],
            working: vec![],
            gone: vec![],
            warnings: vec![],
            fired: vec![],
            tasks_done: vec![],
            tasks_added: 0,
            tasks_open: 0,
            tasks_claimed: 0,
            messages: vec![],
            turns: 0,
        }
    }

    #[test]
    fn nothing_to_report_is_reported_as_nothing() {
        assert!(empty().is_empty());
        assert_eq!(empty().headline(), None);
    }

    /// An agent still working is not news you missed — it is the current state, visible in
    /// the sidebar. Only the digest's own findings make it non-empty.
    #[test]
    fn a_working_agent_alone_does_not_make_a_digest() {
        let mut d = empty();
        d.working.push(line("builder", AgentState::Working, 30));
        assert!(d.is_empty());
    }

    #[test]
    fn headline_leads_with_what_needs_a_human() {
        let mut d = empty();
        d.messages = vec![];
        d.tasks_done.push(TaskLine {
            id: 1,
            text: "t".into(),
            owner: None,
            result: None,
            dropped: false,
        });
        d.needs_you.push(line("reviewer", AgentState::Blocked, 90));
        let h = d.headline().unwrap();
        assert!(h.starts_with("while you were away: 1 agent needs you"), "{h}");
    }

    #[test]
    fn a_single_item_is_not_pluralised() {
        let mut d = empty();
        d.tasks_done.push(TaskLine {
            id: 1,
            text: "t".into(),
            owner: None,
            result: None,
            dropped: false,
        });
        assert_eq!(d.headline().unwrap(), "while you were away: 1 task done");
        d.needs_you.push(line("a", AgentState::Blocked, 1));
        let h = d.headline().unwrap();
        assert!(h.contains("1 agent needs you"), "{h}");
    }

    #[test]
    fn headline_shows_two_facts_not_five() {
        let mut d = empty();
        d.needs_you.push(line("a", AgentState::Blocked, 1));
        d.finished.push(line("b", AgentState::Done, 1));
        d.tasks_done.push(TaskLine {
            id: 1,
            text: "t".into(),
            owner: None,
            result: None,
            dropped: false,
        });
        d.gone.push("c".into());
        let h = d.headline().unwrap();
        assert_eq!(h.matches(',').count(), 1, "too much for a toast: {h}");
    }
}
