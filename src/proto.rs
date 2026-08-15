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
pub const PROTOCOL_VERSION: u32 = 14;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A long-running process that is up and holding: a dev server, a watcher, a tunnel.
    ///
    /// Deliberately not `working`. Both mean "busy", but they mean opposite things to a
    /// person reading the sidebar: `working` is a turn that will end and produce something,
    /// and its count is how much of your fleet is mid-thought. A dev server is never going
    /// to finish, so counting it there is how three panes of `npm run dev` make a quiet
    /// session look busy. Appended rather than inserted — see `cmd_variants_are_append_only`.
    Serving,
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
            // A diamond rather than another circle: the circles are one agent's turn moving
            // through its states, and a service is not in that cycle at all.
            AgentState::Serving => "◆",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
            AgentState::Unknown => "unknown",
            AgentState::Serving => "serving",
        }
    }

    /// Whether this state should pull the user's eye.
    pub fn needs_attention(&self) -> bool {
        matches!(self, AgentState::Blocked | AgentState::Done)
    }
}

/// What git says about a pane's directory, or a project's.
///
/// Present only when the directory is a repository. The client never sees a path, so
/// `worktree` has to be decided daemon-side: it is the difference between "this agent is
/// isolated" and "this agent is editing the same files as its neighbour".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub branch: String,
    /// Tracked files differ from HEAD. Untracked files do not count, or a build directory
    /// would leave every project permanently marked.
    pub dirty: bool,
    /// One of horde's own agent worktrees, made by `horde spawn --worktree`.
    pub worktree: bool,
}

/// A question a blocked agent is waiting on, lifted off its screen.
///
/// On the wire because only the daemon can see it: the client renders the panes in the
/// focused tab and nothing else, so the six agents blocked in other tabs are exactly the ones
/// it could never read for itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    /// What can be pressed to answer, in the order the agent listed them.
    pub options: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// The key that picks it: a digit for a menu, `y`/`n` for a plain prompt.
    pub key: String,
    pub label: String,
}

/// What kind of thing horde recognised in a pane.
///
/// The states are shared — a dev server that cannot bind its port is `blocked` in exactly the
/// sense an agent waiting on a permission prompt is — but almost everything horde does *with*
/// an agent (deliver a bus message, hand it board work, derive `done` from a finish it did not
/// watch) is meaningless for a service, so the two have to be distinguishable at the point
/// those decisions are made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentClass {
    /// Something you can talk to and give work to.
    #[default]
    Agent,
    /// Something that runs until you stop it: `npm run dev`, `vite`, a watcher, a tunnel.
    Service,
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
    /// What this pane is *for*, when you have said. Independent of `title` and of
    /// `agent.kind`, and present on panes with no agent at all — a pane can be labelled
    /// before the program in it has booted far enough to be recognised.
    #[serde(default)]
    pub role: Option<String>,
    /// Held at the top of the sidebar's agent list, whichever space it lives in.
    #[serde(default)]
    pub pinned: bool,
    /// This agent takes work from its project's board, so the nudge may interrupt it.
    ///
    /// On the wire because "why did that agent start doing something" has to be answerable
    /// from the UI. An agent that is not enlisted is never told about the board at all.
    #[serde(default)]
    pub board: bool,
    /// The branch this pane's directory is on, when it is in a repository at all.
    ///
    /// Per pane rather than only per space, because that is the whole point of a worktree:
    /// two agents in one project are on two different branches, and a project-level answer
    /// would report the main tree's branch for both of them.
    #[serde(default)]
    pub repo: Option<RepoInfo>,
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
    /// Whether this is something you talk to or something that just runs.
    #[serde(default)]
    pub class: AgentClass,
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
    /// What it is waiting on, when it is blocked and the prompt could be read.
    #[serde(default)]
    pub question: Option<Question>,
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
    /// Slot in the theme's project ramp. The client turns it into a colour with *its* theme,
    /// which is the whole reason this is an index and not a hex string.
    ///
    /// The `serde(default)` buys nothing on the render channel — postcard is positional, so
    /// this field is a protocol break regardless. It is here for the JSON control channel,
    /// where `space.list` and `session.snapshot` serialise this same struct.
    #[serde(default)]
    pub accent: u8,
    /// The sidebar folds this space's agents away.
    #[serde(default)]
    pub collapsed: bool,
    /// What git says about the project directory. The main tree's branch, which is a
    /// different question from what any one agent is working on: see `PaneInfo::repo`.
    #[serde(default)]
    pub repo: Option<RepoInfo>,
    /// Notes indexed for this project, or `None` when it has no vault at all.
    ///
    /// A count rather than the notes: this struct is broadcast on every shape change, and a
    /// vault's contents are unbounded. The notes themselves come back from a query.
    #[serde(default)]
    pub notes: Option<usize>,
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
    /// Projects opened before, most recent first — the dashboard's memory.
    ///
    /// Kept by the daemon rather than the client because the daemon is the thing that
    /// outlives every attach, and because a client on another machine has no business
    /// deciding what this session has been working on.
    #[serde(default)]
    pub recents: Vec<RecentProject>,
}

