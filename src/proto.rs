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
pub const PROTOCOL_VERSION: u32 = 1;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}