/// A project the session has opened before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentProject {
    /// The space's name when it was last open, which is usually the directory's.
    pub name: String,
    pub cwd: String,
    /// Unix millis, for ordering and for "3 days ago".
    pub last_used: u64,
    /// True while a space is open on it, so the dashboard can say "3 agents" instead of
    /// offering to reopen something already on screen.
    #[serde(default)]
    pub live: bool,
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
    /// Retint a space. `slot` wraps into the theme's project ramp; `None` means "the next one
    /// along", so a caller with no snapshot in hand can still cycle.
    SetSpaceAccent { space: SpaceId, slot: Option<u8> },
    /// Give a pane a job, or clear it with an empty string — the same contract `RenamePane`
    /// uses, so there is one rule for "a text field a menu can empty".
    SetPaneRole { pane: PaneId, role: String },
    /// Fold a space's agents away in the sidebar.
    ToggleSpaceCollapsed(SpaceId),
    /// Hold a pane at the top of the sidebar's agent list.
    TogglePanePinned(PaneId),
    /// Ask what is in a project's vault. Answered with [`ServerFrame::Vault`] to the asking
    /// client only — the second command with a result rather than an effect, after
    /// `RequestDigest`, and for the same reason: note lists are unbounded and change often,
    /// so they have no business riding a snapshot everyone gets.
    VaultQuery { space: SpaceId, kind: VaultQuery },
    /// Write a note, creating it if it is not there. Answered with the saved note, so the
    /// view it came from can show what is now on disk rather than what it hoped was.
    VaultSave { space: SpaceId, path: String, body: String },
    /// Set up a vault where there is not one yet: the directory, its marker, a first note.
    VaultInit { space: SpaceId },
    /// What is in a project: its files, so opening one shows the project rather than a
    /// terminal that happens to be sitting in it.
    FileQuery { space: SpaceId },
    /// Write a project file. Relative to the project root, and jailed to it.
    FileSave { space: SpaceId, path: String, body: String },
    /// One project file's text, for editing.
    FileRead { space: SpaceId, path: String },
    /// Show a file in a pane of its own, beside whatever is already there.
    OpenDocPane { space: SpaceId, path: String },
    /// Open a project directory: focus the space already on it, or make one if there is none.
    ///
    /// Focus-or-create rather than always-create, because the dashboard offers live projects
    /// and remembered ones in the same list, and pressing enter on the one you are already
    /// running should take you there rather than start a second copy of it.
    OpenProject { cwd: String },
}

// Add new `Cmd` variants at the end of the enum, never in the middle. Frames travel as postcard,
// which identifies a variant by its *index* — inserting one silently renumbers everything below
// it, so a client one build ahead of the daemon sends `FocusPane` and the daemon decodes some
// other variant, reads a field that is not there, and drops the connection. Found the hard way:
// clicking a pane killed the session while every keybinding above the inserted variant still
// worked.

/// What to ask a vault. Append new kinds at the end, like every other wire enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VaultQuery {
    /// Every note, newest first.
    List,
    /// Notes matching a query, best first.
    Search { q: String },
    /// One note in full, with its backlinks.
    Note { path: String },
    /// The whole link graph: what to draw, not where to draw it.
    ///
    /// Positions are the client's business. The daemon knows which notes link to which, and
    /// a layout is a property of the terminal it is being drawn in — sending coordinates
    /// would mean the daemon owning a screen it cannot see.
    Graph,
}

/// One note in the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Empty for a ghost — a link target nobody has written yet.
    pub path: String,
    pub label: String,
    /// Links in plus links out, which is what decides how big a node draws.
    pub degree: u16,
    /// Groups nodes by their first tag, else their folder, so a cluster has a colour.
    pub group: String,
    /// A link target with no note behind it. Drawn hollow: it is a note somebody meant to
    /// write, and that is worth seeing rather than hiding.
    pub ghost: bool,
}

/// A project's files, as the file browser shows them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileList {
    pub space: SpaceId,
    pub root: String,
    /// Relative paths, sorted.
    pub files: Vec<String>,
    /// True when there were more than the daemon was willing to list.
    pub truncated: bool,
    /// Set by `FileRead` and `FileSave`: the file's text and which file it is.
    pub body: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VaultGraph {
    pub nodes: Vec<GraphNode>,
    /// Indices into `nodes`.
    pub edges: Vec<(u16, u16)>,
}

/// A note as a list shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteLine {
    /// Relative to the vault root, and the identity a `Note` query takes.
    pub path: String,
    pub title: String,
    pub tags: Vec<String>,
    /// Unix millis.
    pub mtime: u64,
    /// How many notes point at this one.
    pub backlinks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VaultReply {
    pub space: SpaceId,
    /// Absolute path of the vault root, so a client can show where it is reading from.
    pub root: String,
    pub notes: Vec<NoteLine>,
    /// Set for a `Note` query: the note's own text.
    pub body: Option<String>,
    /// Set for a `Note` query: what links here.
    pub backlinks: Vec<NoteLine>,
    /// Set for a `Graph` query.
    #[serde(default)]
    pub graph: Option<VaultGraph>,
}

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
    /// Answer to [`Cmd::VaultQuery`], for the note browser.
    Vault(Box<VaultReply>),
    /// Answer to [`Cmd::FileQuery`] and friends, for the file browser and editor.
    Files(Box<FileList>),
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

    /// Postcard identifies an enum variant by its index, so inserting one silently renumbers
    /// every variant below it: a client one build ahead sends `FocusPane`, the daemon decodes
    /// something else, reads a field that is not there, and drops the connection. That is not
    /// hypothetical — it shipped once, and clicking a pane killed the session while every
    /// keybinding above the inserted variant carried on working.
    ///
    /// Pinning the encoded index of a few mid-enum variants turns the next attempt into a
    /// failing test instead. **If this fails, you inserted a variant rather than appending
    /// one — move it to the end of the enum.**
    #[test]
    fn cmd_variants_are_append_only() {
        for (cmd, want) in [
            (Cmd::SplitRight, 0u8),
            (Cmd::NewTab, 7),
            (Cmd::JumpAttention, 18),
            (Cmd::RequestDigest, 30),
            (Cmd::Redraw, 31),
        ] {
            let bytes = postcard::to_allocvec(&cmd).unwrap();
            assert_eq!(bytes[0], want, "{cmd:?} moved: variants must only ever be appended");
        }
    }

    /// Adding a field to a snapshot struct is just as much a wire break as moving a variant,
    /// and the only defence is the handshake — so the version has to move with the shape.
    #[test]
    fn the_protocol_version_covers_the_current_wire_shape() {
        assert_eq!(PROTOCOL_VERSION, 14, "bump this whenever a wire struct or enum changes");
    }

    /// The assert above is a reminder, not a detector. It fires when you *do* bump the
    /// version, never when you *should have*: appending a variant or adding a field leaves
    /// every test in this file green, and the change ships silently. A newer client then
    /// sends a shape the running daemon cannot decode, and the handshake built to catch
    /// exactly that stays quiet, because the number it compares never moved.
    ///
    /// These functions close that hole, and they are the reason to keep them ugly. Every
    /// `match` is exhaustive with no `_` arm; every struct is destructured with no `..`. So
    /// **adding a variant or a field stops the build right here**, in the one place that
    /// says what to do about it: append (never insert), then bump `PROTOCOL_VERSION` and the
    /// assert above.
    ///
    /// They are never called, and must not be. Being forced to edit them is the whole test.
    #[test]
    fn a_new_variant_or_field_cannot_reach_the_wire_unnoticed() {
        #[allow(dead_code)]
        fn every_cmd(c: Cmd) {
            match c {
                Cmd::SplitRight
                | Cmd::SplitDown
                | Cmd::ClosePane
                | Cmd::FocusDir(..)
                | Cmd::Resize { .. }
                | Cmd::ToggleZoom
                | Cmd::SwapDir(..)
                | Cmd::NewTab
                | Cmd::NextTab
                | Cmd::PrevTab
                | Cmd::GotoTab(..)
                | Cmd::CloseTab
                | Cmd::NewSpace { .. }
                | Cmd::FocusSpace(..)
                | Cmd::NextSpace
                | Cmd::PrevSpace
                | Cmd::ToggleSidebar
                | Cmd::ToggleBus
                | Cmd::JumpAttention
                | Cmd::Scroll { .. }
                | Cmd::ScrollBottom { .. }
                | Cmd::FocusPane(..)
                | Cmd::RenamePane { .. }
                | Cmd::SpawnAgent { .. }
                | Cmd::ApplyLayout { .. }
                | Cmd::RenameSpace { .. }
                | Cmd::RenameTab { .. }
                | Cmd::CloseSpace(..)
                | Cmd::FocusTab(..)
                | Cmd::NewTabIn(..)
                | Cmd::RequestDigest
                | Cmd::Redraw
                | Cmd::SetSpaceAccent { .. }
                | Cmd::SetPaneRole { .. }
                | Cmd::ToggleSpaceCollapsed(..)
                | Cmd::TogglePanePinned(..)
                | Cmd::VaultQuery { .. }
                | Cmd::VaultSave { .. }
                | Cmd::VaultInit { .. }
                | Cmd::FileQuery { .. }
                | Cmd::FileSave { .. }
                | Cmd::FileRead { .. }
                | Cmd::OpenDocPane { .. }
                | Cmd::OpenProject { .. } => {}
            }
        }

        #[allow(dead_code)]
        fn every_frame(c: ClientFrame, s: ServerFrame, e: Event) {
            match c {
                ClientFrame::Input { .. }
                | ClientFrame::Resize { .. }
                | ClientFrame::Focus { .. }
                | ClientFrame::Command(..)
                | ClientFrame::Ping
                | ClientFrame::Detach => {}
            }
            match s {
                ServerFrame::Snapshot(..)
                | ServerFrame::Rows { .. }
                | ServerFrame::Event(..)
                | ServerFrame::Digest(..)
                | ServerFrame::Bye { .. }
                | ServerFrame::Vault(..)
                | ServerFrame::Files(..) => {}
            }
            match e {
                Event::AgentStateChanged { .. }
                | Event::BusMessage(..)
                | Event::PaneExited { .. }
                | Event::Notice { .. } => {}
            }
        }

        #[allow(dead_code)]
        fn every_field(snap: &Snapshot, pane: &PaneInfo, space: &SpaceInfo, tab: &TabInfo, view: &ViewState) {
            let Snapshot {
                protocol: _,
                daemon_version: _,
                spaces: _,
                tabs: _,
                panes: _,
                focused_space: _,
                focused_tab: _,
                focused_pane: _,
                view: _,
                sidebar: _,
                bus: _,
                status: _,
                tabbar: _,
                tasks_open: _,
                tasks_claimed: _,
                triggers_armed: _,
                recents: _,
            } = snap;
            let PaneInfo {
                id: _,
                tab: _,
                space: _,
                title: _,
                cwd: _,
                cell: _,
                content: _,
                cols: _,
                rows: _,
                agent: _,
                spawned_by: _,
                exited: _,
                scroll_offset: _,
                wants_mouse: _,
                bracketed_paste: _,
                role: _,
                pinned: _,
                board: _,
                repo: _,
            } = pane;
            let SpaceInfo {
                id: _,
                name: _,
                cwd: _,
                tabs: _,
                focused_tab: _,
                agent_count: _,
                attention_count: _,
                accent: _,
                collapsed: _,
                repo: _,
                notes: _,
            } = space;
            let TabInfo { id: _, space: _, name: _, panes: _, focused_pane: _ } = tab;
            let ViewState {
                sidebar_open: _,
                bus_open: _,
                sidebar_width: _,
                bus_width: _,
                zoom: _,
            } = view;
        }
    }
}
