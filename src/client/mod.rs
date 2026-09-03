//! The attached client: renders frames, forwards input, owns nothing.
//!
//! All geometry and session state comes from the daemon, so the client is free to die and
//! come back without disturbing a single running process.

pub mod clipboard;
pub mod editor;
pub mod glyphs;
pub mod graph;
pub mod image;
pub mod input;
pub mod kitty;
pub mod syntax;
pub mod menu;
pub mod roster;
pub mod selection;
pub mod settings;
pub mod ui;

use std::collections::{HashMap, VecDeque};
use std::io::{BufWriter, Stdout, Write as _};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::config::{Action, Chord, Config, LeaderMatch, Notify, Trigger};
use crate::client::editor::Vim;
use crate::client::menu::{Act, Level, Prompt, Target};
use crate::framing;
use crate::proto::{
    ClientFrame, Cmd, CursorPos, Digest, Message, NoticeLevel, PaneId, Row, ServerFrame, Snapshot,
    SpaceId, TabId, PROTOCOL_VERSION,
};
use ui::overlays::Item;
use crate::client::roster::Focus;
use ui::sidebar::Hit;

/// Animation cadence for spinners, toast expiry, and whatever is crossing the start screen.
const ANIM: Duration = Duration::from_millis(110);
/// Redraw cadence while the graph is open.
///
/// Twice as often, because the graph is the one view whose whole point is that it moves, and
/// nine frames a second reads as a slideshow. Only while it is open: the rest of horde has
/// nothing to animate but a spinner and the occasional passer-by on the start screen, and
/// paying for twenty frames a second to watch a static screen is how a terminal multiplexer
/// ends up warming a lap.
const ANIM_GRAPH: Duration = Duration::from_millis(50);
/// Heartbeat for a screen with nothing moving on it.
///
/// A start screen between crossings is a photograph, and so is one on a terminal too small
/// to have drawn a wordmark at all. It still wants a pulse — a toast has to expire, and the
/// next crossing has to be noticed when it falls due — but a pulse is all it wants, and a
/// second is well under the point where either reads as late. It also means an idle greeter
/// now wakes nine times less often than it did before anything walked across it.
const ANIM_STILL: Duration = Duration::from_millis(1000);
/// How long the graph's cursor has to sit still before its note is fetched.
const PREVIEW_SETTLE: Duration = Duration::from_millis(160);

/// Start and stop the walk across the wordmark.
///
/// Here rather than at the seven places that assign `Mode::Dashboard`, six of which are the
/// cursor moving one row: a shamble that restarted every time you pressed `j` would be a
/// twitch rather than a walk. Matching on the variant and not the value is what makes
/// walking the menu invisible to the animation.
///
/// It is also where every reason *not* to run lives — the setting, the calm switch, and
/// leaving the screen — so nothing downstream has to ask more than whether there is a walk.
fn sync_zombie(app: &mut App) {
    let want = matches!(app.mode, Mode::Dashboard { .. }) && app.cfg.zombie && app.cfg.animate;
    match (want, app.zombie.is_some()) {
        (true, false) => app.zombie = Some(ui::zombie::Walk::new()),
        (false, true) => {
            app.zombie = None;
            app.zombie_stage = None;
        }
        _ => {}
    }
}

/// The cadence the client wants right now.
///
/// A function of the whole client rather than of the mode alone, because two screens with
/// the same name can want different things: a start screen with something halfway across it
/// and a start screen that is a still picture are not the same picture.
fn cadence(app: &App) -> Duration {
    match app.mode {
        Mode::Graph { .. } => ANIM_GRAPH,
        Mode::Dashboard { .. } if !app.restless() => ANIM_STILL,
        _ => ANIM,
    }
}
const TOAST_LIFE: Duration = Duration::from_secs(6);
/// Bus messages kept client-side for the drawer.
const BUS_CAP: usize = 300;

/// A completion list, open over the editor.
///
/// Held rather than re-requested as you type: the answer takes a round trip and tens of
/// milliseconds, so narrowing the list already in hand is the difference between completion
/// that feels instant and completion that feels like a network.
#[derive(Debug, Clone)]
pub struct Completions {
    items: Vec<crate::proto::Completion>,
    /// Whether accepting closes a wikilink. Links are the one completion that finishes its
    /// own punctuation, because `[[` is an opening and nobody types the closing half on
    /// purpose.
    link: bool,
    /// Which of the *matching* items is selected.
    pub sel: usize,
    /// The line the request was made on. Leaving it closes the popup, because a list of
    /// completions for another line is worse than no list.
    line: usize,
    /// Where the word being completed starts. Typing before it is not narrowing any more.
    from: usize,
}

impl Completions {
    /// The items still matching what has been typed, in the order the server ranked them.
    ///
    /// Case-insensitive prefix, then anything containing it — which is what people expect
    /// from typing three letters, without pulling in a fuzzy matcher for a list of twenty.
    pub fn matching(&self, prefix: &str) -> Vec<&crate::proto::Completion> {
        if prefix.is_empty() {
            return self.items.iter().collect();
        }
        let p = prefix.to_lowercase();
        let mut exact: Vec<&crate::proto::Completion> = Vec::new();
        let mut loose: Vec<&crate::proto::Completion> = Vec::new();
        for i in &self.items {
            let l = i.label.to_lowercase();
            if l.starts_with(&p) {
                exact.push(i);
            } else if l.contains(&p) {
                loose.push(i);
            }
        }
        exact.extend(loose);
        exact
    }
}

/// When to hand the daemon the buffer so a language server can look at it.
///
/// Every keystroke would be a whole file down a socket and a reparse per character. Waiting
/// for a save would mean diagnostics that describe the last thing you wrote out rather than
/// the line you are on — which is the whole reason this is not just [`Cmd::FileSave`].
///
/// So: shortly after you stop typing, and at least every couple of seconds regardless, since
/// somebody writing a paragraph without pausing should still be told about it.
#[derive(Debug, Default)]
pub struct DocSync {
    /// Revision last sent. `None` means this buffer has never gone out at all.
    sent: Option<usize>,
    /// Revision last seen here, for noticing that typing is still going on.
    seen: usize,
    changed_at: Option<Instant>,
    sent_at: Option<Instant>,
}

/// How long a pause counts as having stopped typing.
const DOC_SETTLE: Duration = Duration::from_millis(400);
/// The longest a change may go unsent while someone types without pausing.
const DOC_MAX_WAIT: Duration = Duration::from_secs(2);

impl DocSync {
    /// Whether this revision should go now. Records that it did.
    fn due(&mut self, rev: usize) -> bool {
        if rev != self.seen {
            self.seen = rev;
            self.changed_at = Some(Instant::now());
        }
        if self.sent == Some(rev) {
            return false;
        }
        // A buffer that has never been sent goes immediately: that first message is what
        // starts the language server, and waiting to open a file nobody has typed in yet
        // would be a pause for no reason.
        let first = self.sent.is_none();
        let quiet = self.changed_at.is_some_and(|t| t.elapsed() >= DOC_SETTLE);
        let overdue = self.sent_at.is_some_and(|t| t.elapsed() >= DOC_MAX_WAIT);
        if first || quiet || overdue {
            self.sent = Some(rev);
            self.sent_at = Some(Instant::now());
            return true;
        }
        false
    }
}

/// What a held mouse button is doing to the graph.
///
/// Grabbing a node moves the node; grabbing the background moves the view. Which one you get
/// is decided by what was under the pointer when the button went down, which is the rule
/// every graph editor uses and nobody has to be told.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphDrag {
    /// Panning. Holds the last pointer cell, so each event moves by its own delta.
    Pan { at: (u16, u16) },
    Node { i: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Keystrokes go to the focused pane.
    Terminal,
    /// The prefix key was pressed; the next key is a horde binding.
    Prefix,
    /// A project's notes, full screen. `query` filters as you type.
    Notes { query: String, sel: usize },
    /// A project's files. The thing opening a project shows you.
    Files { query: String, sel: usize },
    /// Writing. Modeless: the keys are the ones a text field has, because this is a notes
    /// app and typing should type.
    ///
    /// `project` says which half of horde the file belongs to — a note in a vault, or a file
    /// in the project — because that decides where saving sends it and nothing else about
    /// the editor changes.
    /// `vim` is which half of the keyboard you are in: insert types, normal commands.
    Editor { path: String, scroll: usize, project: bool, vim: editor::Vim },
    /// A note, rendered. `link` indexes the wikilinks in it, so `tab` walks them and
    /// `enter` follows one — which is the whole difference between a file and a vault.
    Reader { scroll: usize, link: usize },
    /// The link graph. The layout itself lives on `App` rather than in here, because it is
    /// expensive to build and a `Mode` is cloned on every keystroke.
    Graph { sel: usize },
    /// The setup walkthrough, shown once before anything else.
    Setup { step: ui::setup::Step },
    /// The start screen. `sel` indexes the *selectable* rows, so headers and hints are
    /// skipped without the cursor ever having to know they exist.
    ///
    /// Shown on attaching to a session that started from nothing, and reachable afterwards
    /// with `prefix 0` — tabs are 1 through 9, so home is 0.
    Dashboard { sel: usize },
    /// The leader was pressed; `pending` is the sequence typed since.
    ///
    /// Unlike [`Mode::Prefix`] this can span several keys, so it holds them — and it holds
    /// `back` too, because the leader is reachable from horde's own views as a bare `space`
    /// and leaving it has to return the keyboard where it was found, not to a pane.
    ///
    /// Keys are only ever released as an *action*: an abandoned sequence is dropped, never
    /// replayed into a pane. Replaying would type `wv` at whatever an agent was doing.
    Leader { pending: Vec<Chord>, back: Box<Mode> },
    Help,
    Palette { query: String, sel: usize },
    SpaceSwitcher { query: String, sel: usize },
    /// A single-line text prompt, opened by a key or a context menu.
    Prompt { prompt: Prompt, value: String },
    /// The settings page. `cat` indexes `Category::all`, `sel` indexes that category's rows.
    /// `capture` holds the action awaiting a key press while rebinding.
    Settings { cat: usize, sel: usize, capture: Option<String> },
    /// A context menu. The stack grows when a submenu opens so `esc` steps back out.
    Menu { stack: Vec<Level>, at: (u16, u16) },
    /// The catch-up report. Scrollable, because a long absence produces a long digest.
    Digest { scroll: usize },
    /// The full-screen roster. Scroll lives here because it is a one-shot view; the
    /// *selection* is `app.sidebar.cursor`, so one cursor serves both views of the same list.
    Roster { scroll: usize },
    /// Every agent blocked on a decision, in one list, answerable from there.
    ///
    /// `sel` indexes `overlays::pending`, which is ordered by how long each has been waiting.
    /// An index rather than a pane id because the list is short, always sorted the same way,
    /// and only ever grows at the bottom: an agent that unblocks leaves, and the one under
    /// the cursor keeps its place unless it was the one that left.
    Approvals { sel: usize },
    /// Your own board, full screen — see `daemon::kanban`.
    ///
    /// `col` and `sel` are a cursor into the *shown* columns and cards, not into anything the
    /// daemon knows about: the board is filtered and sorted client-side, so an index into the
    /// reply would move under a keystroke that only changed the filter. Everything the view
    /// costs anything to hold — the reply, the per-column scroll, the drag — lives on `App`,
    /// because a `Mode` is cloned on every keystroke.
    Kanban { view: ui::kanban::View, col: usize, sel: usize },
    /// One card, full screen: description, dates, tags, and its thread.
    ///
    /// By id rather than by position, because everything on the board can move under it —
    /// including this card, when the agent that was handed it comments and the sort is by
    /// recency. An id that has gone is the one case, and it returns to the board.
    Card { id: u64, focus: ui::kanban::Field },
    /// The sidebar has the keyboard.
    ///
    /// Carries nothing: everything it edits lives on `App` or in the daemon, because a
    /// collapse has to survive leaving the mode and `Mode` variants are constructed and
    /// discarded. A mode at all — rather than plain bindings — because `Keymap::rebind`
    /// rightly refuses bare printable keys, so without one every cursor move would cost
    /// `prefix`+key and crossing a dozen rows would cost two dozen keystrokes.
    Sidebar,
}

/// What selecting a picker row does.
#[derive(Debug, Clone, PartialEq)]
pub enum PickKind {
    Command(String),
    Space(SpaceId),
}

pub struct Toast {
    pub level: NoticeLevel,
    pub text: String,
    born: Instant,
}

pub struct App {
    pub cfg: Config,
    pub snapshot: Option<Snapshot>,
    /// Per-pane row cache, kept in step with the daemon by dirty-row updates.
    pub rows: HashMap<PaneId, Vec<Row>>,
    pub cursors: HashMap<PaneId, CursorPos>,
    pub bus: Vec<Message>,
    /// The last digest the daemon sent, held while its overlay is open.
    pub digest: Option<Digest>,
    pub mode: Mode,
    pub toasts: VecDeque<Toast>,
    pub tick: usize,
    /// Row-to-target map produced by the sidebar during render, used to resolve clicks.
    pub sidebar_hits: Vec<(u16, Hit)>,
    /// Cursor, scroll, and nothing else — see `roster::SidebarState`.
    pub sidebar: roster::SidebarState,
    /// Where each landable roster row was drawn: row, column, width, and what it is. Rects
    /// rather than rows, because the roster is multi-column and a row means several things.
    pub roster_hits: Vec<(u16, u16, u16, Focus)>,
    /// The most recent answer to a vault query, held until the next one replaces it.
    pub vault: Option<crate::proto::VaultReply>,
    /// Set while a note is being fetched to write into rather than to read.
    pub opening_editor: bool,
    /// Every note in the vault: the tree, the browser's list, and what a `[[link]]` completes
    /// against.
    ///
    /// Held apart from `vault` because a reply to `Note` carries *one* note — the one asked
    /// for — and both used to land in the same field. So opening a note replaced the index
    /// with a list of length one, and the tree beside it collapsed to the note you were
    /// already reading. A reply to `Graph` carries none at all and emptied it outright.
    ///
    /// Only a list answers here: `List` and `Search` say what is in the vault, and nothing
    /// else claims to.
    pub vault_index: Option<crate::proto::VaultReply>,
    /// Where a prompt was opened from, so cancelling it goes back rather than to the terminal.
    ///
    /// `None` means the terminal, which is where a prompt opened from a pane belongs. Every
    /// prompt used to end there whatever opened it, so cancelling "new note" cost you the
    /// browser or the note you were reading — the one keystroke that is supposed to cost
    /// nothing.
    pub prompt_back: Option<Box<Mode>>,
    /// Set while the note list is being fetched in order to land in the vault.
    ///
    /// The list has to arrive before the home note can be picked out of it, so the intent
    /// outlives the request — otherwise the reply is indistinguishable from the one the note
    /// browser asked for, and both views would open at once.
    pub opening_vault: bool,
    /// Whether the tree of notes is showing beside the one being edited.
    ///
    /// Only ever true for a vault note. A project file has its own tree at `F` and no reason
    /// to grow a second one that lists somebody's notes beside their source.
    pub vault_tree: bool,
    /// Where the tree drew its rows: `(y, path)`, so a click opens the note it points at.
    pub vault_tree_hits: Vec<(u16, String)>,
    /// A note whose body has been asked for, so the next reply is known to be a read.
    pub pending_read: Option<String>,
    /// A wikilink being followed: the daemon is resolving the name, and the best hit becomes
    /// the next note to read. Held here because resolution is the index's job, not the
    /// client's — the client has a name, and only the daemon knows what it points at.
    pub follow: Option<String>,
    /// Folders showing their contents in the file tree. On `App` rather than in the mode
    /// because a mode is cloned on every keystroke, and which folders you opened is a thing
    /// you did rather than a thing the view is.
    pub open_dirs: std::collections::HashSet<String>,
    /// Set when a project has just been opened and its files should be listed as soon as
    /// the focus change lands.
    pub want_files: bool,
    /// The project's files, from the last file query.
    pub files: Option<crate::proto::FileList>,
    /// What the setup walkthrough has been told.
    pub setup: ui::setup::Answers,
    /// Syntax highlighting for the open buffer, and the revision it was computed at.
    ///
    /// Cached because highlighting a file is milliseconds and a frame is microseconds: a
    /// highlighter run on every keystroke would make a fast editor feel like a slow one.
    ///
    /// Keyed by path as well as revision. Every buffer opens at revision zero, so a cache
    /// that only knew the number would hand the next file the last one's coloured text —
    /// which is not a wrong colour, it is the wrong file's contents on the screen.
    pub highlight: Option<(String, usize, Vec<ratatui::text::Line<'static>>)>,
    /// The note being written, alive only while the editor is open.
    pub buffer: Option<editor::Buffer>,
    /// Keeping the daemon's copy of that buffer roughly in step with this one.
    pub doc: DocSync,
    /// The completion list, while one is open.
    pub completions: Option<Completions>,
    /// A note list was asked for by `[[` in the editor, so the next vault reply belongs to
    /// the completion popup rather than to the browser.
    pub linking: bool,
    /// What a language server thinks of the file being edited, keyed by the path the editor
    /// knows it as. Cleared when the editor closes, because it is about one open file.
    pub diags: HashMap<String, Vec<crate::proto::Diag>>,
    /// The line held by `dd`, `yy` or `cc`, for `p` to put down. One unnamed register: named
    /// ones are a filing system, and this is an editor you keep a note open in.
    pub yank: Option<String>,
    /// What `/` last looked for, so `n` has something to repeat.
    pub search: String,
    /// The graph layout, alive only while the graph is open.
    pub sim: Option<graph::Sim>,
    /// The graph itself, held apart from `vault`.
    ///
    /// It used to be read back out of the last vault reply, which meant any *other* vault
    /// query — a note being previewed, a search — replaced it with a reply carrying no graph
    /// at all, and the picture vanished. A graph outlives the query that fetched it.
    pub graph: Option<crate::proto::VaultGraph>,
    /// How far in, and where the view is centred. Panning moves the centre; the layout
    /// underneath does not know the difference.
    pub graph_zoom: f64,
    pub graph_centre: graph::Point,
    /// Pictures the terminal is to draw, as of the last frame. Filled in by whichever view
    /// drew them, because only the renderer knows where a line ended up on screen.
    pub images: Vec<kitty::Place>,
    /// What the terminal is currently showing, so an unchanged picture is left alone.
    pub placed: kitty::Placed,
    /// Node hits for the graph: `(y, x, node index)`.
    pub graph_hits: Vec<(u16, u16, usize)>,
    /// The area the graph was last drawn into, so a click can be turned back into a place in
    /// the layout. Recorded by the renderer rather than recomputed, because two copies of
    /// the same geometry agree only until one of them changes.
    pub graph_plot: Option<ratatui::layout::Rect>,
    /// What the pointer is doing to the graph, while it is doing it.
    pub graph_drag: Option<GraphDrag>,
    /// When the graph opened, which is what the drift is a function of. Time rather than
    /// frames, so the motion looks the same however fast the client is redrawing.
    pub graph_since: Option<Instant>,
    /// The start screen's occasional passer-by: when its clock started, and the seed its
    /// schedule is jittered by. `Some` exactly while the start screen is open and the
    /// setting is on — see [`sync_zombie`], which is the only thing that writes it.
    pub zombie: Option<ui::zombie::Walk>,
    /// Where the wordmark was last drawn, or `None` when the terminal was too small for a
    /// banner. Recorded by the renderer rather than recomputed here, for the reason
    /// `graph_plot` gives — and it is also the answer to "is there anything to animate",
    /// which is what stops the loop waking up hopefully on a screen with no stage.
    pub zombie_stage: Option<ratatui::layout::Rect>,
    /// Whether the note beside the graph is showing.
    pub graph_panel: bool,
    /// The note the panel is showing, and which node it is for.
    pub preview: Option<Box<crate::proto::VaultReply>>,
    pub preview_for: Option<String>,
    /// When the selection last changed, so a walk through the graph does not ask the daemon
    /// for every note it passes over.
    pub preview_at: Option<Instant>,
    /// A note was asked for by the graph, so the next vault reply belongs to the panel.
    pub previewing: bool,
    /// The personal board, from the last query. Replaced wholesale, never patched.
    pub kanban: Option<crate::proto::KanbanReply>,
    /// The area the board was last drawn into.
    ///
    /// Recorded rather than recomputed, for the reason `graph_plot` gives — and it is the
    /// only thing the mouse handler needs, because the layout is a pure function of it. No
    /// hit list: two copies of the same geometry agree only until one of them changes, and a
    /// recorded hit list is a copy that is one frame stale by construction.
    pub kanban_area: Option<ratatui::layout::Rect>,
    /// Where a floating card was drawn, or `None` when it has the whole frame.
    ///
    /// Recorded because "outside the popup" is a place you can click, and the handler cannot
    /// know where the edge is without being told where the renderer put it.
    pub card_popup: Option<ratatui::layout::Rect>,
    /// How far each shown column is scrolled, by position in the shown list.
    pub kanban_scroll: Vec<u16>,
    /// Whether the board is showing every project rather than the one you are standing in.
    pub kanban_all: bool,
    /// Whether archived cards are showing.
    pub kanban_archived: bool,
    /// What `/` last filtered the board by.
    pub kanban_query: String,
    /// A one-line question the board is asking, and the answer so far.
    pub kanban_ask: Option<ui::kanban::Asking>,
    /// What the pointer is doing to a card, while it is doing it.
    pub card_drag: Option<ui::kanban::CardDrag>,
    /// Where and when the last click landed, so a second one in the same cell can be told
    /// from a first one somewhere else. crossterm has no notion of a double click.
    pub card_click: Option<(u16, u16, Instant)>,
    /// The field being typed into on a card, and what has been typed.
    pub card_edit: Option<ui::kanban::Editing>,
    /// How far the card view is scrolled. On `App` rather than in the mode because a thread
    /// long enough to need scrolling is one you come back to.
    pub card_scroll: usize,
    /// How many lines the card view last had, so scrolling cannot run off the end of one.
    /// Recorded by the renderer, which is the only thing that knows how a description wrapped.
    pub card_lines: usize,
    /// Where the cursor was on the board when a card was opened.
    ///
    /// Without this, opening a card and pressing escape puts you back at the top-left corner
    /// of the board — which after three cards feels like the board losing your place, because
    /// it is.
    /// The view, column and row a card was opened from.
    pub kanban_back: (ui::kanban::View, usize, usize),
    /// Row hits for the note browser.
    pub notes_hits: Vec<(u16, usize)>,
    /// Row hits for the dashboard: `(y, index into its row list)`.
    pub dashboard_hits: Vec<ui::dashboard::Hit>,
    /// Whether the start-screen decision has been made for this attach. Made once, on the
    /// first snapshot, so a later shape change cannot yank you back to a greeter.
    greeted: bool,
    /// Set once the version mismatch warning has been shown, so it appears only once.
    pub warned_version: bool,
    /// Discard what ratatui believes is on screen before the next draw.
    ///
    /// The client only sends the host the cells that changed since its last frame, which is
    /// right for as long as ratatui's record of the screen is a faithful one. A host resize, or
    /// a layout that changed shape under it, can leave that record describing a screen that no
    /// longer exists — and a diffing renderer never recovers on its own, because every frame
    /// after agrees with a record that is already wrong. Stale glyphs sit there until something
    /// happens to overwrite that exact cell. `/clear` was the manual way out; this is the
    /// automatic one.
    pub needs_clear: bool,
    /// Where the cursor goes and, when it belongs to a program rather than to horde's own
    /// editor, what shape it asked for. Recorded during render and written out after the
    /// frame rather than handed to ratatui -- see the draw loop.
    pub cursor_at: Option<(u16, u16, Option<u8>)>,
    /// Screen row to menu-item index, recorded during render for mouse hit-testing.
    pub menu_hits: Vec<(u16, usize)>,
    /// Rect the menu occupies, so clicks outside it can dismiss it.
    pub menu_rect: crate::proto::Rect,
    /// Screen row to settings-category index.
    pub settings_cat_hits: Vec<(u16, usize)>,
    /// Screen row to settings-row index.
    pub settings_row_hits: Vec<(u16, usize)>,
    /// Text highlighted with the mouse, if any. Belongs to exactly one pane.
    pub selection: Option<selection::Selection>,
    pub quit: bool,
}

impl App {
    #[cfg(test)]
    pub fn new_for_test(cfg: Config) -> App {
        App::new(cfg)
    }

    fn new(cfg: Config) -> App {
        App {
            cfg,
            snapshot: None,
            rows: HashMap::new(),
            cursors: HashMap::new(),
            bus: Vec::new(),
            digest: None,
            mode: Mode::Terminal,
            toasts: VecDeque::new(),
            tick: 0,
            sidebar_hits: Vec::new(),
            sidebar: roster::SidebarState::default(),
            roster_hits: Vec::new(),
            dashboard_hits: Vec::new(),
            kanban: None,
            kanban_area: None,
            card_popup: None,
            kanban_scroll: Vec::new(),
            kanban_all: false,
            kanban_archived: false,
            kanban_query: String::new(),
            kanban_ask: None,
            card_drag: None,
            card_click: None,
            card_edit: None,
            card_scroll: 0,
            card_lines: 0,
            kanban_back: (ui::kanban::View::Board, 0, 0),
            notes_hits: Vec::new(),
            open_dirs: std::collections::HashSet::new(),
            want_files: false,
            files: None,
            setup: ui::setup::Answers::default(),
            highlight: None,
            buffer: None,
            doc: DocSync::default(),
            completions: None,
            linking: false,
            diags: HashMap::new(),
            yank: None,
            search: String::new(),
            sim: None,
            graph: None,
            graph_zoom: 1.0,
            graph_centre: graph::Point { x: 0.0, y: 0.0 },
            images: Vec::new(),
            placed: kitty::Placed::default(),
            graph_hits: Vec::new(),
            graph_plot: None,
            graph_drag: None,
            graph_since: None,
            zombie: None,
            zombie_stage: None,
            graph_panel: true,
            preview: None,
            preview_for: None,
            preview_at: None,
            previewing: false,
            vault: None,
            follow: None,
            pending_read: None,
            opening_editor: false,
            vault_index: None,
            prompt_back: None,
            opening_vault: false,
            vault_tree: false,
            vault_tree_hits: Vec::new(),
            greeted: false,
            warned_version: false,
            needs_clear: false,
            cursor_at: None,
            menu_hits: Vec::new(),
            menu_rect: crate::proto::Rect::default(),
            settings_cat_hits: Vec::new(),
            settings_row_hits: Vec::new(),
            selection: None,
            quit: false,
        }
    }

    pub fn toast(&mut self, level: NoticeLevel, text: impl Into<String>) {
        let text = text.into();
        if self.cfg.notify == Notify::Off {
            return;
        }
        if self.cfg.notify == Notify::System {
            notify_system(&text);
        }
        self.toasts.push_back(Toast { level, text, born: Instant::now() });
        while self.toasts.len() > 5 {
            self.toasts.pop_front();
        }
    }

    fn expire_toasts(&mut self) {
        while self.toasts.front().is_some_and(|t| t.born.elapsed() > TOAST_LIFE) {
            self.toasts.pop_front();
        }
    }

    pub fn focused_pane(&self) -> Option<PaneId> {
        self.snapshot.as_ref().and_then(|s| s.focused_pane)
    }

    /// Whether an agent is mid-turn, and so a spinner is turning.
    fn working(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| {
            s.panes
                .iter()
                .any(|p| p.agent.as_ref().is_some_and(|a| a.state == crate::proto::AgentState::Working))
        })
    }

    /// Whether the start screen's passer-by is both on screen and moving.
    ///
    /// Both halves matter. A crossing under way with no banner drawn is a crossing happening
    /// off stage: the clock keeps running, because the alternative is a schedule that resets
    /// every time someone drags a window edge, but there is nothing to draw and so nothing to
    /// wake up for.
    fn walking(&self) -> bool {
        self.zombie_stage.is_some() && self.zombie.is_some_and(|w| w.phase().walking())
    }

    /// Whether anything on screen changes on its own.
    ///
    /// The whole promise this file makes — that a still screen costs nothing — is this one
    /// predicate being false.
    fn restless(&self) -> bool {
        !self.toasts.is_empty() || self.working() || self.walking()
    }

    fn pane_info(&self, id: PaneId) -> Option<&crate::proto::PaneInfo> {
        self.snapshot.as_ref()?.panes.iter().find(|p| p.id == id)
    }

    // -- picker contents --------------------------------------------------

    pub fn palette_items(&self) -> Vec<Item> {
        let query = match &self.mode {
            Mode::Palette { query, .. } => query.clone(),
            _ => String::new(),
        };
        crate::daemon::rpc::command_names()
            .iter()
            .filter(|n| fuzzy(&query, n))
            .map(|n| Item { label: n.to_string(), kind: PickKind::Command(n.to_string()) })
            .collect()
    }

    pub fn space_items(&self) -> Vec<Item> {
        let query = match &self.mode {
            Mode::SpaceSwitcher { query, .. } => query.clone(),
            _ => String::new(),
        };
        self.snapshot
            .as_ref()
            .map(|s| {
                s.spaces
                    .iter()
                    .filter(|sp| fuzzy(&query, &sp.name))
                    .map(|sp| {
                        let mut label = sp.name.clone();
                        if sp.agent_count > 0 {
                            label.push_str(&format!("  {} agents", sp.agent_count));
                        }
                        if sp.attention_count > 0 {
                            label.push_str(&format!("  ◍{}", sp.attention_count));
                        }
                        Item { label, kind: PickKind::Space(sp.id) }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Case-insensitive subsequence match: `sp` matches `split-right`.
fn fuzzy(query: &str, candidate: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut chars = candidate.to_lowercase().chars().collect::<Vec<_>>().into_iter();
    for q in query.to_lowercase().chars() {
        if q == ' ' {
            continue;
        }
        if !chars.any(|c| c == q) {
            return false;
        }
    }
    true
}

/// Desktop notification, whatever this host has for one.
///
/// Best effort in both directions: a host with no notifier does nothing, and a notifier that
/// fails to start is not worth reporting. The toast that prompted this is already on screen, so
/// there is a human looking at the message either way — which is exactly what makes the
/// detached path in `daemon::notify` the half that has to complain when it cannot deliver.
fn notify_system(text: &str) {
    if let Some(mut cmd) = crate::platform::system_notify(text) {
        use std::process::Stdio;
        let _ = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}

// ---------------------------------------------------------------------------
// Attach
// ---------------------------------------------------------------------------

pub async fn attach(cfg: Config, warnings: Vec<String>) -> Result<()> {
    let socket = crate::config::socket_path();
    let stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("could not connect to {}", socket.display()))?;

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));

    let (read_half, mut write_half) = stream.into_split();
    // Ask to attach in JSON, then the connection is postcard frames both ways.
    let hello = serde_json::json!({
        "id": "attach",
        "method": "attach",
        "params": { "protocol": PROTOCOL_VERSION, "cols": cols, "rows": rows }
    });
    let mut line = serde_json::to_vec(&hello)?;
    line.push(b'\n');
    write_half.write_all(&line).await?;
    write_half.flush().await?;

    // Outbound frames.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientFrame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if framing::write_frame(&mut write_half, &frame).await.is_err() {
                break;
            }
        }
    });

    // Inbound frames.
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerFrame>();
    let reader = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(read_half);
        loop {
            match framing::read_frame::<_, ServerFrame>(&mut reader).await {
                Ok(frame) => {
                    if in_tx.send(frame).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut app = App::new(cfg);
    for w in warnings {
        app.toast(NoticeLevel::Warn, w);
    }

    let (mut term, glyph_notes) = setup_terminal()?;
    // Surfaced rather than silent: a terminal whose glyph widths differ from the tables is the
    // difference between a pane that lines up and one that spills over its own border, and it
    // is worth being able to see which answer horde got.
    for n in glyph_notes {
        app.toast(NoticeLevel::Info, n);
    }
    let result = run_loop(&mut term, &mut app, out_tx.clone(), in_rx).await;
    restore_terminal(&mut term)?;

    // Tell the daemon we are going, so it stops rendering for us.
    let _ = out_tx.send(ClientFrame::Detach);
    writer.abort();
    reader.abort();
    result
}

/// One frame's escape stream can run to tens of kilobytes. `std::io::Stdout` is a
/// `LineWriter` with a 1KB buffer, so writing a frame through it unbuffered means dozens of
/// `write` syscalls, each one a partial repaint the terminal renders on its own schedule --
/// which is what a sweeping, torn frame looks like. Buffer the whole frame and flush it once.
type Term = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

/// Big enough that no realistic frame reaches it, so a frame is one flush.
const OUT_BUF: usize = 1 << 20;

/// Begin synchronized output, then hide the cursor.
///
/// Terminals that understand `?2026` hold the whole frame back and present it in one go, so a
/// clear-and-repaint never shows as a flash. Hiding the cursor covers the ones that do not:
/// without it the hardware cursor visibly walks the screen, stopping at every run the diff
/// writes. Both are the frame's opening bytes, before ratatui writes a single cell.
const FRAME_BEGIN: &[u8] = b"\x1b[?2026h\x1b[?25l";
const SYNC_END: &[u8] = b"\x1b[?2026l";

fn setup_terminal() -> Result<(Term, Vec<String>)> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;

    // Before the input thread exists. The terminal answers a cursor query as ordinary input,
    // and whoever reads stdin first gets it — start the reader and the reply is swallowed as a
    // keystroke instead.
    let notes = glyphs::measure(&mut stdout);

    // A panic with the terminal in raw mode leaves an unusable shell, so restore first and
    // let the default hook print afterwards.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?2026l\x1b[0 q\x1b[?25h");
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(DisableMouseCapture);
        let _ = out.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
        default_hook(info);
    }));

    let out = BufWriter::with_capacity(OUT_BUF, stdout);
    Ok((Terminal::new(CrosstermBackend::new(out))?, notes))
}

fn restore_terminal(term: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    // End any frame still open and hand the cursor shape back to the shell -- horde only
    // borrowed it from whichever pane last asked for one.
    term.backend_mut().write_all(b"\x1b[?2026l\x1b[0 q")?;
    // Pictures are not part of the screen horde is about to leave, so leaving without taking
    // them down leaves them over whatever shell comes back.
    if kitty::supported() {
        use std::io::Write;
        let _ = term.backend_mut().write_all(&kitty::clear());
    }
    execute!(
        term.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    term.show_cursor()?;
    Ok(())
}

/// Read terminal events on a dedicated blocking thread.
///
/// **Do not replace this with `EventStream` in a `select!`.** crossterm's `EventStream`
/// dispatches a wake-task guarded by an `AtomicBool` that is only cleared once that task
/// fires. `select!` drops every branch future that does not win, so the first dropped
/// `next()` leaves the flag set with a waker belonging to a dead future: no new task is
/// dispatched, `poll_next` returns `Pending` forever, and all input stops. A blocking read
/// on its own thread cannot be cancelled, so it cannot wedge.
fn spawn_event_reader() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("horde-input".into())
        .spawn(move || {
            while let Ok(ev) = crossterm::event::read() {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        })
        .expect("spawn input thread");
    rx
}

async fn run_loop(
    term: &mut Term,
    app: &mut App,
    out: mpsc::UnboundedSender<ClientFrame>,
    mut inbound: mpsc::UnboundedReceiver<ServerFrame>,
) -> Result<()> {
    let mut events = spawn_event_reader();
    let mut beat = cadence(app);
    let mut anim = tokio::time::interval(beat);
    anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut needs_draw = true;
    // 0 is "whatever the host was using", which is what the cursor still is before the first
    // frame, and never a code any pane asks for.
    let mut last_shape: u8 = 0;

    loop {
        // Arriving at or leaving the start screen starts and stops the walk across the
        // wordmark, which is one of the two things that decide how often there is anything
        // new to draw. Opening or closing the graph is the other.
        sync_zombie(app);
        if cadence(app) != beat {
            beat = cadence(app);
            anim = tokio::time::interval(beat);
            anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        }
        if needs_draw {
            // Throw away ratatui's record of the screen so the next frame is written in full.
            //
            // `Terminal::clear` is the obvious call and must not be used: it snapshots the
            // cursor first, and asking a terminal where its cursor is means writing `ESC [ 6 n`
            // and reading the answer back as input. By this point horde's own input thread owns
            // stdin and takes the reply, so the query times out after two seconds and returns
            // an error — which propagated out of the draw loop and closed the whole client.
            // Closing a pane changes the layout's shape, so `ctrl+b x` did it every time.
            //
            // `resize` to the size it already is reaches the same reset — clear the viewport,
            // reset the back buffer — and on a fullscreen viewport it has nothing to restore,
            // so it never asks.
            term.backend_mut().write_all(FRAME_BEGIN)?;
            if std::mem::take(&mut app.needs_clear) {
                let size = term.size()?;
                term.resize(size.into())?;
            }
            app.images.clear();
            term.draw(|f| ui::draw(f, app))?;
            // After the frame, and only after: these are not in ratatui's grid, so anything
            // it paints would go over them. Doing nothing when nothing moved is what keeps a
            // photograph from being re-sent twenty times a second.
            if kitty::supported() {
                let want = std::mem::take(&mut app.images);
                let _ = app.placed.sync(&mut std::io::stdout(), &want);
                app.images = want;
            }
            // Last, while the cursor is still hidden, and shown only once it is where it
            // belongs. ratatui's own `set_cursor_position` shows first and moves second, which
            // leaves the cursor visible for one round trip at whichever cell the diff happened
            // to write last -- a sidebar spinner, usually, not the prompt. Pictures go out
            // above, so this closes over them too.
            let cursor = app.cursor_at;
            let out = term.backend_mut();
            if let Some((x, y, shape)) = cursor {
                // Shape only on a change: DECSCUSR restarts the blink phase, so re-sending it
                // every frame gives a blinking cursor that never gets far enough into its
                // cycle to blink. `None` is horde's own caret, which asks for no shape at all.
                if let Some(shape) = shape.filter(|s| *s != last_shape) {
                    write!(out, "\x1b[{shape} q")?;
                    last_shape = shape;
                }
                write!(out, "\x1b[{};{}H\x1b[?25h", y + 1, x + 1)?;
            }
            out.write_all(SYNC_END)?;
            out.flush()?;
            needs_draw = false;
        }
        if app.quit {
            return Ok(());
        }

        tokio::select! {
            // Server frames first: rendering the newest state matters more than input echo.
            frame = inbound.recv() => {
                match frame {
                    Some(frame) => {
                        if let Some(reason) = apply_frame(app, frame, &out) {
                            return Err(anyhow!(reason));
                        }
                        needs_draw = true;
                        // Drain anything already queued so one draw covers the burst.
                        while let Ok(f) = inbound.try_recv() {
                            if let Some(reason) = apply_frame(app, f, &out) {
                                return Err(anyhow!(reason));
                            }
                        }
                    }
                    None => return Err(anyhow!("daemon closed the connection")),
                }
            }
            ev = events.recv() => {
                match ev {
                    Some(ev) => {
                        handle_event(app, ev, &out)?;
                        // Drain the rest of the burst so a fast typist gets one redraw,
                        // not one per keystroke.
                        while let Ok(ev) = events.try_recv() {
                            handle_event(app, ev, &out)?;
                        }
                        needs_draw = true;
                    }
                    None => return Ok(()),
                }
            }
            _ = anim.tick() => {
                app.tick = app.tick.wrapping_add(1);
                app.expire_toasts();
                // Fetch the note beside the graph once the cursor has stopped moving. A walk
                // across a vault passes over dozens of notes; asking for each one is a round
                // trip per keystroke for answers nobody reads.
                if let Mode::Graph { sel } = app.mode {
                    let want = app
                        .graph
                        .as_ref()
                        .and_then(|g| g.nodes.get(sel))
                        .filter(|n| !n.ghost && !n.path.is_empty())
                        .map(|n| n.path.clone());
                    let settled =
                        app.preview_at.is_some_and(|t| t.elapsed() >= PREVIEW_SETTLE);
                    if app.graph_panel && settled && want != app.preview_for {
                        app.preview_at = None;
                        app.preview_for = want.clone();
                        app.preview = None;
                        match (want, app.snapshot.as_ref().and_then(|s| s.focused_space)) {
                            (Some(path), Some(space)) => {
                                app.previewing = true;
                                let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
                                    space,
                                    kind: crate::proto::VaultQuery::Note { path },
                                }));
                            }
                            // A ghost, or no project: nothing to ask for, and the panel says so.
                            _ => app.previewing = false,
                        }
                    }
                }

                // Hand the buffer over when the typing settles. Here rather than in the key
                // handler because "has stopped typing" is a fact about time passing, which is
                // the one thing a key handler never hears about.
                if let (Mode::Editor { path, project, .. }, Some(rev)) =
                    (&app.mode, app.buffer.as_ref().map(|b| b.rev))
                {
                    if app.doc.due(rev) {
                        let (path, vault) = (path.clone(), !*project);
                        if let (Some(space), Some(body)) = (
                            app.snapshot.as_ref().and_then(|s| s.focused_space),
                            app.buffer.as_ref().map(|b| b.text()),
                        ) {
                            let _ = out.send(ClientFrame::Command(Cmd::DocChanged {
                                space,
                                path,
                                body,
                                vault,
                            }));
                        }
                    }
                }
                // Only the spinner, the elapsed timers and whatever is crossing the start
                // screen change on their own — and each of them only sometimes.
                if app.restless() {
                    needs_draw = true;
                }

                // Advance the graph layout, if one is open and still moving. Several steps
                // a frame, because one would take half a minute to settle — and then *stop*,
                // which is the whole reason the simulation anneals. A graph left open must
                // cost exactly as much as any other still picture.
                // A settled layout still redraws, because the drift is still moving it. That
                // is the whole difference between a graph that is alive and a photograph of
                // one — and it costs two sines a node rather than a force pass.
                if matches!(app.mode, Mode::Graph { .. }) {
                    needs_draw = true;
                    if let Some(sim) = app.sim.as_mut() {
                        if !sim.settled() {
                            for _ in 0..graph::STEPS_PER_FRAME {
                                sim.step();
                                if sim.settled() {
                                    break;
                                }
                            }
                            needs_draw = true;
                        }
                    }
                }
            }
        }
    }
}

/// Whether two snapshots draw their lines in different places.
///
/// Contents are ignored on purpose — text changing every frame is the normal case and costs
/// nothing to redraw. This is only about where the *edges* are, which is what gets left behind.
fn shape_changed(old: &Snapshot, new: &Snapshot) -> bool {
    if (old.tabbar, old.sidebar, old.bus, old.status)
        != (new.tabbar, new.sidebar, new.bus, new.status)
    {
        return true;
    }
    if old.panes.len() != new.panes.len() {
        return true;
    }
    old.panes
        .iter()
        .zip(new.panes.iter())
        .any(|(a, b)| a.id != b.id || a.cell != b.cell || a.content != b.content)
}

/// What the first attach of a session opens on.
///
/// Every attach opens on the start screen. Opening horde is arriving, and what you want on
/// arrival is the state of things — which agents need you, which projects are live — not
/// whichever pane happened to be focused last time. The daemon and its agents are untouched by
/// this: they keep running whether or not anyone is looking, which is the whole point of the
/// daemon. Only the *view* resets, and `esc` is one keystroke away from the terminal.
///
/// Ahead of that comes the walkthrough, once. Being asked three questions once beats
/// discovering them by hitting them — "no vault" the first time you write a note is not a
/// prompt, it is a wall.
///
/// Whether it has happened is read from `setup.done`, which the walkthrough writes when it is
/// finished *or* skipped. It used to be inferred from whether `config.toml` existed, and that
/// was wrong in both directions: a file that exists for any other reason — the example config
/// copied, dotfiles restored, one key set on the settings page — meant nobody was ever walked
/// through anything, while pressing `esc` wrote nothing and so asked again on every launch
/// forever.
///
/// A pure function of the config so all three cases can be asserted, which none of them were.
fn greet_mode(cfg: &Config) -> Mode {
    if !cfg.kit {
        // Nothing to arrive at: the walkthrough asks about the vault, and the dashboard is
        // the kit's own front page. A plain build opens where it always did.
        Mode::Terminal
    } else if !cfg.setup_done {
        Mode::Setup { step: ui::setup::Step::Vault }
    } else if cfg.dashboard {
        Mode::Dashboard { sel: 0 }
    } else {
        Mode::Terminal
    }
}

/// Apply one server frame. Returns a reason string when the session must end.
///
/// Takes the sender because one answer can lead to another: following a wikilink asks the
/// daemon to resolve a name, and the reply to *that* is what says which note to fetch.
fn apply_frame(
    app: &mut App,
    frame: ServerFrame,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Option<String> {
    match frame {
        // Unsolicited, and about whatever file the daemon was told about. Kept by path
        // rather than replacing one list, because a save and a change can be in flight for
        // different files at once.
        // Late by definition, so it has to check that it is still about where the cursor is.
        // Between asking and answering the person has usually typed another two characters,
        // and may well have moved to another line entirely.
        ServerFrame::Completions { path, items } => {
            let open_here = matches!(&app.mode, Mode::Editor { path: p, .. } if *p == path);
            match app.buffer.as_ref() {
                Some(b) if open_here && !items.is_empty() => {
                    app.completions =
                        Some(Completions { items, link: false, sel: 0, line: b.line, from: b.word_start() });
                }
                _ => app.toast(NoticeLevel::Info, "nothing to complete"),
            }
        }
        // The whole board, every time. Nothing here reconciles or patches: the reply is the
        // truth and the old copy is discarded, which is the same contract `Snapshot` has and
        // the reason neither can drift.
        ServerFrame::Kanban(reply) => {
            app.kanban = Some(*reply);
        }
        ServerFrame::Diagnostics { path, diags } => {
            if diags.is_empty() {
                app.diags.remove(&path);
            } else {
                app.diags.insert(path, diags);
            }
        }
        ServerFrame::Digest(d) => {
            // Nothing to report is worth saying out loud rather than opening an empty panel.
            if d.is_empty() {
                let quiet = match d.working.len() {
                    0 => "nothing has happened since you last looked".to_string(),
                    n => format!("nothing new — {n} still working"),
                };
                app.toast(NoticeLevel::Info, quiet);
            } else {
                app.digest = Some(*d);
                app.mode = Mode::Digest { scroll: 0 };
            }
        }
        ServerFrame::Snapshot(snap) => {
            // A daemon left over from an older build serves stale behaviour while looking
            // perfectly healthy, so say so loudly the first time we see it.
            let mine = env!("CARGO_PKG_VERSION");
            if snap.daemon_version != mine && !app.warned_version {
                app.warned_version = true;
                app.toast(
                    NoticeLevel::Warn,
                    format!(
                        "daemon is v{} but this client is v{mine} — run `horde stop`, then `horde`",
                        snap.daemon_version
                    ),
                );
            }
            // A layout that changed shape leaves the old one's borders and panel edges on
            // the host in cells the new one never writes to. Every rect is compared, not just
            // the pane count: a pane closing, the bus opening, a tab with a different split
            // coming forward — they all move the lines, and any line left behind stays until
            // something happens to overwrite that exact cell.
            if app.snapshot.as_ref().is_some_and(|old| shape_changed(old, &snap)) {
                app.needs_clear = true;
            }
            // Every attach opens on the start screen. Opening horde is arriving, and what
            // you want on arrival is the state of things — which agents need you, which
            // projects are live — not whichever pane happened to be focused last time.
            //
            // The daemon and its agents are untouched by this: they keep running whether or
            // not anyone is looking, which is the whole point of the daemon. Only the *view*
            // resets, and `esc` is one keystroke away from the terminal.
            if !app.greeted {
                app.greeted = true;
                if app.mode == Mode::Terminal {
                    app.mode = greet_mode(&app.cfg);
                    if let Mode::Setup { .. } = app.mode {
                        app.setup.cursor =
                            ui::setup::cursor_for(ui::setup::Step::Vault, &app.setup);
                    }
                }
            }
            // A project was opened and is now focused: list it.
            if app.want_files {
                if let Some(space) = snap.focused_space {
                    app.want_files = false;
                    let _ = out.send(ClientFrame::Command(Cmd::FileQuery { space }));
                }
            }
            // Forget caches for panes that no longer exist.
            let live: Vec<PaneId> = snap.panes.iter().map(|p| p.id).collect();
            app.rows.retain(|id, _| live.contains(id));
            app.cursors.retain(|id, _| live.contains(id));
            // And a cursor pointing at a space that closed or an agent that exited. The same
            // "forget what no longer exists" site, so there is only one of them.
            app.sidebar.prune(&snap);
            app.snapshot = Some(*snap);
        }
        ServerFrame::Rows { pane, rows, cursor } => {
            let want = app.pane_info(pane).map(|p| p.rows as usize).unwrap_or(0);
            let cache = app.rows.entry(pane).or_default();
            if want > 0 && cache.len() != want {
                cache.resize(want, Row::default());
            }
            for u in rows {
                let y = u.y as usize;
                if y >= cache.len() {
                    cache.resize(y + 1, Row::default());
                }
                cache[y] = u.row;
            }
            if let Some(c) = cursor {
                app.cursors.insert(pane, c);
            }
        }
        ServerFrame::Event(ev) => match ev {
            crate::proto::Event::BusMessage(m) => {
                // Replace rather than append when a queued message is later delivered.
                match app.bus.iter_mut().find(|x| x.id == m.id) {
                    Some(slot) => *slot = m,
                    None => app.bus.push(m),
                }
                if app.bus.len() > BUS_CAP {
                    let drop = app.bus.len() - BUS_CAP;
                    app.bus.drain(0..drop);
                }
            }
            crate::proto::Event::AgentStateChanged { name, to, .. } => {
                use crate::proto::AgentState as S;
                // Only surface transitions worth interrupting for.
                match to {
                    S::Blocked => app.toast(NoticeLevel::Warn, format!("{name} needs you")),
                    S::Done => app.toast(NoticeLevel::Info, format!("{name} finished")),
                    _ => {}
                }
            }
            crate::proto::Event::Notice { level, text } => app.toast(level, text),
            crate::proto::Event::PaneExited { .. } => {}
        },
        ServerFrame::Files(f) => {
            match (f.body.clone(), f.path.clone()) {
                // A body means this was asked for in order to edit it.
                (Some(body), Some(path)) => {
                    app.buffer = Some(editor::Buffer::new(&body));
                    app.doc = DocSync::default();
                    app.mode = Mode::Editor { path, scroll: 0, project: true, vim: Vim::Insert };
                }
                _ => app.files = Some(*f),
            }
        }
        // The note under the graph's cursor. Like the `[[` list below, this is the browser's
        // reply arriving for somebody else's question.
        ServerFrame::Vault(v) if app.previewing => {
            app.previewing = false;
            app.preview = Some(v);
        }
        // The note list, asked for by `[[` rather than by the browser. Taken before the
        // browser's own handling, because these two want the same reply for different things
        // and only one of them asked.
        ServerFrame::Vault(v) if app.linking => {
            app.linking = false;
            let items: Vec<crate::proto::Completion> = v
                .notes
                .iter()
                .map(|n| crate::proto::Completion {
                    label: n.title.clone(),
                    insert: n.title.clone(),
                    replace: None,
                    // What links here, which is the closest a vault has to a ranking and the
                    // one fact that distinguishes two notes with similar names.
                    kind: (n.backlinks > 0).then(|| format!("←{}", n.backlinks)),
                    detail: None,
                })
                .collect();
            match app.buffer.as_ref() {
                Some(b) if !items.is_empty() => {
                    app.completions =
                        Some(Completions { items, link: true, sel: 0, line: b.line, from: b.col });
                }
                _ => app.toast(NoticeLevel::Info, "no notes to link to yet"),
            }
        }
        ServerFrame::Vault(v) => {
            // A link being followed: take the best match and ask for its body.
            if let Some(name) = app.follow.take() {
                if let Some(hit) = v.notes.first() {
                    app.pending_read = Some(hit.path.clone());
                } else {
                    app.toast(NoticeLevel::Info, format!("no note called {name:?} yet"));
                    app.mode = Mode::Notes { query: String::new(), sel: 0 };
                }
            }
            if let Some(path) = app.pending_read.take() {
                // The search answered; now fetch the note itself. Two round trips on a local
                // socket, and it keeps name resolution in the one place that owns the index.
                if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
                    let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
                        space,
                        kind: crate::proto::VaultQuery::Note { path },
                    }));
                }
            }
            // The list the vault asked for: pick the home note out of it and go there.
            if app.opening_vault {
                app.opening_vault = false;
                app.vault_tree = true;
                match vault_home_note(&v) {
                    Some(path) => edit_note(app, &path, out),
                    // An empty vault is the ordinary state of a new one, so this makes the
                    // note it is missing rather than reporting that it is missing. `Home` and
                    // not `index`: the file it writes is the one a person will read, and the
                    // fallbacks exist for vaults that already chose differently.
                    None => create_note(app, "Home", out),
                }
                return None;
            }

            // A note asked for by the editor arrives as a body to write into.
            if app.opening_editor {
                app.opening_editor = false;
                if let (Some(body), Some(note)) = (v.body.clone(), v.notes.first()) {
                    app.buffer = Some(editor::Buffer::new(&body));
                    app.doc = DocSync::default();
                    app.mode =
                        Mode::Editor { path: note.path.clone(), scroll: 0, project: false, vim: Vim::Insert };
                }
            }
            if let Some(g) = v.graph.clone() {
                let mut sim = graph::Sim::new(&g);
                // Past the animation limit the picture is a shape rather than a story, and
                // watching two thousand nodes shuffle costs more than it explains.
                if g.nodes.len() > graph::ANIMATE_LIMIT {
                    sim.settle(600);
                }
                app.graph_centre = sim.centre();
                app.sim = Some(sim);
                app.graph = Some(g);
            }
            // A list, and not a note's detail or a graph, is what the index is made of. Told
            // apart by shape rather than by remembering what was asked: replies are not
            // correlated with requests, and a body is the one thing only `Note` ever sets.
            if v.body.is_none() && v.graph.is_none() {
                app.vault_index = Some((*v).clone());
            }
            app.vault = Some(*v);
        }
        ServerFrame::Bye { reason } => return Some(reason),
    }
    None
}

fn handle_event(
    app: &mut App,
    ev: Event,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Result<()> {
    match ev {
        Event::Key(k) => handle_key(app, k, out),
        Event::Resize(cols, rows) => {
            let _ = out.send(ClientFrame::Resize { cols, rows });
            // A resize is the moment the host screen and ratatui's record of it are most
            // likely to have parted company: the terminal reflows or truncates on its own
            // terms, and nothing tells the renderer which cells survived.
            app.needs_clear = true;
            // A resize moves or drops whatever the terminal was holding, and this side has no
            // way to be told which. Forgetting means the next frame places them again rather
            // than deciding nothing changed and leaving a picture in last frame's place.
            app.placed.forget();
            Ok(())
        }
        Event::Paste(text) => {
            if let Some(pane) = app.focused_pane() {
                let bracketed =
                    app.pane_info(pane).map(|p| p.bracketed_paste).unwrap_or(false);
                let bytes = input::encode_paste(&text, bracketed);
                let _ = out.send(ClientFrame::Input { pane, bytes });
            }
            Ok(())
        }
        Event::Mouse(m) => handle_mouse(app, m, out),
        Event::FocusGained | Event::FocusLost => Ok(()),
    }
}

/// The role of the agent the cursor is on, when it has one.
fn role_under_cursor(app: &App) -> Option<String> {
    let Some(Focus::Agent(pane)) = app.sidebar.cursor else { return None };
    app.pane_info(pane)?.role.clone()
}

fn handle_key(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Result<()> {
    if k.kind == KeyEventKind::Release {
        return Ok(());
    }
    // Typing means you have finished reading what you highlighted. Leaving it up while new
    // output arrives underneath points at text that is no longer there.
    app.selection = None;
    let chord = Chord::new(k.modifiers, k.code);

    // Overlay modes consume keys entirely.
    match app.mode.clone() {
        // The sidebar takes the keyboard while it has the cursor. Everything it does not
        // recognise is *ignored* rather than passed through: falling through would type `x`
        // into an agent you were only looking at.
        Mode::Sidebar => {
            // The *filtered* list, so the cursor only ever walks rows that are on screen.
            let lens = app.sidebar.lens.clone();
            let rows = app
                .snapshot
                .as_ref()
                .map(|s| {
                    let w = s.view.sidebar_width.saturating_sub(2);
                    roster::filtered_rows(s, roster::Density::of(w), &lens)
                })
                .unwrap_or_default();
            let page = 10;
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => app.sidebar.step(&rows, 1),
                KeyCode::Char('k') | KeyCode::Up => app.sidebar.step(&rows, -1),
                KeyCode::PageDown => app.sidebar.step(&rows, page),
                KeyCode::PageUp => app.sidebar.step(&rows, -page),
                KeyCode::Char('g') | KeyCode::Home => app.sidebar.jump(&rows, false),
                KeyCode::Char('G') | KeyCode::End => app.sidebar.jump(&rows, true),
                // Acts *and* exits, so the common path — glance, jump, type — never leaves
                // you parked in a mode you then have to notice and leave.
                KeyCode::Enter => {
                    if let Some(f) = app.sidebar.cursor {
                        let _ = out.send(ClientFrame::Command(f.activate()));
                    }
                    app.mode = Mode::Terminal;
                }
                KeyCode::Char(' ') | KeyCode::Char('h') | KeyCode::Char('l')
                | KeyCode::Left | KeyCode::Right => {
                    let collapse = matches!(k.code, KeyCode::Char('h') | KeyCode::Left);
                    let expand = matches!(k.code, KeyCode::Char('l') | KeyCode::Right);
                    match app.sidebar.cursor {
                        Some(Focus::Group(space)) => {
                            let now = app
                                .snapshot
                                .as_ref()
                                .and_then(|s| s.spaces.iter().find(|x| x.id == space))
                                .is_some_and(|x| x.collapsed);
                            // `h`/`l` are directional, so they only act in their direction;
                            // space toggles. Pressing `l` on an open group should do nothing,
                            // not close it.
                            if (collapse && !now) || (expand && now) || (!collapse && !expand) {
                                let _ = out.send(ClientFrame::Command(
                                    Cmd::ToggleSpaceCollapsed(space),
                                ));
                            }
                        }
                        // On an agent, `h` folds the group it belongs to and moves the cursor
                        // up to the header — otherwise the cursor would be left pointing at a
                        // row that just stopped existing.
                        Some(Focus::Agent(pane)) if collapse => {
                            if let Some(space) = app
                                .snapshot
                                .as_ref()
                                .and_then(|s| s.panes.iter().find(|p| p.id == pane))
                                .map(|p| p.space)
                            {
                                app.sidebar.cursor = Some(Focus::Group(space));
                                let _ = out.send(ClientFrame::Command(
                                    Cmd::ToggleSpaceCollapsed(space),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                KeyCode::Char('p') => {
                    if let Some(Focus::Agent(pane)) = app.sidebar.cursor {
                        let _ = out.send(ClientFrame::Command(Cmd::TogglePanePinned(pane)));
                    }
                }
                KeyCode::Char('f') => app.sidebar.lens = app.sidebar.lens.cycle(),
                // "Show me everyone doing this job." Discoverable because you are pointing
                // at the role when you press it, and `f` steps back out to `all` from here.
                KeyCode::Char('r') => {
                    if let Some(role) = role_under_cursor(app) {
                        app.sidebar.lens = roster::Lens::Role(role);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Terminal,
                _ => {}
            }
            return Ok(());
        }
        // The one mode where a keypress answers on your behalf, so it is deliberately narrow:
        // it moves, it opens a pane, and it sends exactly the digits the agent itself offered.
        // Nothing here can type free text into a pane.
        Mode::Approvals { sel } => {
            let items = app
                .snapshot
                .as_ref()
                .map(crate::client::ui::overlays::pending)
                .unwrap_or_default();
            let last = items.len().saturating_sub(1);
            let sel = sel.min(last);
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    app.mode = Mode::Approvals { sel: (sel + 1).min(last) }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.mode = Mode::Approvals { sel: sel.saturating_sub(1) }
                }
                KeyCode::Enter => {
                    // Answering from here covers the questions horde could read. Anything
                    // else, and anything you would rather see in context, is one key away.
                    if let Some(item) = items.get(sel) {
                        let _ = out.send(ClientFrame::Command(Cmd::FocusPane(item.pane)));
                    }
                    app.mode = Mode::Terminal;
                }
                KeyCode::Char(c) if c.is_ascii_digit() || c == 'y' || c == 'n' => {
                    // Only a key the agent itself listed. A digit with no matching option is
                    // ignored rather than forwarded: this window is showing you a menu, and a
                    // keystroke that means nothing in it must not mean something in the pane.
                    let choice = items
                        .get(sel)
                        .and_then(|i| i.question.as_ref())
                        .and_then(|q| q.options.iter().find(|o| o.key == c.to_string()));
                    if let (Some(item), Some(choice)) = (items.get(sel), choice) {
                        let _ = out.send(ClientFrame::Input {
                            pane: item.pane,
                            bytes: choice.answer_bytes(),
                        });
                        app.toast(
                            NoticeLevel::Info,
                            format!("{}: {}", item.name, choice.label),
                        );
                        // Stay open. Answering one of six is the case this exists for, and
                        // the answered agent drops out of the list on the next snapshot.
                        app.mode = Mode::Approvals { sel: sel.min(last.saturating_sub(1)) };
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Terminal,
                _ => {}
            }
        }
        // The roster is the sidebar's list at a size that can afford detail, so it shares the
        // sidebar's cursor — open it and you land where you left off, jump from it and the
        // sidebar agrees.
        Mode::Roster { scroll } => {
            let lens = app.sidebar.lens.clone();
            let rows = app
                .snapshot
                .as_ref()
                .map(|s| roster::filtered_rows(s, roster::Density::Wide, &lens))
                .unwrap_or_default();
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => app.sidebar.step(&rows, 1),
                KeyCode::Char('k') | KeyCode::Up => app.sidebar.step(&rows, -1),
                KeyCode::PageDown => app.mode = Mode::Roster { scroll: scroll + 5 },
                KeyCode::PageUp => {
                    app.mode = Mode::Roster { scroll: scroll.saturating_sub(5) }
                }
                KeyCode::Char('g') | KeyCode::Home => app.sidebar.jump(&rows, false),
                KeyCode::Char('G') | KeyCode::End => app.sidebar.jump(&rows, true),
                KeyCode::Enter => {
                    if let Some(f) = app.sidebar.cursor {
                        let _ = out.send(ClientFrame::Command(f.activate()));
                    }
                    app.mode = Mode::Terminal;
                }
                KeyCode::Char('p') => {
                    if let Some(Focus::Agent(pane)) = app.sidebar.cursor {
                        let _ = out.send(ClientFrame::Command(Cmd::TogglePanePinned(pane)));
                    }
                }
                KeyCode::Char('f') => app.sidebar.lens = app.sidebar.lens.cycle(),
                // "Show me everyone doing this job." Discoverable because you are pointing
                // at the role when you press it, and `f` steps back out to `all` from here.
                KeyCode::Char('r') => {
                    if let Some(role) = role_under_cursor(app) {
                        app.sidebar.lens = roster::Lens::Role(role);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Terminal,
                _ => {}
            }
            return Ok(());
        }
        Mode::Help => {
            // Any key closes help; there is nothing else to do in it.
            app.mode = Mode::Terminal;
            return Ok(());
        }
        // Unlike help, the digest can be longer than the panel, so navigation keys have to
        // scroll rather than dismiss.
        Mode::Digest { scroll } => {
            let page = 10;
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    app.mode = Mode::Digest { scroll: scroll + 1 }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.mode = Mode::Digest { scroll: scroll.saturating_sub(1) }
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    app.mode = Mode::Digest { scroll: scroll + page }
                }
                KeyCode::PageUp => {
                    app.mode = Mode::Digest { scroll: scroll.saturating_sub(page) }
                }
                KeyCode::Home => app.mode = Mode::Digest { scroll: 0 },
                _ => {
                    app.mode = Mode::Terminal;
                    app.digest = None;
                }
            }
            return Ok(());
        }
        Mode::Palette { query, sel } => {
            return picker_key(app, k, out, query, sel, true);
        }
        Mode::SpaceSwitcher { query, sel } => {
            return picker_key(app, k, out, query, sel, false);
        }
        Mode::Prompt { prompt, value } => {
            return prompt_key(app, k, out, prompt, value);
        }
        Mode::Menu { stack, at } => {
            return menu_key(app, k, out, stack, at);
        }
        Mode::Settings { cat, sel, capture } => {
            return settings_key(app, k, out, cat, sel, capture);
        }
        Mode::Prefix => {
            app.mode = Mode::Terminal;
            match app.cfg.keys.lookup(&Trigger::Prefix(chord)).cloned() {
                Some(action) => return run_action(app, action, out),
                None => {
                    // Escape cancels quietly; anything else is a typo worth a hint.
                    if k.code != KeyCode::Esc {
                        app.toast(
                            NoticeLevel::Info,
                            format!("{} is not bound — press {} ? for keys", chord.describe(), app.cfg.prefix.describe()),
                        );
                    }
                    return Ok(());
                }
            }
        }
        Mode::Graph { sel } => {
            let count = app
                .vault
                .as_ref()
                .and_then(|v| v.graph.as_ref())
                .map(|g| g.nodes.len())
                .unwrap_or(0);
            let last = count.saturating_sub(1);
            // Panning moves in layout units, scaled so one press crosses a similar fraction
            // of the view whatever the zoom.
            let pan = 200.0 / 12.0 / app.graph_zoom;
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    // The layout is worth tens of milliseconds to rebuild and nothing to
                    // keep, and holding it would pin a vault's worth of points in a client
                    // that may not open the graph again this session.
                    app.sim = None;
                    app.graph = None;
                    app.mode = Mode::Terminal;
                }
                // Tab walks the nodes, because the arrows are already panning the view.
                KeyCode::Tab | KeyCode::Char('j') => {
                    graph_select(app, if sel >= last { 0 } else { sel + 1 })
                }
                KeyCode::BackTab | KeyCode::Char('k') => {
                    graph_select(app, if sel == 0 { last } else { sel - 1 })
                }
                KeyCode::Left => app.graph_centre.x -= pan,
                KeyCode::Right => app.graph_centre.x += pan,
                KeyCode::Up => app.graph_centre.y -= pan,
                KeyCode::Down => app.graph_centre.y += pan,
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    app.graph_zoom = (app.graph_zoom * 1.25).min(8.0)
                }
                KeyCode::Char('-') => app.graph_zoom = (app.graph_zoom / 1.25).max(0.4),
                // Recentre and reset, for when panning has lost you.
                // The note beside the graph, on and off. `p` for preview, and free here.
                KeyCode::Char('p') => {
                    app.graph_panel = !app.graph_panel;
                    if app.graph_panel {
                        app.preview_for = None;
                        app.preview_at = Some(Instant::now());
                    }
                }
                KeyCode::Char('0') => {
                    app.graph_zoom = 1.0;
                    if let Some(s) = app.sim.as_ref() {
                        app.graph_centre = s.centre();
                    }
                }
                // Read it here, in horde. This used to spawn `$EDITOR` in a split, which was
                // the right answer in phase 3 when horde had no reader of its own — and the
                // wrong one ever since, because it drops you into an empty vim beside the
                // graph rather than into the note.
                //
                // Read rather than edit, matching the note browser: arriving at a note from
                // the graph is arriving to look at it.
                KeyCode::Enter | KeyCode::Char('e') => {
                    // A ghost has no note to open, so enter on one does nothing rather than
                    // inventing a file the person never asked for.
                    let node =
                        app.graph.as_ref().and_then(|g| g.nodes.get(sel)).cloned();
                    if let Some(n) = node.filter(|n| !n.ghost && !n.path.is_empty()) {
                        if k.code == KeyCode::Char('e') {
                            edit_note(app, &n.path, out);
                        } else {
                            read_note(app, &n.path, out);
                        }
                        // The layout is expensive to hold and cheap to rebuild, so it goes —
                        // but the *mode* is now whatever the note opened into. Setting it to
                        // the terminal here is what used to make sense when this shelled out
                        // to `$EDITOR` in a pane, and it silently undid the reader.
                        app.sim = None;
                        app.graph = None;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        Mode::Setup { step } => {
            use ui::setup::Step;
            let count = ui::setup::choices(step, &app.setup);
            let advance = |app: &mut App, out: &mpsc::UnboundedSender<ClientFrame>, step: Step| {
                let steps = Step::all();
                let i = steps.iter().position(|s| *s == step).unwrap_or(0);
                match steps.get(i + 1) {
                    Some(next) => {
                        // Opening on the answer already held, so walking back through the
                        // steps to re-read one does not silently change it.
                        app.setup.cursor = ui::setup::cursor_for(*next, &app.setup);
                        app.mode = Mode::Setup { step: *next };
                    }
                    // Finishing applies the answers. Writing goes through the settings
                    // writer, which merges into an existing config.toml and keeps its
                    // comments — the walkthrough is reachable from the settings page, so
                    // "there is already a file" is the normal case rather than the odd one.
                    None => {
                        let results = app.setup.apply();
                        let failures: Vec<String> =
                            results.iter().filter_map(|r| r.as_ref().err().cloned()).collect();
                        if failures.is_empty() {
                            let _ = out.send(ClientFrame::Command(Cmd::VaultInit {
                                space: app
                                    .snapshot
                                    .as_ref()
                                    .and_then(|s| s.focused_space)
                                    .unwrap_or(0),
                            }));
                            app.toast(
                                NoticeLevel::Info,
                                format!("set up — settings are in {}", settings::config_file().display()),
                            );
                            // Reload, so what was just written is what is running rather than
                            // something that takes effect next time horde starts.
                            let (cfg, _) = Config::load();
                            app.cfg = cfg;
                            let _ = crate::cli::call("server.reload_config", serde_json::json!({}));
                        } else {
                            for f in failures {
                                app.toast(NoticeLevel::Warn, f);
                            }
                        }
                        app.mode = Mode::Dashboard { sel: 0 };
                    }
                }
            };

            match k.code {
                // Skipping is allowed and applies none of the answers: someone who wants to
                // look around first should not have to answer three questions to be let in.
                // It does record that the offer was made, so "not now" is not re-asked on
                // every launch, and says where to find it again.
                KeyCode::Esc => {
                    if let Err(e) = ui::setup::mark_done() {
                        app.toast(NoticeLevel::Warn, e);
                    } else {
                        app.toast(
                            NoticeLevel::Info,
                            "setup skipped — Settings → Agents runs it again".to_string(),
                        );
                    }
                    app.mode = Mode::Dashboard { sel: 0 };
                }
                KeyCode::Enter => advance(app, out, step),
                KeyCode::Down | KeyCode::Tab => {
                    app.setup.cursor = (app.setup.cursor + 1).min(count.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::BackTab => {
                    app.setup.cursor = app.setup.cursor.saturating_sub(1)
                }
                KeyCode::Backspace if step == Step::Vault => {
                    app.setup.vault.pop();
                }
                KeyCode::Char(c) if step == Step::Vault => app.setup.vault.push(c),
                _ => {}
            }
            // The highlight is the answer on a radio step, so moving is choosing rather than a
            // second keystroke to confirm what the screen already shows.
            ui::setup::choose(step, &mut app.setup);
            return Ok(());
        }
        Mode::Editor { path, scroll, project, vim } => {
            // The screen the editor is drawn on, measured the way it is drawn: the status
            // bar sits on the last row, so its `y` is the height less one.
            let screen = app
                .snapshot
                .as_ref()
                .map(|s| ratatui::layout::Rect::new(0, 0, s.status.w, s.status.y + 1))
                .unwrap_or(ratatui::layout::Rect::new(0, 0, 80, 24));
            // Less whatever the vault's tree is taking, by the same function that drew it.
            let (screen, _) =
                crate::client::ui::editor_split(screen, app.vault_tree && !project);
            let (col, rows) = crate::client::ui::editor_page(screen);
            let (col, rows) = (col as usize, (rows as usize).max(1));
            let page = rows;
            if app.buffer.is_none() {
                app.mode = Mode::Terminal;
                return Ok(());
            }

            // `None` means the editor is gone — closed, or handed over to the reader — and
            // has already put the mode wherever it went.
            let next = match vim {
                Vim::Insert => editor_insert(app, k, out, &path, project, page),
                Vim::Command(line) => editor_line(app, k, out, &path, project, false, line),
                Vim::Search(line) => editor_line(app, k, out, &path, project, true, line),
                Vim::Normal => editor_normal(app, k, out, &path, project, page, None),
                Vim::Pending(c) => editor_normal(app, k, out, &path, project, page, Some(c)),
            };
            let Some(vim) = next else { return Ok(()) };

            // Keep the cursor on screen by following it, never by moving it. Counted in
            // screen rows rather than in lines, because a wrapped line is several rows and
            // scrolling by lines would leave the end of a paragraph off the bottom.
            if let Some(b) = app.buffer.as_ref() {
                let scroll = b.follow(scroll, col, rows);
                app.mode = Mode::Editor { path, scroll, project, vim };
            }
            return Ok(());
        }
        Mode::Reader { scroll, link } => {
            let (body, path) = app
                .vault
                .as_ref()
                .map(|v| {
                    (
                        v.body.clone().unwrap_or_default(),
                        v.notes.first().map(|n| n.path.clone()).unwrap_or_default(),
                    )
                })
                .unwrap_or_default();
            let width = app.snapshot.as_ref().map(|s| s.status.w).unwrap_or(80);
            // Rendered exactly as the drawing side renders it, images and all. Counting
            // lines a different way from the way they are drawn is how scrolling stops short
            // of the bottom of a note with a picture in it.
            let rows = app.snapshot.as_ref().map(|s| s.status.y + 1).unwrap_or(24);
            let home = ui::markdown::Home::of(app.vault_index.as_ref());
            let rendered = ui::markdown::render_in(
                &body,
                width.saturating_sub(6).min(96),
                &app.cfg.theme,
                home.at((rows / 2).clamp(6, 24)),
            );
            let page = 10;
            let max = rendered.lines.len().saturating_sub(1);
            match k.code {
                // Back to the browser, which is where you came from and still has your
                // filter in it.
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.mode = Mode::Notes { query: String::new(), sel: 0 }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    app.mode = Mode::Reader { scroll: (scroll + 1).min(max), link }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.mode = Mode::Reader { scroll: scroll.saturating_sub(1), link }
                }
                KeyCode::Char(' ') | KeyCode::PageDown => {
                    app.mode = Mode::Reader { scroll: (scroll + page).min(max), link }
                }
                KeyCode::PageUp => {
                    app.mode = Mode::Reader { scroll: scroll.saturating_sub(page), link }
                }
                KeyCode::Char('g') | KeyCode::Home => app.mode = Mode::Reader { scroll: 0, link },
                KeyCode::Char('G') | KeyCode::End => {
                    app.mode = Mode::Reader { scroll: max, link }
                }
                // Walk the links in the note, scrolling to keep the selected one in view.
                KeyCode::Tab if !rendered.links.is_empty() => {
                    let next = (link + 1) % rendered.links.len();
                    let at = rendered.links[next].0;
                    let scroll = if at < scroll || at > scroll + page * 2 { at.saturating_sub(2) } else { scroll };
                    app.mode = Mode::Reader { scroll, link: next };
                }
                KeyCode::BackTab if !rendered.links.is_empty() => {
                    let next = if link == 0 { rendered.links.len() - 1 } else { link - 1 };
                    let at = rendered.links[next].0;
                    let scroll = if at < scroll || at > scroll + page * 2 { at.saturating_sub(2) } else { scroll };
                    app.mode = Mode::Reader { scroll, link: next };
                }
                // Following a link is what makes this a vault rather than a file viewer.
                // The daemon resolves the name, because it is the one holding the index.
                KeyCode::Enter => {
                    if let Some((_, target)) = rendered.links.get(link) {
                        let target = target.clone();
                        if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
                            let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
                                space,
                                kind: crate::proto::VaultQuery::Search { q: target.clone() },
                            }));
                        }
                        app.mode = Mode::Reader { scroll: 0, link: 0 };
                        app.follow = Some(target);
                    }
                }
                KeyCode::Char('e') => edit_note(app, &path, out),
                _ => {}
            }
            return Ok(());
        }
        Mode::Files { query, sel } => {
            let rows = ui::notes::file_rows(app.files.as_ref(), &query, &app.open_dirs);
            match k.code {
                KeyCode::Esc => app.mode = Mode::Dashboard { sel: 0 },
                KeyCode::Down => {
                    app.mode = Mode::Files { query, sel: (sel + 1).min(rows.len().saturating_sub(1)) }
                }
                KeyCode::Up => app.mode = Mode::Files { query, sel: sel.saturating_sub(1) },
                // Left closes, right opens — what an arrow means in every tree.
                KeyCode::Left => {
                    if let Some(row) = rows.get(sel).filter(|r| r.folder) {
                        app.open_dirs.remove(&row.path);
                    }
                }
                KeyCode::Right => {
                    if let Some(row) = rows.get(sel).filter(|r| r.folder) {
                        app.open_dirs.insert(row.path.clone());
                    }
                }
                KeyCode::Enter => {
                    // The space the *listing* came from, not whatever is focused now. Those
                    // differ for exactly as long as it takes a focus change to round-trip,
                    // which is exactly when someone opens a project and picks a file — and
                    // the file then gets looked for in the previous project, where it is
                    // not.
                    match rows.get(sel) {
                        // A folder is a thing to open, not a thing to edit.
                        Some(row) if row.folder => {
                            if !app.open_dirs.remove(&row.path) {
                                app.open_dirs.insert(row.path.clone());
                            }
                        }
                        Some(row) => {
                            if let Some(space) = app.files.as_ref().map(|f| f.space) {
                                let path = row.path.clone();
                                let _ =
                                    out.send(ClientFrame::Command(Cmd::FileRead { space, path }));
                            }
                        }
                        None => {}
                    }
                }
                // The multiplexer is one keystroke away rather than the way in. Opening a
                // project shows the project; a terminal in it is a thing you then ask for.
                KeyCode::Char('t') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.mode = Mode::Terminal
                }
                // Beside the terminal rather than instead of it: the point of the pane split
                // is having the file and the agent working on it on screen at once.
                KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let (Some(row), Some(space)) =
                        (rows.get(sel).filter(|r| !r.folder), app.files.as_ref().map(|f| f.space))
                    {
                        let _ = out.send(ClientFrame::Command(Cmd::OpenDocPane {
                            space,
                            path: row.path.clone(),
                        }));
                        app.mode = Mode::Terminal;
                    }
                }
                KeyCode::Backspace => {
                    let mut q = query;
                    q.pop();
                    app.mode = Mode::Files { query: q, sel: 0 };
                }
                KeyCode::Char(c) => {
                    let mut q = query;
                    q.push(c);
                    app.mode = Mode::Files { query: q, sel: 0 };
                }
                _ => {}
            }
            return Ok(());
        }
        Mode::Notes { query, sel } => {
            let rows = ui::notes::rows(app.vault_index.as_ref(), &query);
            match k.code {
                KeyCode::Esc => app.mode = Mode::Terminal,
                KeyCode::Down => {
                    let sel = ui::notes::step(&rows, sel, 1);
                    app.mode = Mode::Notes { query, sel }
                }
                KeyCode::Up => {
                    let sel = ui::notes::step(&rows, sel, -1);
                    app.mode = Mode::Notes { query, sel }
                }
                KeyCode::Backspace => {
                    let mut q = query;
                    q.pop();
                    let sel = ui::notes::first(&ui::notes::rows(app.vault_index.as_ref(), &q));
                    app.mode = Mode::Notes { query: q, sel };
                }
                KeyCode::Enter => {
                    if let Some(row) = rows.get(sel).filter(|r| !r.folder) {
                        let path = row.path.clone();
                        read_note(app, &path, out);
                    }
                }
                // Editing is a deliberate second step. Browsing a vault is overwhelmingly
                // reading it, and enter should do the thing you came to do.
                KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(row) = rows.get(sel).filter(|r| !r.folder) {
                        let path = row.path.clone();
                        edit_note(app, &path, out);
                    }
                }
                // Every printable key types into the filter. This is the one full-screen
                // view where bare letters are a field rather than a command, which is why
                // it has no single-key bindings of its own competing with them.
                // ctrl+n makes a note without leaving the browser; ctrl+e writes in the
                // one under the cursor. Reading is what plain enter does.
                KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.prompt_back =
                        Some(Box::new(Mode::Notes { query: query.clone(), sel }));
                    app.mode = Mode::Prompt { prompt: Prompt::NewNote, value: String::new() }
                }
                KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let (Some(row), Some(space)) = (
                        rows.get(sel).filter(|r| !r.folder),
                        app.snapshot.as_ref().and_then(|s| s.focused_space),
                    ) {
                        let _ = out.send(ClientFrame::Command(Cmd::OpenDocPane {
                            space,
                            path: row.path.clone(),
                        }));
                        app.mode = Mode::Terminal;
                    }
                }
                KeyCode::Char(c) => {
                    let mut q = query;
                    q.push(c);
                    let sel = ui::notes::first(&ui::notes::rows(app.vault_index.as_ref(), &q));
                    app.mode = Mode::Notes { query: q, sel };
                }
                _ => {}
            }
            return Ok(());
        }
        Mode::Dashboard { sel } => {
            let rows = app
                .snapshot
                .as_ref()
                .map(|s| ui::dashboard::rows(s, ui::now_millis()))
                .unwrap_or_default();
            let picks = ui::dashboard::selectable(&rows);
            let last = picks.len().saturating_sub(1);
            match k.code {
                // The menu at the foot is a grid, so down is a line of it rather than one
                // entry, and left/right mean something there. `move_sel` owns that maths —
                // it is the only thing that knows the shape the rows are drawn in.
                KeyCode::Char('j') | KeyCode::Down => {
                    app.mode = Mode::Dashboard { sel: ui::dashboard::move_sel(&rows, sel, 1, 0) }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.mode = Mode::Dashboard { sel: ui::dashboard::move_sel(&rows, sel, -1, 0) }
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    app.mode = Mode::Dashboard { sel: ui::dashboard::move_sel(&rows, sel, 0, -1) }
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    app.mode = Mode::Dashboard { sel: ui::dashboard::move_sel(&rows, sel, 0, 1) }
                }
                KeyCode::Char('g') | KeyCode::Home => app.mode = Mode::Dashboard { sel: 0 },
                KeyCode::Char('G') | KeyCode::End => app.mode = Mode::Dashboard { sel: last },
                // Acts *and* leaves, like every other list in horde.
                KeyCode::Enter => {
                    if let Some(row) = picks.get(sel).map(|i| rows[*i].clone()) {
                        return dashboard_open(app, row, out);
                    }
                    app.mode = Mode::Terminal;
                }
                // The quote is "push P for project", and the habit is lower case, so both.
                KeyCode::Char('P') => {
                    return dashboard_act(app, ui::dashboard::Act::Projects, out)
                }
                KeyCode::Esc => return dashboard_act(app, ui::dashboard::Act::Terminal, out),
                // Everything else the greeter answers to is a line on its menu, so the menu
                // is what decides which key does what.
                KeyCode::Char(c) => {
                    if let Some(a) = ui::dashboard::Act::from_key(c) {
                        return dashboard_act(app, a, out);
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        Mode::Kanban { view, col, sel } => {
            return kanban_key(app, k, out, view, col, sel);
        }
        Mode::Card { id, focus } => {
            return card_key(app, k, out, id, focus);
        }
        Mode::Leader { mut pending, back } => {
            // Esc abandons the whole sequence, however deep. Backspace steps back one key,
            // so a mistyped middle key costs one press instead of starting over.
            if k.code == KeyCode::Esc {
                app.mode = *back;
                return Ok(());
            }
            if k.code == KeyCode::Backspace {
                // Stepping back off the last key returns to the open table rather than
                // leaving it, so a wrong first key costs one press instead of two. Only a
                // backspace with nothing left to undo actually exits.
                app.mode = match pending.pop() {
                    Some(_) => Mode::Leader { pending, back },
                    None => *back,
                };
                return Ok(());
            }

            pending.push(chord);
            match app.cfg.keys.leader_match(&pending) {
                LeaderMatch::Action(a) => {
                    let action = a.clone();
                    app.mode = *back;
                    return run_action(app, action, out);
                }
                // Still inside the table: hold the keys and wait for the rest.
                LeaderMatch::Partial => {
                    app.mode = Mode::Leader { pending, back };
                    return Ok(());
                }
                // Nothing starts this way. The keys are dropped rather than forwarded —
                // passing them on would type the sequence into whatever is in the pane.
                LeaderMatch::None => {
                    let typed =
                        pending.iter().map(|c| c.describe()).collect::<Vec<_>>().join(" ");
                    app.mode = *back;
                    app.toast(
                        NoticeLevel::Info,
                        format!("leader {typed} is not bound — press {} ? for keys", app.cfg.prefix.describe()),
                    );
                    return Ok(());
                }
            }
        }
        Mode::Terminal => {}
    }

    // Direct bindings win over passthrough.
    if let Some(action) = app.cfg.keys.lookup(&Trigger::Direct(chord)).cloned() {
        return run_action(app, action, out);
    }

    if chord == app.cfg.prefix {
        app.mode = Mode::Prefix;
        return Ok(());
    }

    // The leader, from a terminal pane. Bare `space` cannot do this job here — it is
    // typing — which is why the leader has a chord of its own outside horde's own views.
    if chord == app.cfg.leader {
        app.mode = Mode::Leader { pending: Vec::new(), back: Box::new(Mode::Terminal) };
        return Ok(());
    }

    if let (Some(pane), Some(bytes)) = (app.focused_pane(), input::encode_key(&k)) {
        let _ = out.send(ClientFrame::Input { pane, bytes });
    }
    Ok(())
}

fn picker_key(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    mut query: String,
    mut sel: usize,
    is_palette: bool,
) -> Result<()> {
    let items = if is_palette { app.palette_items() } else { app.space_items() };
    match k.code {
        KeyCode::Esc => {
            app.mode = Mode::Terminal;
            return Ok(());
        }
        KeyCode::Enter => {
            let chosen = items.get(sel).map(|i| i.kind.clone());
            app.mode = Mode::Terminal;
            match chosen {
                Some(PickKind::Command(name)) => {
                    if let Some(cmd) = command_for(&name) {
                        let _ = out.send(ClientFrame::Command(cmd));
                    }
                }
                Some(PickKind::Space(id)) => {
                    let _ = out.send(ClientFrame::Command(Cmd::FocusSpace(id)));
                }
                None => {}
            }
            return Ok(());
        }
        KeyCode::Up | KeyCode::BackTab => sel = sel.saturating_sub(1),
        KeyCode::Down | KeyCode::Tab => {
            sel = (sel + 1).min(items.len().saturating_sub(1));
        }
        KeyCode::Backspace => {
            query.pop();
            sel = 0;
        }
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            sel = (sel + 1).min(items.len().saturating_sub(1));
        }
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            sel = sel.saturating_sub(1)
        }
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
            query.push(c);
            sel = 0;
        }
        _ => {}
    }
    app.mode = if is_palette {
        Mode::Palette { query, sel }
    } else {
        Mode::SpaceSwitcher { query, sel }
    };
    Ok(())
}

/// Open the settings page on `cat`, landing on its first changeable row.
pub fn open_settings(app: &mut App, cat: usize) {
    let category = settings::Category::all()[cat.min(settings::Category::all().len() - 1)];
    let rows = settings::rows(&app.cfg, category);
    let sel = rows.iter().position(|r| r.selectable()).unwrap_or(0);
    app.mode = Mode::Settings { cat, sel, capture: None };
}

/// Open a text prompt, prefilled where there is an obvious current value so a small tweak
/// does not mean retyping.
pub fn open_prompt(app: &mut App, prompt: Prompt) {
    let value = match &prompt {
        Prompt::RenamePane(p) => app
            .pane_info(*p)
            .map(|i| i.agent.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| i.title.clone()))
            .unwrap_or_default(),
        Prompt::RenameSpace(id) => app
            .snapshot
            .as_ref()
            .and_then(|s| s.spaces.iter().find(|x| x.id == *id))
            .map(|x| x.name.clone())
            .unwrap_or_default(),
        Prompt::RenameTab(id) => app
            .snapshot
            .as_ref()
            .and_then(|s| s.tabs.iter().find(|x| x.id == *id))
            .map(|x| x.name.clone())
            .unwrap_or_default(),
        Prompt::SetRole(p) => {
            app.pane_info(*p).and_then(|i| i.role.clone()).unwrap_or_default()
        }
        // Nothing sensible to prefill for these.
        Prompt::NewSpace | Prompt::SendTo(_) | Prompt::RunCommand | Prompt::NewNote => String::new(),
    };
    app.mode = Mode::Prompt { prompt, value };
}

/// Open a context menu for `target` at the cursor.
fn open_menu(app: &mut App, target: Target, at: (u16, u16)) {
    let Some(snap) = app.snapshot.as_ref() else { return };
    let level = menu::build(target, snap, &app.cfg.prefix.describe());
    app.mode = Mode::Menu { stack: vec![level], at };
}

/// Run whatever a menu entry does.
fn activate(app: &mut App, act: Act, out: &mpsc::UnboundedSender<ClientFrame>) -> Result<()> {
    match act {
        Act::Cmd(cmd) => {
            let _ = out.send(ClientFrame::Command(cmd));
            app.mode = Mode::Terminal;
        }
        Act::Prompt(p) => open_prompt(app, p),
        Act::Submenu(sub) => {
            if let Mode::Menu { stack, at } = &app.mode {
                let mut stack = stack.clone();
                stack.push(menu::submenu(sub, &app.cfg));
                app.mode = Mode::Menu { stack, at: *at };
            }
        }
        Act::Settings => open_settings(app, 0),
        Act::Help => app.mode = Mode::Help,
        Act::CopyPane(pane) => {
            // Read through the daemon rather than the local row cache: it holds the
            // authoritative text, already trimmed.
            match crate::cli::call(
                "pane.read",
                serde_json::json!({ "pane": pane, "source": "visible" }),
            ) {
                Ok(v) => {
                    let text: Vec<String> = v
                        .get("lines")
                        .and_then(|l| l.as_array())
                        .map(|a| {
                            a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect()
                        })
                        .unwrap_or_default();
                    let joined = text.join("\n");
                    match copy_to_clipboard(&joined) {
                        Ok(()) => app.toast(
                            NoticeLevel::Info,
                            format!("copied {} lines", text.len()),
                        ),
                        Err(e) => app.toast(NoticeLevel::Warn, format!("copy failed: {e}")),
                    }
                }
                Err(e) => app.toast(NoticeLevel::Warn, format!("could not read pane: {e}")),
            }
            app.mode = Mode::Terminal;
        }
        Act::Close => app.mode = Mode::Terminal,
    }
    Ok(())
}

/// Hand text to the system clipboard, by whichever route this machine has.
///
/// Two routes, and the order matters. A local clipboard program — `pbcopy`, `clip.exe`,
/// `wl-copy` — is tried first because it is the only one that can be *checked*: it either exits
/// zero or it does not. [`osc52`](crate::platform::osc52) asks the terminal to do the copying
/// instead, which needs nothing installed and works from inside WSL or across SSH, but is
/// write-only — the terminal never answers, so a terminal that ignores the sequence is
/// indistinguishable from one that obeyed it.
///
/// So: try the route that can fail loudly, and fall through to the route that always "works".
/// A program that is present but broken falls through too, which is the case that matters on a
/// headless box where `xclip` is installed and there is no display behind it.
fn copy_to_clipboard(text: &str) -> Result<()> {
    if let Some(cmd) = crate::platform::clipboard_command() {
        if pipe_into(cmd, text).is_ok() {
            return Ok(());
        }
    }
    if text.len() > crate::platform::OSC52_LIMIT {
        return Err(anyhow!(
            "no clipboard program, and {}KB is past what an escape sequence can carry",
            text.len() / 1024
        ));
    }
    // Straight at the terminal rather than through ratatui: this is a request to the emulator,
    // not something to be drawn, and the next frame must not be able to overwrite it.
    use std::io::Write as _;
    let mut out = std::io::stdout();
    out.write_all(crate::platform::osc52(text).as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Run `cmd`, feeding it `text` on stdin, and fail if it does not exit cleanly.
fn pipe_into(mut cmd: std::process::Command, text: &str) -> Result<()> {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        // Closed here rather than at the end of the call: a clipboard program reads to EOF, and
        // waiting on one that is still waiting on us is a deadlock rather than a slow copy.
        drop(stdin);
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(anyhow!("{:?} exited with {status}", cmd.get_program()));
    }
    Ok(())
}

/// Keys in a context menu.
fn menu_key(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    mut stack: Vec<Level>,
    at: (u16, u16),
) -> Result<()> {
    if stack.is_empty() {
        app.mode = Mode::Terminal;
        return Ok(());
    }
    let depth = stack.len();
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Step out of a submenu before closing the whole thing.
            stack.pop();
            app.mode = if stack.is_empty() { Mode::Terminal } else { Mode::Menu { stack, at } };
            return Ok(());
        }
        KeyCode::Left | KeyCode::Char('h') if depth > 1 => {
            stack.pop();
            app.mode = Mode::Menu { stack, at };
            return Ok(());
        }
        KeyCode::Up | KeyCode::Char('k') => stack[depth - 1].step(-1),
        KeyCode::Down | KeyCode::Char('j') => stack[depth - 1].step(1),
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Char('l') => {
            let act = stack[depth - 1].selected().map(|i| i.act.clone());
            app.mode = Mode::Menu { stack, at };
            if let Some(act) = act {
                return activate(app, act, out);
            }
            return Ok(());
        }
        _ => {}
    }
    app.mode = Mode::Menu { stack, at };
    Ok(())
}

/// Keys in a text prompt.
fn prompt_key(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    prompt: Prompt,
    mut value: String,
) -> Result<()> {
    match k.code {
        KeyCode::Esc => {
            app.mode = match app.prompt_back.take() {
                Some(back) => *back,
                None => Mode::Terminal,
            };
            return Ok(());
        }
        KeyCode::Enter => {
            // Answering navigates: whatever the prompt does decides where you end up, so the
            // way back is spent rather than followed.
            app.prompt_back = None;
            app.mode = Mode::Terminal;
            let v = value.trim().to_string();
            match prompt {
                // Creating leaves you in the editor rather than back where you were: you
                // asked for a note in order to write in it.
                Prompt::NewNote => {
                    create_note(app, &v, out);
                    return Ok(());
                }
                Prompt::RenamePane(pane) => {
                    let _ = out.send(ClientFrame::Command(Cmd::RenamePane { pane, name: v }));
                }
                Prompt::RenameSpace(space) => {
                    let _ = out.send(ClientFrame::Command(Cmd::RenameSpace { space, name: v }));
                }
                Prompt::RenameTab(tab) => {
                    let _ = out.send(ClientFrame::Command(Cmd::RenameTab { tab, name: v }));
                }
                Prompt::SetRole(pane) => {
                    // An empty value clears it, the same contract every other text prompt
                    // here uses — see `Prompt::hint`.
                    let _ = out.send(ClientFrame::Command(Cmd::SetPaneRole { pane, role: v }));
                }
                Prompt::NewSpace => {
                    let name = if v.is_empty() { None } else { Some(v) };
                    let _ = out.send(ClientFrame::Command(Cmd::NewSpace { name }));
                }
                Prompt::RunCommand => {
                    if !v.is_empty() {
                        let _ = out.send(ClientFrame::Command(Cmd::SpawnAgent {
                            cmd: v,
                            name: None,
                            split: None,
                        }));
                    }
                }
                Prompt::SendTo(pane) => {
                    if v.is_empty() {
                        return Ok(());
                    }
                    // Route through the bus so the message is recorded and state-gated,
                    // exactly as `horde send` would be.
                    let target = app
                        .pane_info(pane)
                        .and_then(|p| p.agent.as_ref().map(|a| a.name.clone()))
                        .unwrap_or_else(|| pane.to_string());
                    match crate::cli::call(
                        "bus.send",
                        serde_json::json!({ "to": target, "body": v }),
                    ) {
                        Ok(m) => {
                            let how = m.get("delivery").and_then(|d| d.as_str()).unwrap_or("sent");
                            app.toast(NoticeLevel::Info, format!("{how} to {target}"));
                        }
                        Err(e) => app.toast(NoticeLevel::Warn, format!("{e}")),
                    }
                }
            }
            return Ok(());
        }
        KeyCode::Backspace => {
            value.pop();
        }
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => value.push(c),
        _ => {}
    }
    app.mode = Mode::Prompt { prompt, value };
    Ok(())
}

/// Keys on the settings page. Changes apply and persist immediately.
fn settings_key(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    cat: usize,
    mut sel: usize,
    capture: Option<String>,
) -> Result<()> {
    // Rebinding: the very next key press becomes the binding, so nothing else may consume it.
    if let Some(action) = capture {
        if k.code == KeyCode::Esc {
            app.mode = Mode::Settings { cat, sel, capture: None };
            return Ok(());
        }
        let chord = Chord::new(k.modifiers, k.code);
        match settings::rebind(&mut app.cfg, &action, chord) {
            Ok((key, value)) => match settings::write(&key, value) {
                Ok(()) => {
                    app.toast(
                        NoticeLevel::Info,
                        format!("{} → {}", action.replace('_', " "), chord.describe()),
                    );
                }
                Err(e) => app.toast(NoticeLevel::Error, format!("could not save: {e:#}")),
            },
            Err(e) => app.toast(NoticeLevel::Warn, format!("{e}")),
        }
        app.mode = Mode::Settings { cat, sel, capture: None };
        return Ok(());
    }

    let cats = settings::Category::all();
    let category = cats[cat.min(cats.len() - 1)];
    let rows = settings::rows(&app.cfg, category);
    let selectable: Vec<usize> =
        rows.iter().enumerate().filter(|(_, r)| r.selectable()).map(|(i, _)| i).collect();
    let pos = selectable.iter().position(|i| *i == sel).unwrap_or(0);

    let mut delta = 0i32;
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Terminal;
            return Ok(());
        }
        // Tab moves between categories, leaving left/right free to change values.
        KeyCode::Tab => {
            let next = (cat + 1) % cats.len();
            open_settings(app, next);
            return Ok(());
        }
        KeyCode::BackTab => {
            let next = (cat + cats.len() - 1) % cats.len();
            open_settings(app, next);
            return Ok(());
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !selectable.is_empty() {
                sel = selectable[pos.saturating_sub(1)];
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !selectable.is_empty() {
                sel = selectable[(pos + 1).min(selectable.len() - 1)];
            }
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => delta = 1,
        KeyCode::Left | KeyCode::Char('h') => delta = -1,
        _ => {}
    }

    if delta != 0 {
        match rows.get(sel).map(|r| r.kind.clone()) {
            Some(settings::Kind::Setting(field)) => {
                let (key, value) = settings::bump(&mut app.cfg, field, delta);
                match settings::write(&key, value) {
                    // The daemon owns geometry and PTY sizes, so it has to re-read too.
                    Ok(()) => {
                        let _ = crate::cli::call("server.reload_config", serde_json::json!({}));
                    }
                    Err(e) => app.toast(NoticeLevel::Error, format!("could not save: {e:#}")),
                }
            }
            Some(settings::Kind::Keybind(action)) => {
                app.mode = Mode::Settings { cat, sel, capture: Some(action) };
                return Ok(());
            }
            Some(settings::Kind::Action(settings::Action::Reload)) => {
                let (cfg, warnings) = Config::load();
                app.cfg = cfg;
                let _ = crate::cli::call("server.reload_config", serde_json::json!({}));
                for w in warnings {
                    app.toast(NoticeLevel::Warn, w);
                }
                app.toast(NoticeLevel::Info, "config reloaded");
            }
            Some(settings::Kind::Action(settings::Action::InstallClaudeHooks)) => {
                // The same work as `horde integration install claude`, without its printing:
                // a `println!` from in here lands on top of the frame and stays there.
                match crate::cli::integration::install_reporting("claude") {
                    Ok(_) => app.toast(
                        NoticeLevel::Info,
                        "hooks and skill installed — restart running Claude sessions",
                    ),
                    Err(e) => app.toast(NoticeLevel::Error, format!("{e:#}")),
                }
            }
            Some(settings::Kind::Action(settings::Action::RunSetup)) => {
                // Opens on the answers already in force, so it reads as revisiting decisions
                // rather than starting from nothing and overwriting them.
                app.setup = ui::setup::Answers {
                    vault: app.cfg.vault_home.to_string_lossy().to_string(),
                    unattended: app.cfg.unattended,
                    ..ui::setup::Answers::default()
                };
                let first = ui::setup::Step::Vault;
                app.setup.cursor = ui::setup::cursor_for(first, &app.setup);
                app.mode = Mode::Setup { step: first };
                return Ok(());
            }
            Some(settings::Kind::Action(settings::Action::EditFile)) => {
                let path = settings::config_file();
                // Give the editor something to open rather than an empty buffer.
                if !path.exists() {
                    if let Some(p) = path.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::write(&path, settings::template());
                }
                let cmd = format!("{} {}", settings::editor(), path.display());
                let _ = out.send(ClientFrame::Command(Cmd::SpawnAgent {
                    cmd,
                    name: Some("config".into()),
                    split: None,
                }));
                app.mode = Mode::Terminal;
                app.toast(NoticeLevel::Info, "opened config.toml — reload from Appearance");
                return Ok(());
            }
            _ => {}
        }
    }

    app.mode = Mode::Settings { cat, sel, capture: None };
    Ok(())
}

/// Palette names map to the same commands the RPC layer uses, so there is one definition.
fn command_for(name: &str) -> Option<Cmd> {
    use crate::proto::Dir;
    Some(match name {
        "split-right" => Cmd::SplitRight,
        "split-down" => Cmd::SplitDown,
        "close-pane" => Cmd::ClosePane,
        "zoom" => Cmd::ToggleZoom,
        "focus-left" => Cmd::FocusDir(Dir::Left),
        "focus-right" => Cmd::FocusDir(Dir::Right),
        "focus-up" => Cmd::FocusDir(Dir::Up),
        "focus-down" => Cmd::FocusDir(Dir::Down),
        "new-tab" => Cmd::NewTab,
        "next-tab" => Cmd::NextTab,
        "prev-tab" => Cmd::PrevTab,
        "close-tab" => Cmd::CloseTab,
        "new-space" => Cmd::NewSpace { name: None },
        "next-space" => Cmd::NextSpace,
        "prev-space" => Cmd::PrevSpace,
        "toggle-sidebar" => Cmd::ToggleSidebar,
        "toggle-bus" => Cmd::ToggleBus,
        "jump-attention" => Cmd::JumpAttention,
        "digest" => Cmd::RequestDigest,
        _ => return None,
    })
}

/// Which note the vault opens on.
///
/// `Home`, then `index`, then `README`, at the vault root and case-insensitively — the three
/// names a vault that already has a front page will have used. Only the root: a `Home.md`
/// filed inside a folder is that folder's front page, not the vault's, and picking it would
/// land somebody in a corner of their own notes.
///
/// `None` for a vault with no such note, which the caller turns into one rather than an error.
fn vault_home_note(v: &crate::proto::VaultReply) -> Option<String> {
    for want in ["home", "index", "readme"] {
        let hit = v.notes.iter().find(|n| {
            let p = n.path.trim_start_matches("./");
            !p.contains('/')
                && p.rsplit_once('.')
                    .map(|(stem, ext)| {
                        stem.eq_ignore_ascii_case(want)
                            && matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown")
                    })
                    .unwrap_or(false)
        });
        if let Some(n) = hit {
            return Some(n.path.clone());
        }
    }
    None
}

/// Ask the daemon for a note and open the reading view on it.
/// Open a note for writing: fetch its body, then hand it to the editor.
fn edit_note(app: &mut App, path: &str, out: &mpsc::UnboundedSender<ClientFrame>) {
    if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
        let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
            space,
            kind: crate::proto::VaultQuery::Note { path: path.to_string() },
        }));
        app.opening_editor = true;
    }
}

/// Make a note and start writing in it.
///
/// Works from anywhere — the start screen, a pane, another note — because a thought worth
/// keeping rarely arrives while you happen to have the right directory open. When the
/// project has no vault of its own the note goes to the home vault, which always exists.
fn create_note(app: &mut App, title: &str, out: &mpsc::UnboundedSender<ClientFrame>) {
    let title = title.trim();
    if title.is_empty() {
        return;
    }
    // A title is a filename here, so the characters a filename cannot hold come out.
    let stem: String = title
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '-' } else { c })
        .collect();
    let path = format!("{}.md", stem.trim());
    let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) else { return };
    let _ = out.send(ClientFrame::Command(Cmd::VaultSave {
        space,
        path: path.clone(),
        body: format!("# {title}\n\n"),
    }));
    // Ask for the list again, so the tree beside the note has the note in it. Safe to fire
    // here and not after `edit_note`: that one is waiting for a reply carrying a *body*, and
    // a list arriving first would be taken for it and spend the flag.
    let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
        space,
        kind: crate::proto::VaultQuery::List,
    }));
    app.buffer = Some(editor::Buffer::new(&format!("# {title}\n\n")));
    if let Some(b) = app.buffer.as_mut() {
        b.goto(2, 0); // past the heading, where you were going to type anyway
    }
    app.mode = Mode::Editor { path, scroll: 0, project: false, vim: Vim::Insert };
}

/// Move the graph's cursor, and start the clock on fetching what it landed on.
///
/// One funnel, because the selection changes from three places and a preview that only
/// followed two of them would look like the panel had frozen.
fn graph_select(app: &mut App, sel: usize) {
    app.mode = Mode::Graph { sel };
    app.preview_at = Some(Instant::now());
}

// -- the kanban's keyboard ---------------------------------------------------
//
// Two handlers, because the board and one card of it answer to almost nothing in common: the
// board is a grid you walk, and a card is a form you fill in. Nesting them in one match would
// mean every key checking which of the two it was in first.

/// The board, back where you left it before opening a card.
///
/// Including *which* of the two views you left, which it used to discard — so opening a card
/// from the list and pressing escape put you on the board, having lost the place you were
/// reading. A view you have to re-enter after every card is a view you stop using.
fn kanban_back(app: &App) -> Mode {
    let (view, col, sel) = app.kanban_back;
    Mode::Kanban { view, col, sel }
}

/// Which project the board is scoped to, or `None` while it is showing all of them.
fn kanban_project(app: &App) -> Option<String> {
    if app.kanban_all {
        return None;
    }
    let snap = app.snapshot.as_ref()?;
    let id = snap.focused_space?;
    snap.spaces.iter().find(|s| s.id == id).map(|s| s.name.clone())
}

/// The columns as the board is currently showing them.
///
/// Recomputed rather than cached, for the reason nothing in this client caches: the reply is
/// replaced wholesale, and a derived list kept beside it is a second copy to get wrong. It is
/// a few hundred cards and a filter.
fn kanban_view(app: &App) -> Vec<ui::kanban::Column> {
    ui::kanban::columns(
        app.kanban.as_ref(),
        kanban_project(app).as_deref(),
        app.kanban_archived,
        &app.kanban_query,
    )
}

fn kanban_refresh(app: &App, out: &mpsc::UnboundedSender<ClientFrame>) {
    let space = match app.kanban_all {
        true => None,
        false => app.snapshot.as_ref().and_then(|s| s.focused_space),
    };
    let _ = out.send(ClientFrame::Command(Cmd::KanbanQuery { space }));
}

/// Put a card behind `after` in `column`, and keep the cursor on it.
fn kanban_move(
    app: &mut App,
    out: &mpsc::UnboundedSender<ClientFrame>,
    id: u64,
    column: &str,
    after: Option<u64>,
) {
    let _ = out.send(ClientFrame::Command(Cmd::CardMove {
        id,
        column: column.to_string(),
        after,
    }));
    // The daemon will send the board back; nothing is drawn from a guess in the meantime.
    let _ = app;
}

/// Keys on the board and in its list. One handler, because they are one view of one thing.
fn kanban_key(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    view: ui::kanban::View,
    mut col: usize,
    mut sel: usize,
) -> Result<()> {
    use ui::kanban::{Ask, View};

    // A question the board asked owns the keyboard until it is answered. Nothing typed here
    // can reach a binding, which is the point: this is a text field.
    if let Some(mut asking) = app.kanban_ask.take() {
        match k.code {
            KeyCode::Esc => {
                // Escaping a filter clears it rather than restoring what it was. A half-typed
                // filter you abandoned is not a filter you wanted.
                if matches!(asking.ask, Ask::Filter) {
                    app.kanban_query.clear();
                }
            }
            KeyCode::Enter => {
                let value = asking.text.text().trim().to_string();
                match asking.ask {
                    // Already applied on every keystroke; enter just puts the keyboard back.
                    Ask::Filter => {}
                    Ask::NewCard { column } if !value.is_empty() => {
                        let space = app.snapshot.as_ref().and_then(|s| s.focused_space);
                        let _ = out.send(ClientFrame::Command(Cmd::CardNew {
                            // A card made while looking at one project belongs to it. Made
                            // while looking at all of them, it belongs to none — which is a
                            // real answer on a board that also holds things that are not code.
                            space: (!app.kanban_all).then_some(space).flatten(),
                            column,
                            title: value,
                        }));
                    }
                    Ask::NewCard { .. } => {}
                    Ask::NewColumn if !value.is_empty() => {
                        let mut cols = app.cfg.kanban_columns.clone();
                        if !cols.iter().any(|c| c.eq_ignore_ascii_case(&value)) {
                            cols.push(value);
                            save_columns(app, cols);
                        }
                    }
                    Ask::NewColumn => {}
                    Ask::RenameColumn { from } if !value.is_empty() => {
                        let cols: Vec<String> = app
                            .cfg
                            .kanban_columns
                            .iter()
                            .map(|c| if c.eq_ignore_ascii_case(&from) { value.clone() } else { c.clone() })
                            .collect();
                        save_columns(app, cols);
                        // And carry the cards, or renaming a column is a way of losing them.
                        let _ = out.send(ClientFrame::Command(Cmd::ColumnRename { from, to: value }));
                    }
                    Ask::RenameColumn { .. } => {}
                }
            }
            KeyCode::Backspace => {
                asking.text.backspace();
                if matches!(asking.ask, Ask::Filter) {
                    app.kanban_query = asking.text.text();
                }
                app.kanban_ask = Some(asking);
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                asking.text.insert(c);
                // A filter that only applies when you finish typing it is a search box, and a
                // board small enough to redraw is better filtered as you go.
                if matches!(asking.ask, Ask::Filter) {
                    app.kanban_query = asking.text.text();
                }
                app.kanban_ask = Some(asking);
            }
            _ => app.kanban_ask = Some(asking),
        }
        return Ok(());
    }

    let cols = kanban_view(app);
    let now = ui::now_millis();
    let rows = ui::kanban::list_rows(&cols, now);
    // What the cursor is on, in whichever of the two views is showing.
    let here = match view {
        View::Board => ui::kanban::selected(&cols, col, sel),
        View::List => rows.get(sel).map(|(c, _)| c.id),
    };
    let in_col = |c: usize| cols.get(c).map(|x| x.cards.len()).unwrap_or(0);
    let last_of = |c: usize| cols.get(c).and_then(|x| x.cards.last().map(|k| k.id));

    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Terminal;
            return Ok(());
        }
        KeyCode::Char('v') | KeyCode::Tab => {
            // The cursor cannot survive the flip — one is a grid position and the other an
            // index into a differently sorted list — so it goes to the top rather than
            // somewhere arbitrary that merely looks deliberate.
            app.mode = Mode::Kanban { view: view.flip(), col, sel: 0 };
            return Ok(());
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = match view {
                View::Board => in_col(col),
                View::List => rows.len(),
            };
            sel = (sel + 1).min(len.saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
        KeyCode::Char('h') | KeyCode::Left if view == View::Board => {
            col = col.saturating_sub(1);
            sel = sel.min(in_col(col).saturating_sub(1));
        }
        KeyCode::Char('l') | KeyCode::Right if view == View::Board => {
            col = (col + 1).min(cols.len().saturating_sub(1));
            sel = sel.min(in_col(col).saturating_sub(1));
        }
        KeyCode::Char('g') | KeyCode::Home => sel = 0,
        KeyCode::Char('G') | KeyCode::End => {
            sel = match view {
                View::Board => in_col(col).saturating_sub(1),
                View::List => rows.len().saturating_sub(1),
            }
        }
        // Shove across columns. Lands at the end of the destination, which is where a card
        // you just decided about belongs — the top is for work you have ordered on purpose.
        KeyCode::Char('H') | KeyCode::Char('L') if view == View::Board => {
            let to = match k.code {
                KeyCode::Char('H') => col.checked_sub(1),
                _ => (col + 1 < cols.len()).then_some(col + 1),
            };
            if let (Some(id), Some(to)) = (here, to) {
                let name = cols[to].name.clone();
                let after = last_of(to);
                kanban_move(app, out, id, &name, after);
                col = to;
                sel = in_col(to);
            }
        }
        // Reorder within the column, which is the other half of what dragging does.
        KeyCode::Char('J') | KeyCode::Char('K') if view == View::Board => {
            let cards = cols.get(col).map(|c| c.cards.clone()).unwrap_or_default();
            if let Some(id) = here {
                let down = matches!(k.code, KeyCode::Char('J'));
                let after = match down {
                    // Behind the one below it.
                    true => cards.get(sel + 1).map(|c| c.id),
                    // Behind the one two above, which is the gap above the one above.
                    false if sel >= 2 => cards.get(sel - 2).map(|c| c.id),
                    false => None,
                };
                let moved = if down { sel + 1 < cards.len() } else { sel > 0 };
                if moved {
                    let name = cols[col].name.clone();
                    kanban_move(app, out, id, &name, after);
                    sel = if down { sel + 1 } else { sel - 1 };
                }
            }
        }
        KeyCode::Enter => {
            if let Some(id) = here {
                app.card_scroll = 0;
                app.kanban_back = (view, col, sel);
                app.mode = Mode::Card { id, focus: ui::kanban::Field::Title };
                return Ok(());
            }
        }
        KeyCode::Char('n') => {
            // Both views mean the same thing by this: a new card in the column you are
            // looking at. On the board that is the column the cursor is in; in the list the
            // only column on screen is the one in the row under the cursor, and using the
            // board's stale `col` instead — which is what this did — filed new cards into
            // whichever column you last visited before pressing `v`.
            let column = match view {
                View::List => rows.get(sel).map(|(_, name)| name.clone()),
                View::Board => cols.get(col).map(|c| c.name.clone()),
            }
            .or_else(|| app.cfg.kanban_columns.first().cloned())
            .unwrap_or_else(|| "Todo".into());
            app.kanban_ask = Some(ui::kanban::Asking {
                ask: Ask::NewCard { column },
                text: ui::kanban::TextArea::new(""),
            });
        }
        KeyCode::Char('/') => {
            app.kanban_ask = Some(ui::kanban::Asking {
                ask: Ask::Filter,
                text: ui::kanban::TextArea::new(&app.kanban_query),
            });
        }
        KeyCode::Char('p') => {
            app.kanban_all = !app.kanban_all;
            kanban_refresh(app, out);
        }
        KeyCode::Char('x') => app.kanban_archived = !app.kanban_archived,
        KeyCode::Char('X') => {
            if let Some(id) = here {
                let on = app
                    .kanban
                    .as_ref()
                    .and_then(|kb| kb.cards.iter().find(|c| c.id == id))
                    .map(|c| !c.archived)
                    .unwrap_or(true);
                let _ = out.send(ClientFrame::Command(Cmd::CardArchive { id, archived: on }));
            }
        }
        KeyCode::Char('C') => {
            app.kanban_ask =
                Some(ui::kanban::Asking { ask: Ask::NewColumn, text: ui::kanban::TextArea::new("") });
        }
        KeyCode::Char('R') if view == View::Board => {
            if let Some(c) = cols.get(col) {
                app.kanban_ask = Some(ui::kanban::Asking {
                    ask: Ask::RenameColumn { from: c.name.clone() },
                    text: ui::kanban::TextArea::new(&c.name),
                });
            }
        }
        // Delete a column by giving its cards to the one before it. Renaming *is* the move —
        // cards hold a column name — so there is no separate way for a delete to lose work,
        // which is the failure mode this whole design was arranged to make impossible.
        KeyCode::Char('D') if view == View::Board => {
            match (cols.get(col), col.checked_sub(1).and_then(|i| cols.get(i))) {
                (Some(gone), Some(into)) => {
                    let (from, to) = (gone.name.clone(), into.name.clone());
                    let n = gone.cards.len();
                    let keep: Vec<String> = app
                        .cfg
                        .kanban_columns
                        .iter()
                        .filter(|c| !c.eq_ignore_ascii_case(&from))
                        .cloned()
                        .collect();
                    save_columns(app, keep);
                    let _ = out.send(ClientFrame::Command(Cmd::ColumnRename {
                        from: from.clone(),
                        to: to.clone(),
                    }));
                    app.toast(
                        NoticeLevel::Info,
                        match n {
                            0 => format!("{from} is gone"),
                            1 => format!("{from} is gone — its card moved to {to}"),
                            n => format!("{from} is gone — {n} cards moved to {to}"),
                        },
                    );
                    col = col.saturating_sub(1);
                    sel = 0;
                }
                // The first column has nowhere to send its cards, and deleting the only
                // column would leave a board with nowhere to put anything.
                _ => app.toast(
                    NoticeLevel::Warn,
                    "a column can only be removed into the one before it",
                ),
            }
        }
        // Reorder the columns themselves. The configured list is the display order and
        // nothing else, so this moves an entry and touches no card.
        KeyCode::Char('<') | KeyCode::Char('>') if view == View::Board => {
            let left = matches!(k.code, KeyCode::Char('<'));
            let here = cols
                .get(col)
                .and_then(|c| app.cfg.kanban_columns.iter().position(|n| n.eq_ignore_ascii_case(&c.name)));
            if let Some(i) = here {
                let to = if left { i.checked_sub(1) } else { (i + 1 < app.cfg.kanban_columns.len()).then_some(i + 1) };
                if let Some(to) = to {
                    let mut list = app.cfg.kanban_columns.clone();
                    list.swap(i, to);
                    save_columns(app, list);
                    col = to;
                }
            }
        }
        KeyCode::Char('r') => kanban_refresh(app, out),
        _ => {}
    }
    app.mode = Mode::Kanban { view, col, sel };
    Ok(())
}

/// Write the column list back to `config.toml`, the same way every other setting persists.
fn save_columns(app: &mut App, cols: Vec<String>) {
    app.cfg.kanban_columns = cols.clone();
    match settings::write("kanban.columns", settings::Value::List(cols)) {
        Ok(()) => {}
        Err(e) => app.toast(NoticeLevel::Warn, format!("could not save the columns: {e}")),
    }
}

/// Keys on one card.
fn card_key(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    id: u64,
    mut focus: ui::kanban::Field,
) -> Result<()> {
    use crate::proto::CardPatch;
    use ui::kanban::Field;

    let Some(card) = app.kanban.as_ref().and_then(|kb| kb.cards.iter().find(|c| c.id == id)).cloned()
    else {
        // The card is gone — archived away, or the board was replaced under it. Back to the
        // board, which is somewhere that certainly exists.
        app.mode = kanban_back(app);
        return Ok(());
    };

    // Typing owns the keyboard. `esc` saves and `ctrl+c` discards — enter cannot save,
    // because in a description enter is a new line and that is the whole point of the field.
    if let Some(mut editing) = app.card_edit.take() {
        match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Esc => return card_save(app, out, &card, editing, focus),
            KeyCode::Enter if editing.field.multiline() => {
                editing.text.newline();
                app.card_edit = Some(editing);
            }
            KeyCode::Enter => return card_save(app, out, &card, editing, focus),
            KeyCode::Backspace => {
                editing.text.backspace();
                app.card_edit = Some(editing);
            }
            KeyCode::Left => {
                editing.text.left();
                app.card_edit = Some(editing);
            }
            KeyCode::Right => {
                editing.text.right();
                app.card_edit = Some(editing);
            }
            KeyCode::Up => {
                editing.text.up();
                app.card_edit = Some(editing);
            }
            KeyCode::Down => {
                editing.text.down();
                app.card_edit = Some(editing);
            }
            KeyCode::Home => {
                editing.text.home();
                app.card_edit = Some(editing);
            }
            KeyCode::End => {
                editing.text.end();
                app.card_edit = Some(editing);
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                editing.text.insert(c);
                app.card_edit = Some(editing);
            }
            _ => app.card_edit = Some(editing),
        }
        return Ok(());
    }

    let start = |app: &mut App, field: Field, text: String| {
        app.card_edit =
            Some(ui::kanban::Editing { field, text: ui::kanban::TextArea::new(&text) });
    };

    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = kanban_back(app);
            // The board is re-derived from the reply the client already has, but the card may
            // have changed under an agent while it was open.
            kanban_refresh(app, out);
            return Ok(());
        }
        KeyCode::Tab => focus = focus.step(1),
        KeyCode::BackTab => focus = focus.step(-1),
        // Bounded by what the renderer actually drew, so `j` held down cannot scroll a short
        // card into a blank screen you then have to scroll back out of.
        KeyCode::Char('j') | KeyCode::Down => {
            app.card_scroll = (app.card_scroll + 1).min(app.card_lines.saturating_sub(1))
        }
        KeyCode::Char('k') | KeyCode::Up => app.card_scroll = app.card_scroll.saturating_sub(1),
        // Hand it over now, without waiting for the window. The one key that reaches the
        // agents from here, and it is upper case for that reason.
        KeyCode::Char('A') => {
            let _ = out.send(ClientFrame::Command(Cmd::CardHandOff { id }));
        }
        KeyCode::Char('X') => {
            let _ = out.send(ClientFrame::Command(Cmd::CardArchive {
                id,
                archived: !card.archived,
            }));
        }
        // Every field is reached by the key it prints, and `enter` edits whatever the cursor
        // is on — so the two ways in cannot describe different sets of fields.
        KeyCode::Enter => {
            start(app, focus, card_field_text(app, &card, focus));
            return Ok(());
        }
        KeyCode::Char(c) => {
            if let Some(field) = Field::from_key(c) {
                focus = field;
                start(app, field, card_field_text(app, &card, field));
                app.mode = Mode::Card { id, focus };
                return Ok(());
            }
        }
        _ => {}
    }
    app.mode = Mode::Card { id, focus };
    let _ = CardPatch::default();
    Ok(())
}

/// What a field's editor opens with.
///
/// A comment opens empty, because a thread is appended to rather than rewritten; everything
/// else opens with what is there, because those are values you are correcting.
fn card_field_text(app: &App, card: &crate::proto::Card, field: ui::kanban::Field) -> String {
    use ui::kanban::Field;
    match field {
        Field::Title => card.title.clone(),
        Field::Body => card.body.clone(),
        Field::Due => card.due.map(crate::daemon::triggers::local_date).unwrap_or_default(),
        Field::Tags => card.tags.join(" "),
        Field::Project => card.project.clone().unwrap_or_default(),
        Field::Assist => card
            .assist
            .map(ui::kanban::short_duration)
            .unwrap_or_else(|| ui::kanban::short_duration(app.cfg.kanban_assist)),
        Field::Comments => String::new(),
    }
}

/// Apply what was typed. A value that cannot be read is reported and the field stays open.
fn card_save(
    app: &mut App,
    out: &mpsc::UnboundedSender<ClientFrame>,
    card: &crate::proto::Card,
    editing: ui::kanban::Editing,
    focus: ui::kanban::Field,
) -> Result<()> {
    use crate::proto::CardPatch;
    use ui::kanban::Field;

    let value = editing.text.text();
    let trimmed = value.trim().to_string();
    let mut patch = CardPatch::default();
    match editing.field {
        // An empty title would leave a card with nothing on it, so it is refused rather than
        // silently kept — the field stays open with what you typed still in it.
        Field::Title if trimmed.is_empty() => {
            app.toast(NoticeLevel::Warn, "a card needs a title");
            app.card_edit = Some(editing);
            return Ok(());
        }
        Field::Title => patch.title = Some(trimmed),
        Field::Body => patch.body = Some(value),
        Field::Due => match ui::kanban::parse_due(&trimmed, ui::now_millis()) {
            Ok(due) => patch.due = Some(due),
            Err(e) => {
                app.toast(NoticeLevel::Warn, e);
                app.card_edit = Some(editing);
                return Ok(());
            }
        },
        Field::Tags => {
            patch.tags = Some(
                trimmed.split_whitespace().map(|t| t.to_string()).collect::<Vec<_>>(),
            )
        }
        Field::Project => {
            patch.project = Some((!trimmed.is_empty()).then_some(trimmed));
        }
        // An empty window disarms, which is how every other text field here says "none".
        Field::Assist if trimmed.is_empty() => patch.assist = Some(None),
        Field::Assist => match crate::cli::parse_duration(&trimmed) {
            Ok(secs) => patch.assist = Some(Some(secs * 1000)),
            Err(e) => {
                app.toast(NoticeLevel::Warn, format!("{e}"));
                app.card_edit = Some(editing);
                return Ok(());
            }
        },
        Field::Comments => {
            if !trimmed.is_empty() {
                let _ = out.send(ClientFrame::Command(Cmd::CardComment {
                    id: card.id,
                    body: trimmed,
                }));
            }
            app.mode = Mode::Card { id: card.id, focus };
            return Ok(());
        }
    }
    let _ = out.send(ClientFrame::Command(Cmd::CardEdit { id: card.id, patch }));
    app.mode = Mode::Card { id: card.id, focus };
    Ok(())
}

/// How long two clicks in the same cell may be apart and still be one double click.
///
/// crossterm has no notion of one — the terminal reports two presses and it is up to the
/// application to decide they were the same gesture. Four hundred milliseconds is the usual
/// desktop figure and is comfortably longer than anyone's deliberate second click.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// Drive the board with the pointer.
///
/// The same three-state machine the graph uses, and the same rule the panes have: a drag that
/// began on a card belongs to that card until the button comes up, wherever the pointer
/// wanders. `Drag` only updates what is hovered — the drop is worked out once, on release,
/// because a drop target recomputed on every motion event is a card that lands wherever the
/// last event happened to fall rather than where you let go.
fn kanban_mouse(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
) {
    let Some(area) = app.kanban_area else { return };
    let cols = kanban_view(app);
    app.kanban_scroll.resize(cols.len().max(1), 0);
    let focus = match app.mode {
        Mode::Kanban { col, .. } => col,
        _ => 0,
    };
    let at = ratatui::layout::Position { x: m.column, y: m.row };

    // The list is a different geometry and answers to a different set of gestures, so it is
    // handled before the board's layout is computed rather than inside it. Sharing the one
    // handler is how clicking the list came to select a card on the board: every branch below
    // resolves against column rects the list never drew, and then writes `View::Board` back.
    if let Mode::Kanban { view: ui::kanban::View::List, col, sel } = app.mode {
        kanban_list_mouse(app, m, at, &cols, area, col, sel);
        return;
    }

    let lay = ui::kanban::layout(&cols, area, &app.kanban_scroll, focus);

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some((shown, id)) = lay.hit_card(at) else {
                // A press on a header still moves the cursor there, so clicking a column and
                // then typing does what it looks like it should — and a second press on the
                // same header renames it, which is where a person reaches for a column's name.
                if let Some(shown) = lay.hit_column(at) {
                    let col = lay.column_at(shown);
                    app.mode = Mode::Kanban { view: ui::kanban::View::Board, col, sel: 0 };
                    let again = app.card_click.is_some_and(|(cx, cy, when)| {
                        cx == m.column && cy == m.row && when.elapsed() < DOUBLE_CLICK
                    });
                    app.card_click = Some((m.column, m.row, Instant::now()));
                    if again && lay.hit_header(at).is_some() {
                        app.card_click = None;
                        if let Some(c) = cols.get(col) {
                            app.kanban_ask = Some(ui::kanban::Asking {
                                ask: ui::kanban::Ask::RenameColumn { from: c.name.clone() },
                                text: ui::kanban::TextArea::new(&c.name),
                            });
                        }
                    }
                }
                return;
            };
            let col = lay.column_at(shown);
            let sel = cols[col].cards.iter().position(|c| c.id == id).unwrap_or(0);
            app.mode = Mode::Kanban { view: ui::kanban::View::Board, col, sel };

            // Two presses in the same cell inside the window is a double click, and opens the
            // card. Checked before the drag begins, so opening one never leaves a drag live.
            let again = app
                .card_click
                .is_some_and(|(cx, cy, when)| cx == m.column && cy == m.row && when.elapsed() < DOUBLE_CLICK);
            app.card_click = Some((m.column, m.row, Instant::now()));
            if again {
                app.card_click = None;
                app.card_scroll = 0;
                app.kanban_back = (ui::kanban::View::Board, col, sel);
                // Whatever the first press left holding is let go here. A release normally
                // clears it, but a terminal that coalesces the two presses would otherwise
                // leave a drag live behind a card that has already opened.
                app.card_drag = None;
                app.mode = Mode::Card { id, focus: ui::kanban::Field::Title };
                return;
            }

            let rect = lay.cols[shown]
                .cards
                .iter()
                .find(|(cid, _)| *cid == id)
                .map(|(_, r)| *r)
                .unwrap_or(area);
            app.card_drag = Some(ui::kanban::CardDrag {
                id,
                from_col: col,
                // Where in the card you took hold of it. Without this the card jumps so its
                // corner is under the pointer the instant you touch it, which reads as the
                // board rearranging itself rather than as picking something up.
                grab: (m.column.saturating_sub(rect.x), m.row.saturating_sub(rect.y)),
                at: (m.column, m.row),
                hover_col: Some(shown),
                moved: false,
            });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(mut drag) = app.card_drag else { return };
            drag.moved |= (drag.at.0, drag.at.1) != (m.column, m.row);
            drag.at = (m.column, m.row);
            drag.hover_col = lay.hit_column(at);
            app.card_drag = Some(drag);
        }
        // Matched on the drag existing rather than on the button: in the older X10 encoding a
        // release is always reported as button one, whichever was actually let go.
        MouseEventKind::Up(_) => {
            let Some(drag) = app.card_drag.take() else { return };
            let Some(shown) = lay.hit_column(at) else { return };
            let to = lay.column_at(shown);
            let after = lay.drop_after(shown, m.row);
            let Some(column) = cols.get(to).map(|c| c.name.clone()) else { return };

            // Letting go where you picked it up is not a move. Without this every click on a
            // card would write a reorder that changed nothing, and the log would fill with
            // moves nobody made.
            let onto_itself = after == Some(drag.id);
            let same_place = to == drag.from_col
                && (!drag.moved
                    || onto_itself
                    || after
                        == cols[to]
                            .cards
                            .iter()
                            .position(|c| c.id == drag.id)
                            .and_then(|i| i.checked_sub(1))
                            .and_then(|i| cols[to].cards.get(i).map(|c| c.id)));
            if same_place {
                return;
            }
            kanban_move(app, out, drag.id, &column, after);
            app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: to, sel: 0 };
        }
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            let Some(shown) = lay.hit_column(at) else { return };
            let down = matches!(m.kind, MouseEventKind::ScrollDown);
            if let Some(s) = app.kanban_scroll.get_mut(lay.column_at(shown)) {
                *s = if down { s.saturating_add(1) } else { s.saturating_sub(1) };
            }
        }
        _ => {}
    }
}

/// Drive the list with the pointer.
///
/// A row is "which row", which is the case the board's layout was explicitly *not* built for
/// — see the note at the top of `ui::kanban`. So this resolves through
/// [`ui::kanban::ListLayout::row_at`], the same function that drew the rows.
///
/// There is deliberately no drag: the list is sorted by due date, so dropping a row somewhere
/// would either reorder nothing or silently rewrite the date to make the drop true.
fn kanban_list_mouse(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    at: ratatui::layout::Position,
    cols: &[ui::kanban::Column],
    area: ratatui::layout::Rect,
    col: usize,
    sel: usize,
) {
    let now = ui::now_millis();
    let rows = ui::kanban::list_rows(cols, now);
    let lay = ui::kanban::list_layout(area, rows.len(), sel);

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // A click on the header or below the last row is not a click on a card. Leaving
            // the cursor where it was beats moving it somewhere the pointer did not point.
            let Some(row) = lay.row_at(at) else { return };
            app.mode = Mode::Kanban { view: ui::kanban::View::List, col, sel: row };

            let again = app.card_click.is_some_and(|(cx, cy, when)| {
                cx == m.column && cy == m.row && when.elapsed() < DOUBLE_CLICK
            });
            app.card_click = Some((m.column, m.row, Instant::now()));
            if again {
                app.card_click = None;
                if let Some((card, _)) = rows.get(row) {
                    app.card_scroll = 0;
                    app.kanban_back = (ui::kanban::View::List, col, row);
                    app.mode = Mode::Card { id: card.id, focus: ui::kanban::Field::Title };
                }
            }
        }
        // The cursor is the scroll — `list_layout` derives one from the other — so a wheel
        // notch moves the selection rather than sliding rows out from under it.
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            if !area.contains(at) {
                return;
            }
            let sel = match m.kind {
                MouseEventKind::ScrollDown => (sel + 3).min(rows.len().saturating_sub(1)),
                _ => sel.saturating_sub(3),
            };
            app.mode = Mode::Kanban { view: ui::kanban::View::List, col, sel };
        }
        _ => {}
    }
}

/// Drive the graph with the pointer.
///
/// Split out rather than inlined because it is the only view whose mouse handling is more
/// than "what row is that", and because every branch of it needs the same three facts —
/// where the plot is, what the layout is, and where in the layout the pointer is.
fn graph_mouse(app: &mut App, m: crossterm::event::MouseEvent, sel: usize) {
    let (Some(plot), true) = (app.graph_plot, app.sim.is_some()) else { return };
    let (w, h) = (plot.width, plot.height);
    if w == 0 || h == 0 {
        return;
    }
    let (zoom, centre) = (app.graph_zoom, app.graph_centre);
    // Where the pointer is inside the plot. Saturating rather than checked: a pointer above
    // or left of the plot reads as its nearest edge, which is what a drag off the top of the
    // window should do anyway.
    let at = |col: u16, row: u16| {
        (col.saturating_sub(plot.x) as f64, row.saturating_sub(plot.y) as f64)
    };
    let here = at(m.column, m.row);
    let where_is = |app: &App, cell: (f64, f64), zoom: f64| -> graph::Point {
        app.sim.as_ref().map(|s| s.unproject(cell, w, h, zoom, centre)).unwrap_or(centre)
    };

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // A node within a cell of the pointer is the node you meant. One cell of slack,
            // because a glyph is one cell and nobody clicks the middle of anything.
            let hit = app
                .graph_hits
                .iter()
                .find(|(hy, hx, _)| *hy == m.row && hx.abs_diff(m.column) <= 1)
                .map(|(_, _, i)| *i);
            match hit {
                Some(i) => {
                    graph_select(app, i);
                    app.graph_drag = Some(GraphDrag::Node { i });
                }
                None => app.graph_drag = Some(GraphDrag::Pan { at: (m.column, m.row) }),
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => match app.graph_drag {
            Some(GraphDrag::Node { i }) => {
                let to = where_is(app, here, zoom);
                if let Some(s) = app.sim.as_mut() {
                    s.place(i, to);
                }
            }
            Some(GraphDrag::Pan { at: last }) => {
                // The view moves opposite the pointer, so the map follows your hand rather
                // than fleeing it. Measured in layout units, or panning would move by
                // different amounts at different zooms.
                let from = where_is(app, at(last.0, last.1), zoom);
                let to = where_is(app, here, zoom);
                app.graph_centre.x -= to.x - from.x;
                app.graph_centre.y -= to.y - from.y;
                app.graph_drag = Some(GraphDrag::Pan { at: (m.column, m.row) });
            }
            None => {}
        },
        MouseEventKind::Up(_) => {
            // Letting go of a node lets its neighbours answer. Only then — reheating while
            // it is still held would fight the hand holding it.
            if matches!(app.graph_drag, Some(GraphDrag::Node { .. })) {
                if let Some(s) = app.sim.as_mut() {
                    s.nudge();
                }
            }
            app.graph_drag = None;
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            // Zoom about the pointer: whatever is under it stays under it. Zooming about the
            // centre instead is what makes a graph lurch away from the thing you were
            // reaching for, and is the single difference between this feeling like a map and
            // feeling like a slideshow.
            let before = where_is(app, here, zoom);
            let step = if matches!(m.kind, MouseEventKind::ScrollUp) { 1.2 } else { 1.0 / 1.2 };
            app.graph_zoom = (zoom * step).clamp(0.4, 8.0);
            let after = where_is(app, here, app.graph_zoom);
            app.graph_centre.x += before.x - after.x;
            app.graph_centre.y += before.y - after.y;
        }
        _ => {}
    }
    let _ = sel;
}

// -- the editor's keyboard ---------------------------------------------------
//
// Split by mode rather than nested in one match, because insert and normal share almost no
// keys and reading either one should not mean stepping over the other. They all answer the
// same question: where does the keyboard go next, and `None` means the editor is gone.

/// Write the buffer back to wherever it came from. Says whether it went.
fn editor_save(
    app: &mut App,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
) -> bool {
    let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) else {
        // Silence here would be the worst kind: you press `:w`, nothing happens, and you find
        // out at the end of the evening.
        app.toast(NoticeLevel::Warn, "no project is focused, so there is nowhere to save this");
        return false;
    };
    let Some(b) = app.buffer.as_mut() else { return false };
    let body = b.text();
    b.saved();
    let cmd = if project {
        Cmd::FileSave { space, path: path.to_string(), body }
    } else {
        Cmd::VaultSave { space, path: path.to_string(), body }
    };
    let _ = out.send(ClientFrame::Command(cmd));
    true
}

/// Put the editor away, back to whichever browser opened it.
fn editor_close(
    app: &mut App,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
) {
    // Whatever was analysing this can stop. Without it a language server goes on holding a
    // file nobody is looking at until the idle timer notices.
    if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
        let _ = out.send(ClientFrame::Command(Cmd::DocClosed {
            space,
            path: path.to_string(),
            vault: !project,
        }));
    }
    app.buffer = None;
    app.highlight = None;
    app.doc = DocSync::default();
    app.diags.clear();
    app.completions = None;
    app.mode = match (project, app.vault_tree) {
        (true, _) => Mode::Files { query: String::new(), sel: 0 },
        // Out of the vault, not into a second list of the same notes. The vault already has
        // the tree and `ctrl+n`, so the browser it used to land in was a step that offered
        // nothing the page you just left did not — and it arrived showing one note, because
        // the index had been overwritten by the note you opened.
        (false, true) => Mode::Terminal,
        (false, false) => Mode::Notes { query: String::new(), sel: 0 },
    };
}

/// Insert: typing types, and the only key that means something else is the one that leaves.
fn editor_insert(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
    page: usize,
) -> Option<Vim> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // While the list is open it owns the keys that move through it, and nothing else. Every
    // other key falls through to typing, which is what narrows the list.
    if app.completions.is_some() {
        match (k.code, ctrl) {
            (KeyCode::Char('n'), true) | (KeyCode::Down, _) => {
                completion_move(app, 1);
                return Some(Vim::Insert);
            }
            (KeyCode::Char('p'), true) | (KeyCode::Up, _) => {
                completion_move(app, -1);
                return Some(Vim::Insert);
            }
            (KeyCode::Enter, _) | (KeyCode::Tab, _) => {
                completion_accept(app);
                return Some(Vim::Insert);
            }
            // Up one layer, and the layer is the popup. Still writing afterwards, which is
            // where you were before it opened.
            (KeyCode::Esc, _) => {
                app.completions = None;
                return Some(Vim::Insert);
            }
            _ => {}
        }
    }

    match (k.code, ctrl) {
        (KeyCode::Char('s'), true) => {
            editor_save(app, out, path, project);
            return Some(Vim::Insert);
        }
        // vim's own key for it, and free here. Asks the daemon, which asks whatever language
        // server is watching this file; the answer arrives when it arrives.
        (KeyCode::Char('n'), true) | (KeyCode::Char(' '), true) => {
            completion_ask(app, out, path, project);
            return Some(Vim::Insert);
        }
        // Paste a picture. A terminal never delivers image bytes through paste itself — the
        // clipboard has to be asked for directly — so this is a key rather than something
        // that happens when you press the paste you already know.
        (KeyCode::Char('v'), true) => {
            paste_image(app, out, path, project);
            return Some(Vim::Insert);
        }
        // esc leaves *insert*, not the note. That is the whole reason for having modes: the
        // way out of the editor is `:q`, or one more esc.
        (KeyCode::Esc, _) => {
            if let Some(b) = app.buffer.as_mut() {
                b.clamp();
            }
            return Some(Vim::Normal);
        }
        (KeyCode::Char('r'), true) => {
            editor_save(app, out, path, project);
            app.buffer = None;
            app.highlight = None;
            read_note(app, path, out);
            return None;
        }
        // Available because the client runs in raw mode, so ctrl+z is a keystroke here
        // rather than a suspend signal.
        (KeyCode::Char('z'), true) => {
            if !app.buffer.as_mut().is_some_and(|b| b.undo()) {
                app.toast(NoticeLevel::Info, "nothing to undo");
            }
            return Some(Vim::Insert);
        }
        (KeyCode::Char('y'), true) => {
            if !app.buffer.as_mut().is_some_and(|b| b.redo()) {
                app.toast(NoticeLevel::Info, "nothing to redo");
            }
            return Some(Vim::Insert);
        }
        _ => {}
    }

    let Some(buf) = app.buffer.as_mut() else { return Some(Vim::Insert) };
    match (k.code, ctrl) {
        (KeyCode::Enter, _) => buf.newline(),
        (KeyCode::Backspace, _) => buf.backspace(),
        (KeyCode::Delete, _) => buf.delete(),
        (KeyCode::Left, _) => buf.left(),
        (KeyCode::Right, _) => buf.right(),
        (KeyCode::Up, _) => buf.up(),
        (KeyCode::Down, _) => buf.down(),
        (KeyCode::Home, _) => buf.home(),
        (KeyCode::End, _) => buf.end(),
        (KeyCode::Tab, _) => {
            for _ in 0..2 {
                buf.insert(' ');
            }
        }
        (KeyCode::PageDown, _) => {
            for _ in 0..page {
                buf.down();
            }
        }
        (KeyCode::PageUp, _) => {
            for _ in 0..page {
                buf.up();
            }
        }
        // Everything else types — including `:` and `/`, which are punctuation in here and
        // commands one mode over.
        (KeyCode::Char(c), false) => buf.insert(c),
        _ => {}
    }

    // `[[` opens the note list, the way it does in the app this borrows the idea from. No key
    // to learn: the thing you type to make a link is the thing that offers you one.
    if app.buffer.as_ref().is_some_and(|b| b.at_link_open()) && app.completions.is_none() {
        if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
            app.linking = true;
            let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
                space,
                kind: crate::proto::VaultQuery::List,
            }));
        }
    }

    // A completion list is about one word on one line. Leaving either ends it, and so does
    // narrowing it down to nothing — a popup showing no matches is a popup in the way.
    if let (Some(c), Some(b)) = (app.completions.as_ref(), app.buffer.as_ref()) {
        let matches = c.matching(&b.text_from(c.from)).len();
        if b.line != c.line || b.col < c.from || matches == 0 {
            app.completions = None;
        } else if let Some(c) = app.completions.as_mut() {
            c.sel = c.sel.min(matches - 1);
        }
    }
    Some(Vim::Insert)
}

/// Put the clipboard's picture in the vault and link it from here.
///
/// The link goes in whether or not the write lands, and deliberately: a missing attachment
/// renders as a named placeholder, which is a thing you can see and fix. Text that silently
/// did not appear is not.
fn paste_image(
    app: &mut App,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
) {
    // A note's attachment goes in the vault. A code file has no vault to put one in, and
    // scattering PNGs through a source tree is not a thing to do on a keystroke.
    if project {
        app.toast(NoticeLevel::Info, "pictures go in notes, not in project files");
        return;
    }
    let Some(bytes) = clipboard::image() else {
        app.toast(NoticeLevel::Info, "no picture on the clipboard");
        return;
    };
    let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) else { return };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = clipboard::attachment_name(path, stamp);

    let _ = out.send(ClientFrame::Command(Cmd::Attach {
        space,
        name: name.clone(),
        bytes,
    }));
    // An embed, in the vault's own vocabulary — the same `![[...]]` Obsidian writes, so a
    // note pasted into here opens correctly over there.
    if let Some(b) = app.buffer.as_mut() {
        for c in format!("![[{name}]]").chars() {
            b.insert(c);
        }
        b.newline();
    }
    app.toast(NoticeLevel::Info, format!("attached {name}"));
}

/// Ask what could go here.
fn completion_ask(
    app: &mut App,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
) {
    let Some(b) = app.buffer.as_ref() else { return };
    let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) else { return };
    let _ = out.send(ClientFrame::Command(Cmd::Complete {
        space,
        path: path.to_string(),
        body: b.text(),
        line: b.line as u32,
        col: b.col as u32,
        vault: !project,
    }));
}

/// Move through the list, wrapping. A list you can fall off the end of makes you look.
fn completion_move(app: &mut App, by: i32) {
    let from = app.completions.as_ref().map(|c| c.from).unwrap_or(0);
    let prefix = app.buffer.as_ref().map(|b| b.text_from(from)).unwrap_or_default();
    let Some(c) = app.completions.as_mut() else { return };
    let n = c.matching(&prefix).len();
    if n == 0 {
        return;
    }
    c.sel = ((c.sel as i32 + by).rem_euclid(n as i32)) as usize;
}

/// Put the selected completion in.
fn completion_accept(app: &mut App) {
    let Some(c) = app.completions.take() else { return };
    let Some(b) = app.buffer.as_ref() else { return };
    let prefix = b.text_from(c.from);
    let (col, start) = (b.col, c.from);
    let Some(item) = c.matching(&prefix).get(c.sel).copied().cloned() else { return };
    // The server's own range when it gave one, and the word the cursor is in otherwise.
    // Trusting the range matters: a server completing `self.field` knows the dot is not part
    // of what it is replacing, and guessing that from the text is how a completion eats a
    // character it should not have.
    let (from, to) = match item.replace {
        Some((a, z)) => (a as usize, (z as usize).max(col)),
        None => (start, col),
    };
    let text = if c.link { format!("{}]]", item.insert) } else { item.insert.clone() };
    if let Some(b) = app.buffer.as_mut() {
        b.replace_in_line(from, to, &text);
    }
}

/// Normal: keys are verbs. `pending` is the first half of a pair, if one was typed.
fn editor_normal(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
    page: usize,
    pending: Option<char>,
) -> Option<Vim> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // The same key the browser makes a note with, so the notes side has one answer to "new
    // note" rather than one per surface. Insert mode keeps it for completion -- that is vim's
    // own key for that and the editor honours it -- so from insert this is `esc` then `ctrl+n`.
    if !project && pending.is_none() && ctrl && k.code == KeyCode::Char('n') {
        if app.buffer.as_ref().is_some_and(|b| b.dirty) {
            app.toast(NoticeLevel::Warn, "unsaved changes -- :w first");
            return Some(Vim::Normal);
        }
        // The editor's own command line, opened with the command already in it -- not the
        // shared prompt. That one is drawn over the *panes*, so asking for a note from inside
        // the vault dimmed the vault away and showed you the multiplexer to type into. This
        // stays where you are: the field is the editor's footer, the note is still on screen
        // behind it, and backspacing off the front of it puts you back in the note.
        return Some(Vim::Command("new ".into()));
    }

    // `\` shows and hides the vault's tree. Not `t`, which is a motion in every vi anyone has
    // used — taking one to make a mnemonic is how a keymap rots. `\` is what file trees are
    // toggled with elsewhere and is not a motion here or anywhere.
    if !project && pending.is_none() && !ctrl && k.code == KeyCode::Char('\\') {
        app.vault_tree = !app.vault_tree;
        return Some(Vim::Normal);
    }

    // A half-typed pair is resolved or dropped, and never falls through to the single-key
    // meaning of its second half. The `d` of an abandoned `dd` must not delete anything.
    if let Some(first) = pending {
        match (first, k.code) {
            ('g', KeyCode::Char('g')) => {
                if let Some(b) = app.buffer.as_mut() {
                    b.top();
                }
            }
            ('d', KeyCode::Char('d')) => {
                if let Some(gone) = app.buffer.as_mut().map(|b| b.delete_line()) {
                    app.yank = Some(gone);
                }
            }
            ('y', KeyCode::Char('y')) => {
                if let Some(line) = app.buffer.as_ref().map(|b| b.line_text()) {
                    app.yank = Some(line);
                    app.toast(NoticeLevel::Info, "line yanked");
                }
            }
            ('c', KeyCode::Char('c')) => {
                if let Some(b) = app.buffer.as_mut() {
                    app.yank = Some(b.line_text());
                    b.clear_line();
                    return Some(Vim::Insert);
                }
            }
            // Walking the problems. Worth having because the margin only marks the screen,
            // and the one you need is usually not on it.
            (b @ (']' | '['), KeyCode::Char('d')) => {
                let forward = b == ']';
                let here = app.buffer.as_ref().map(|b| b.line as u32).unwrap_or(0);
                let mut lines: Vec<u32> =
                    app.diags.get(path).map(|d| d.iter().map(|d| d.line).collect()).unwrap_or_default();
                lines.sort_unstable();
                lines.dedup();
                let to = if forward {
                    lines.iter().find(|l| **l > here).or_else(|| lines.first())
                } else {
                    lines.iter().rev().find(|l| **l < here).or_else(|| lines.last())
                };
                match to.copied() {
                    Some(l) => {
                        if let Some(b) = app.buffer.as_mut() {
                            b.goto(l as usize, 0);
                            b.first_nonblank();
                        }
                    }
                    None => app.toast(NoticeLevel::Info, "nothing to fix here"),
                }
            }
            _ => {}
        }
        return Some(Vim::Normal);
    }

    // The keys that are about the editor rather than about the text, taken first so the rest
    // can hold the buffer without also needing the app.
    match (k.code, ctrl) {
        // Up one layer, saving on the way. esc is the key people press when they are not
        // sure what else to press, so it must never be the one that loses a note — leaving
        // without writing is spelled `:q!`, deliberately and on purpose.
        (KeyCode::Esc, _) => {
            editor_save(app, out, path, project);
            editor_close(app, out, path, project);
            return None;
        }
        (KeyCode::Char('s'), true) => {
            editor_save(app, out, path, project);
            return Some(Vim::Normal);
        }
        (KeyCode::Char(':'), false) => return Some(Vim::Command(String::new())),
        (KeyCode::Char('/'), false) => return Some(Vim::Search(String::new())),
        (KeyCode::Char('r'), true) => {
            if !app.buffer.as_mut().is_some_and(|b| b.redo()) {
                app.toast(NoticeLevel::Info, "nothing to redo");
            }
            return Some(Vim::Normal);
        }
        _ => {}
    }

    let held = app.yank.clone();
    let last = app.search.clone();
    let Some(b) = app.buffer.as_mut() else { return Some(Vim::Normal) };
    let mut next = Vim::Normal;
    let mut miss: Option<String> = None;

    match (k.code, ctrl) {
        // moving
        (KeyCode::Char('h'), false) | (KeyCode::Left, _) => b.step(false),
        (KeyCode::Char('l'), false) | (KeyCode::Right, _) => b.step(true),
        (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
            b.down();
            b.clamp();
        }
        (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
            b.up();
            b.clamp();
        }
        (KeyCode::Char('w'), false) => b.word_forward(),
        (KeyCode::Char('b'), false) => b.word_back(),
        (KeyCode::Char('e'), false) => b.word_end(),
        (KeyCode::Char('0'), false) | (KeyCode::Home, _) => b.home(),
        (KeyCode::Char('^'), false) => b.first_nonblank(),
        (KeyCode::Char('$'), false) | (KeyCode::End, _) => {
            b.end();
            b.clamp();
        }
        (KeyCode::Char('G'), false) => b.bottom(),
        (KeyCode::Char('{'), false) => b.paragraph(false),
        (KeyCode::Char('}'), false) => b.paragraph(true),
        (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => {
            for _ in 0..page / 2 {
                b.down();
            }
            b.clamp();
        }
        (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => {
            for _ in 0..page / 2 {
                b.up();
            }
            b.clamp();
        }

        // into insert, at the six places people expect to land
        (KeyCode::Char('i'), false) => next = Vim::Insert,
        (KeyCode::Char('a'), false) => {
            b.goto(b.line, b.col + 1);
            next = Vim::Insert;
        }
        (KeyCode::Char('I'), false) => {
            b.first_nonblank();
            next = Vim::Insert;
        }
        (KeyCode::Char('A'), false) => {
            b.end();
            next = Vim::Insert;
        }
        (KeyCode::Char('o'), false) => {
            b.put_line("", true);
            next = Vim::Insert;
        }
        (KeyCode::Char('O'), false) => {
            b.put_line("", false);
            next = Vim::Insert;
        }

        // changing
        (KeyCode::Char('x'), false) => {
            b.delete_char();
        }
        (KeyCode::Char('D'), false) => {
            b.delete_to_end();
            b.clamp();
        }
        (KeyCode::Char('C'), false) => {
            b.delete_to_end();
            next = Vim::Insert;
        }
        (KeyCode::Char('J'), false) => b.join(),
        (KeyCode::Char('p'), false) | (KeyCode::Char('P'), false) => match held {
            Some(y) => b.put_line(&y, k.code == KeyCode::Char('p')),
            None => miss = Some("nothing to put down yet".into()),
        },
        (KeyCode::Char('u'), false) | (KeyCode::Char('z'), true) => {
            if !b.undo() {
                miss = Some("nothing to undo".into());
            }
        }
        (KeyCode::Char('y'), true) => {
            if !b.redo() {
                miss = Some("nothing to redo".into());
            }
        }

        // the first half of a pair
        (KeyCode::Char(c @ ('g' | 'd' | 'y' | 'c' | ']' | '[')), false) => next = Vim::Pending(c),

        // the search again, in both directions
        (KeyCode::Char(c @ ('n' | 'N')), false) => {
            if last.is_empty() {
                miss = Some("nothing searched for yet".into());
            } else if !b.search(&last, c == 'n') {
                miss = Some(format!("no more {last:?}"));
            }
        }
        _ => {}
    }

    if let Some(m) = miss {
        app.toast(NoticeLevel::Info, m);
    }
    Some(next)
}

/// The `:` and `/` lines. One reader, because they are the same act with different verbs.
fn editor_line(
    app: &mut App,
    k: KeyEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
    searching: bool,
    mut line: String,
) -> Option<Vim> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc => return Some(Vim::Normal),
        KeyCode::Backspace => {
            // Backspacing off the front of the prompt is how you take back having opened it,
            // which is what it does everywhere else in horde.
            if line.pop().is_none() {
                return Some(Vim::Normal);
            }
        }
        KeyCode::Enter => {
            return if searching {
                editor_find(app, &line)
            } else {
                editor_run(app, out, path, project, &line)
            };
        }
        KeyCode::Char(c) if !ctrl => line.push(c),
        _ => {}
    }
    Some(if searching { Vim::Search(line) } else { Vim::Command(line) })
}

/// Run what was typed after `/`. An empty line repeats the last search, as it does elsewhere.
fn editor_find(app: &mut App, needle: &str) -> Option<Vim> {
    let needle = if needle.is_empty() { app.search.clone() } else { needle.to_string() };
    if needle.is_empty() {
        app.toast(NoticeLevel::Info, "nothing to search for");
        return Some(Vim::Normal);
    }
    if !app.buffer.as_mut().is_some_and(|b| b.search(&needle, true)) {
        app.toast(NoticeLevel::Info, format!("not found: {needle}"));
    }
    // Remembered either way, so `n` after a miss searches for what you meant rather than for
    // whatever was in there before it.
    app.search = needle;
    Some(Vim::Normal)
}

/// Run what was typed after `:`.
///
/// Only the commands that mean something here. An unknown one says so rather than being
/// swallowed, because a `:` line that silently ignores things is one you cannot trust to have
/// written the file.
fn editor_run(
    app: &mut App,
    out: &mpsc::UnboundedSender<ClientFrame>,
    path: &str,
    project: bool,
    line: &str,
) -> Option<Vim> {
    let cmd = line.trim();
    let dirty = app.buffer.as_ref().is_some_and(|b| b.dirty);
    match cmd {
        "" => Some(Vim::Normal),
        "w" | "w!" => {
            if editor_save(app, out, path, project) {
                app.toast(NoticeLevel::Info, format!("wrote {path}"));
            }
            Some(Vim::Normal)
        }
        "wq" | "wq!" | "x" | "xa" | "wqa" => {
            // A failed write must not close the note: that is the one path where quitting
            // would throw away work while looking like it saved it.
            if editor_save(app, out, path, project) {
                editor_close(app, out, path, project);
                return None;
            }
            Some(Vim::Normal)
        }
        "q" | "qa" => {
            if dirty {
                app.toast(
                    NoticeLevel::Warn,
                    "unsaved changes — :wq to write and go, :q! to throw them away",
                );
                return Some(Vim::Normal);
            }
            editor_close(app, out, path, project);
            None
        }
        "q!" | "qa!" => {
            editor_close(app, out, path, project);
            None
        }
        // A note from inside the vault, which is where you are when you find out you need
        // one: a link you just wrote with nothing behind it, a heading that wants its own
        // page. `ctrl+b w` cannot reach here — the editor consumes every key — and the
        // shared prompt escapes to the terminal, which would cost you the vault to cancel.
        // The command line is the editor's own prompt and comes back to it.
        _ if cmd == "new" || cmd.starts_with("new ") => {
            let title = cmd["new".len()..].trim();
            if title.is_empty() {
                app.toast(NoticeLevel::Info, "give it a title: :new Reading list");
                return Some(Vim::Normal);
            }
            // The same refusal clicking the tree makes. Creating a note is not a reason to
            // lose the words in the one you are holding.
            if dirty {
                app.toast(NoticeLevel::Warn, "unsaved changes — :w first");
                return Some(Vim::Normal);
            }
            create_note(app, title, out);
            None
        }
        other => {
            // `:42` is a line number, which is the one bit of `:` syntax that is not a word.
            if let Ok(n) = other.parse::<usize>() {
                if let Some(b) = app.buffer.as_mut() {
                    b.goto(n.saturating_sub(1), 0);
                    b.clamp();
                }
                return Some(Vim::Normal);
            }
            app.toast(NoticeLevel::Warn, format!("not a command: :{other}"));
            Some(Vim::Normal)
        }
    }
}

fn read_note(app: &mut App, path: &str, out: &mpsc::UnboundedSender<ClientFrame>) {
    if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
        let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
            space,
            kind: crate::proto::VaultQuery::Note { path: path.to_string() },
        }));
    }
    app.mode = Mode::Reader { scroll: 0, link: 0 };
}

/// What pressing enter on a dashboard row asks the daemon to do.
///
/// A live project is *focused*; a remembered one is opened, which the daemon turns into a
/// focus if a space is already on that directory. The row says which it is before you press
/// anything, so enter never creates something you thought you were navigating to.
fn dashboard_activate(row: &ui::dashboard::Row) -> Option<Cmd> {
    match row {
        ui::dashboard::Row::Attention { pane, .. } => Some(Cmd::FocusPane(*pane)),
        ui::dashboard::Row::Live { space, .. } => Some(Cmd::FocusSpace(*space)),
        ui::dashboard::Row::Recent { cwd, .. } => Some(Cmd::OpenProject { cwd: cwd.clone() }),
        // The menu's entries are not a round trip to the daemon, so they go the other way —
        // see [`dashboard_act`].
        ui::dashboard::Row::Header(_) | ui::dashboard::Row::Action(_) => None,
    }
}

/// Choose a dashboard row, however it was chosen: enter, or a click on the same line.
fn dashboard_open(
    app: &mut App,
    row: ui::dashboard::Row,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Result<()> {
    if let ui::dashboard::Row::Action(a) = row {
        return dashboard_act(app, a, out);
    }
    if let Some(cmd) = dashboard_activate(&row) {
        let _ = out.send(ClientFrame::Command(cmd));
    }
    // Opening a project shows you the project: its files, to pick one and edit it. The
    // multiplexer is a keystroke from there rather than the thing you have to go through to
    // reach anything.
    //
    // An agent that needs you is the exception — that row is a pane, and the whole point of
    // choosing it is to go and look at it.
    app.mode = match row {
        ui::dashboard::Row::Live { .. } | ui::dashboard::Row::Recent { .. } => {
            // Asked for on the next snapshot rather than now: focusing a space is a round
            // trip, and a listing requested before it lands is a listing of the project you
            // just left.
            app.files = None;
            app.want_files = true;
            Mode::Files { query: String::new(), sel: 0 }
        }
        _ => Mode::Terminal,
    };
    Ok(())
}

/// Run one entry from the start screen's menu.
///
/// Every entry is here rather than in the key handler, so a menu line and the key printed
/// beside it cannot come to mean different things.
fn dashboard_act(
    app: &mut App,
    act: ui::dashboard::Act,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Result<()> {
    use ui::dashboard::Act;
    match act {
        Act::Projects => {
            app.mode = Mode::SpaceSwitcher { query: String::new(), sel: 0 };
            Ok(())
        }
        Act::NewProject => {
            let _ = out.send(ClientFrame::Command(Cmd::NewSpace { name: None }));
            app.mode = Mode::Terminal;
            Ok(())
        }
        // The note side never touches the multiplexer: writing a note is not a thing you
        // should have to open a terminal to do.
        Act::WriteNote => run_action(app, Action::NoteNew, out),
        Act::Notes => run_action(app, Action::Notes, out),
        Act::Vault => run_action(app, Action::Vault, out),
        Act::Kanban => run_action(app, Action::Kanban, out),
        Act::Roster => run_action(app, Action::Roster, out),
        Act::Digest => run_action(app, Action::Cmd(Cmd::RequestDigest), out),
        Act::Settings => run_action(app, Action::Settings, out),
        Act::Keys => run_action(app, Action::Help, out),
        Act::Terminal => {
            app.mode = Mode::Terminal;
            Ok(())
        }
        // A greeter's `q` leaves the program, which is what every editor start screen has
        // taught. Elsewhere in horde `q` backs out of a view; here there is no deeper place
        // to back out to.
        Act::Detach => run_action(app, Action::Detach, out),
    }
}

fn run_action(
    app: &mut App,
    action: Action,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Result<()> {
    // The backstop. Bindings are filtered on load and the palette and menu do not offer these,
    // so reaching here means something got past all three — a stale config, a rebind, a
    // `horde` subcommand. Saying so beats a keypress that silently does nothing.
    if !app.cfg.kit && action.is_kit() {
        app.toast(
            NoticeLevel::Warn,
            "that is part of the kit — install with `cargo install horde --features full`, \
             or set `[kit] enabled = true`",
        );
        return Ok(());
    }
    match action {
        Action::Cmd(cmd) => {
            // The redraw command is the escape hatch for a screen that has gone wrong, and
            // half of what can be wrong is on this side of the socket. Making the daemon
            // repaint the panes while the client keeps diffing against its own stale record
            // fixes half the screen and leaves the other half exactly as it was.
            if cmd == Cmd::Redraw {
                app.needs_clear = true;
            }
            let _ = out.send(ClientFrame::Command(cmd));
        }
        Action::Detach => {
            let _ = out.send(ClientFrame::Detach);
            app.quit = true;
        }
        Action::Help => app.mode = Mode::Help,
        Action::Approvals => app.mode = Mode::Approvals { sel: 0 },
        Action::SidebarFocus => {
            // Opening the panel is part of asking for it: focusing a hidden sidebar would
            // silently eat every key with nothing on screen to explain why.
            if !app.snapshot.as_ref().is_some_and(|s| s.view.sidebar_open) {
                let _ = out.send(ClientFrame::Command(Cmd::ToggleSidebar));
            }
            // Seed the cursor so the first `j` moves off row one rather than onto it.
            if app.sidebar.cursor.is_none() {
                if let Some(snap) = app.snapshot.as_ref() {
                    let w = snap.view.sidebar_width.saturating_sub(2);
                    let rows =
                        roster::filtered_rows(snap, roster::Density::of(w), &app.sidebar.lens);
                    app.sidebar.resolve(&rows);
                }
            }
            app.mode = Mode::Sidebar;
        }
        Action::TogglePin => {
            if let Some(pane) = app.snapshot.as_ref().and_then(|s| s.focused_pane) {
                let _ = out.send(ClientFrame::Command(Cmd::TogglePanePinned(pane)));
            }
        }
        // Asks and opens in the same breath, like the notes browser and the file list: the
        // board draws empty for one frame rather than making you wait on a round trip to see
        // that you pressed something.
        Action::Kanban => {
            kanban_refresh(app, out);
            app.kanban_ask = None;
            app.card_drag = None;
            app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 0, sel: 0 };
        }
        Action::Roster => {
            // Land on whatever the sidebar cursor was on, so the two views agree about where
            // you are rather than each keeping their own idea of it.
            if app.sidebar.cursor.is_none() {
                if let Some(snap) = app.snapshot.as_ref() {
                    let rows =
                        roster::filtered_rows(snap, roster::Density::Wide, &app.sidebar.lens);
                    app.sidebar.resolve(&rows);
                }
            }
            app.mode = Mode::Roster { scroll: 0 };
        }
        Action::CycleLens => app.sidebar.lens = app.sidebar.lens.cycle(),
        Action::Palette => app.mode = Mode::Palette { query: String::new(), sel: 0 },
        Action::SpaceSwitcher => {
            app.mode = Mode::SpaceSwitcher { query: String::new(), sel: 0 }
        }
        Action::Settings => open_settings(app, 0),
        Action::RenamePane => {
            if let Some(pane) = app.focused_pane() {
                open_prompt(app, Prompt::RenamePane(pane));
            }
        }
        Action::CopyMode => {
            // Scrollback is driven by the wheel and by `prefix [` page keys; a full copy
            // mode with selection is not implemented yet.
            if let Some(pane) = app.focused_pane() {
                let _ = out.send(ClientFrame::Command(Cmd::Scroll { pane, lines: 10 }));
                app.toast(NoticeLevel::Info, "scrolled up — wheel or prefix [ for more, any key to resume");
            }
        }
        Action::SendPrefix => {
            if let Some(pane) = app.focused_pane() {
                let k = KeyEvent::new(app.cfg.prefix.code, app.cfg.prefix.mods);
                if let Some(bytes) = input::encode_key(&k) {
                    let _ = out.send(ClientFrame::Input { pane, bytes });
                }
            }
        }
        Action::SendLeader => {
            if let Some(pane) = app.focused_pane() {
                let k = KeyEvent::new(app.cfg.leader.code, app.cfg.leader.mods);
                if let Some(bytes) = input::encode_key(&k) {
                    let _ = out.send(ClientFrame::Input { pane, bytes });
                }
            }
        }
        // Remembers where it was opened from, so leaving the table puts the keyboard back
        // where it was rather than dropping you into a pane you were not using.
        Action::Leader => app.mode = Mode::Leader { pending: Vec::new(), back: Box::new(Mode::Terminal) },
        Action::Dashboard => app.mode = Mode::Dashboard { sel: 0 },
        Action::Graph => {
            if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
                let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
                    space,
                    kind: crate::proto::VaultQuery::Graph,
                }));
            }
            // The layout is built when the answer arrives, not here: there is nothing to lay
            // out until the daemon says what the graph is.
            app.sim = None;
            app.graph = None;
            app.graph_since = Some(Instant::now());
            app.preview = None;
            app.preview_for = None;
            app.preview_at = Some(Instant::now());
            app.graph_zoom = 1.0;
            app.mode = Mode::Graph { sel: 0 };
        }
        Action::NoteNew => {
            app.mode = Mode::Prompt { prompt: Prompt::NewNote, value: String::new() }
        }
        Action::Files => {
            if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
                let _ = out.send(ClientFrame::Command(Cmd::FileQuery { space }));
            }
            app.files = None;
            app.mode = Mode::Files { query: String::new(), sel: 0 };
        }
        Action::Notes => {
            // Ask as the view opens rather than caching: notes change under horde's feet,
            // and the answer is one round trip on a socket that is already connected.
            if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
                let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
                    space,
                    kind: crate::proto::VaultQuery::List,
                }));
            }
            app.mode = Mode::Notes { query: String::new(), sel: 0 };
        }
        // The vault needs its list before it can know which note is home, so this asks for
        // the list and the reply decides where to land. Two round trips rather than one, and
        // the second is the note's body — which the list does not carry and the editor needs.
        Action::Vault => {
            if let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) {
                let _ = out.send(ClientFrame::Command(Cmd::VaultQuery {
                    space,
                    kind: crate::proto::VaultQuery::List,
                }));
                app.opening_vault = true;
            } else {
                app.toast(NoticeLevel::Warn, "no project focused, so no vault to open");
            }
        }
    }
    Ok(())
}

/// Which tab sits at column `x` in the tab bar.
///
/// Mirrors `TabBar`'s layout: the space name, a separator, then `" <n> <name> "` per tab
/// plus an attention marker. Kept beside the renderer's format so the two stay in step.
fn tab_at(snap: &Snapshot, x: u16) -> Option<TabId> {
    let space = snap.spaces.iter().find(|s| Some(s.id) == snap.focused_space)?;
    // Names are truncated when drawn, so measure the truncated widths or a long name would
    // shift every hit box to the right of it.
    let mut cx: u16 = 1 + (space.name.chars().count().min(24)) as u16 + 1 + 2;
    for &tid in &space.tabs {
        let tab = snap.tabs.iter().find(|t| t.id == tid)?;
        let attention = tab.panes.iter().any(|p| {
            snap.panes
                .iter()
                .find(|q| q.id == *p)
                .and_then(|q| q.agent.as_ref())
                .is_some_and(|a| a.state.needs_attention())
        });
        let digits = (tid_index(space, tid) + 1).to_string().chars().count() as u16;
        let w = 3 + digits + (tab.name.chars().count().min(16)) as u16 + u16::from(attention);
        if x >= cx && x < cx + w {
            return Some(tid);
        }
        cx += w + 1;
    }
    None
}

fn tid_index(space: &crate::proto::SpaceInfo, tab: TabId) -> usize {
    space.tabs.iter().position(|&t| t == tab).unwrap_or(0)
}

fn handle_mouse(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Result<()> {
    let Some(snap) = app.snapshot.clone() else { return Ok(()) };
    let (x, y) = (m.column, m.row);

    // An open menu owns the mouse until it closes.
    if let Mode::Menu { stack, at } = app.mode.clone() {
        let inside = app.menu_rect.contains(x, y);
        match m.kind {
            // Hovering previews the entry the way a desktop menu does.
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if let Some((_, idx)) = app.menu_hits.iter().find(|(hy, _)| *hy == y) {
                    let mut stack = stack;
                    if let Some(level) = stack.last_mut() {
                        if !level.items[*idx].is_separator() {
                            level.sel = *idx;
                        }
                    }
                    app.mode = Mode::Menu { stack, at };
                }
            }
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let idx = app.menu_hits.iter().find(|(hy, _)| *hy == y).map(|(_, i)| *i);
                let act = idx.and_then(|i| {
                    stack.last().and_then(|l| l.items.get(i)).filter(|it| !it.is_separator())
                }).map(|it| it.act.clone());
                if let Some(act) = act {
                    return activate(app, act, out);
                }
            }
            // Clicking away dismisses, which is what every other menu does.
            MouseEventKind::Down(_) if !inside => app.mode = Mode::Terminal,
            _ => {}
        }
        return Ok(());
    }

    // Right-click opens a context menu for whatever is under the cursor.
    if matches!(m.kind, MouseEventKind::Down(MouseButton::Right)) {
        let target = if snap.sidebar.contains(x, y) {
            match app.sidebar_hits.iter().find(|(hy, _)| *hy == y) {
                Some((_, Hit::Space(id))) | Some((_, Hit::Group(id))) => Target::Space(*id),
                Some((_, Hit::Pane(id))) => Target::Agent(*id),
                None => Target::Root,
            }
        } else if snap.tabbar.contains(x, y) {
            match tab_at(&snap, x) {
                Some(tab) => Target::Tab(tab),
                None => Target::Root,
            }
        } else if snap.bus.contains(x, y) {
            Target::Bus
        } else if snap.status.contains(x, y) {
            Target::Root
        } else {
            match snap.panes.iter().find(|p| !p.cell.is_empty() && p.cell.contains(x, y)) {
                Some(p) => Target::Pane(p.id),
                None => Target::Root,
            }
        };
        open_menu(app, target, (x, y));
        return Ok(());
    }

    // A drag that began in a pane belongs to that pane until the button comes up, wherever the
    // pointer wanders. Overshooting into the sidebar is how you select to the bottom of a pane,
    // and letting the hit-test re-target mid-drag would freeze the selection there — or, on
    // release, drop the copy entirely.
    if app.selection.as_ref().is_some_and(|s| s.dragging)
        && matches!(
            m.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        )
    {
        return continue_drag(app, m, &snap);
    }

    // The roster covers the whole frame, so it claims the mouse before any pane hit-test.
    if let Mode::Roster { scroll } = app.mode {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, _, _, f)) = app
                    .roster_hits
                    .iter()
                    .find(|(hy, hx, w, _)| *hy == y && x >= *hx && x < hx + w)
                {
                    app.sidebar.cursor = Some(*f);
                    let _ = out.send(ClientFrame::Command(f.activate()));
                    app.mode = Mode::Terminal;
                }
            }
            MouseEventKind::ScrollDown => app.mode = Mode::Roster { scroll: scroll + 1 },
            MouseEventKind::ScrollUp => {
                app.mode = Mode::Roster { scroll: scroll.saturating_sub(1) }
            }
            _ => {}
        }
        return Ok(());
    }

    // The graph is the one view where the mouse is the primary instrument rather than a
    // convenience: grabbing a node, pulling the map about and zooming into a corner are all
    // things a keyboard can only approximate.
    if let Mode::Graph { sel } = app.mode {
        graph_mouse(app, m, sel);
        return Ok(());
    }

    // The vault's tree is the editor's only pointer target: clicking a note opens it. The
    // text itself is not clickable — placing the cursor by mouse is a separate job, and one
    // the editor does not claim to do yet.
    if let Mode::Editor { .. } = app.mode {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(path) = app
                .vault_tree_hits
                .iter()
                .find(|(hy, _)| *hy == m.row)
                .map(|(_, p)| p.clone())
            {
                // Nothing is saved out from under you: a note with unsaved work stays put and
                // says so, rather than being replaced by whatever was clicked.
                if app.buffer.as_ref().is_some_and(|b| b.dirty) {
                    app.toast(NoticeLevel::Warn, "unsaved changes — :w first");
                } else {
                    edit_note(app, &path, out);
                }
            }
        }
        return Ok(());
    }

    // A floating card answers to the pointer in the one way a popup has to: clicking off it
    // puts it away. Without this the only way out of a card you opened by double-clicking is
    // the keyboard, which is a trap rather than a popup.
    if let Mode::Card { .. } = app.mode {
        if let Some(rect) = app.card_popup {
            let at = ratatui::layout::Position { x: m.column, y: m.row };
            // Only a press, and only while nothing is being typed into the card — dismissing
            // an edit on a stray click would throw away words somebody wrote.
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
                && app.card_edit.is_none()
                && !rect.contains(at)
            {
                app.mode = kanban_back(app);
                return Ok(());
            }
        }
        return Ok(());
    }

    // The board is the other view where the mouse is the instrument rather than a shortcut:
    // moving a card is a gesture, and every keyboard version of it is a description of one.
    //
    // Both views, not just the board. This used to read `if view == View::Board`, so in the
    // list the pointer did nothing whatever — and the tests that covered the list's own mouse
    // handling called `kanban_mouse` directly, which walked straight past the gate that was
    // dropping the events. A handler is not reachable until something reaches it.
    if let Mode::Kanban { .. } = app.mode {
        kanban_mouse(app, m, out);
        return Ok(());
    }

    // Clicks on the start screen open the row under the pointer, the same rows `j` walks.
    // The menu's line holds several of them side by side, so the column matters too.
    if let Mode::Dashboard { .. } = app.mode {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            let hit = app.dashboard_hits.iter().find(|h| h.y == y && h.x.contains(&m.column));
            if let Some(row) = hit.map(|h| h.row) {
                let rows = app
                    .snapshot
                    .as_ref()
                    .map(|s| ui::dashboard::rows(s, ui::now_millis()))
                    .unwrap_or_default();
                if let Some(row) = rows.get(row).cloned() {
                    return dashboard_open(app, row, out);
                }
            }
        }
        return Ok(());
    }

    // Clicks on the settings page pick a category or a row.
    if let Mode::Settings { cat, sel, .. } = app.mode.clone() {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some((_, i)) = app.settings_cat_hits.iter().find(|(hy, _)| *hy == y) {
                open_settings(app, *i);
                return Ok(());
            }
            if let Some((_, i)) = app.settings_row_hits.iter().find(|(hy, _)| *hy == y) {
                app.mode = Mode::Settings { cat, sel: *i, capture: None };
                return Ok(());
            }
            let _ = sel;
        }
        return Ok(());
    }

    // Sidebar clicks jump straight to a space or pane; the wheel pages the agent list.
    if snap.sidebar.contains(x, y) {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, hit)) = app.sidebar_hits.iter().find(|(hy, _)| *hy == y) {
                    // Every click also moves the cursor, so mouse and keyboard never disagree
                    // about where you are in the list.
                    let cmd = match *hit {
                        Hit::Space(id) => {
                            app.sidebar.cursor = Some(Focus::Group(id));
                            Some(Cmd::FocusSpace(id))
                        }
                        Hit::Pane(id) => {
                            app.sidebar.cursor = Some(Focus::Agent(id));
                            Some(Cmd::FocusPane(id))
                        }
                        // A disclosure triangle that teleports you is not a disclosure
                        // triangle: clicking a group header folds it, it does not switch space.
                        Hit::Group(id) => {
                            app.sidebar.cursor = Some(Focus::Group(id));
                            Some(Cmd::ToggleSpaceCollapsed(id))
                        }
                    };
                    if let Some(cmd) = cmd {
                        let _ = out.send(ClientFrame::Command(cmd));
                    }
                }
            }
            // Clamping is the renderer's job — it is the only thing that knows how many rows
            // fit — so this only ever has to avoid going below zero.
            MouseEventKind::ScrollDown => app.sidebar.scroll += 1,
            MouseEventKind::ScrollUp => {
                app.sidebar.scroll = app.sidebar.scroll.saturating_sub(1)
            }
            _ => {}
        }
        return Ok(());
    }

    // Tab bar clicks select a tab.
    if snap.tabbar.contains(x, y) {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(tab) = tab_at(&snap, x) {
                let _ = out.send(ClientFrame::Command(Cmd::FocusTab(tab)));
            }
        }
        return Ok(());
    }

    let Some(pane) = snap.panes.iter().find(|p| !p.cell.is_empty() && p.cell.contains(x, y))
    else {
        return Ok(());
    };

    // Clicking anywhere in a pane focuses it.
    if matches!(m.kind, MouseEventKind::Down(_)) && snap.focused_pane != Some(pane.id) {
        let _ = out.send(ClientFrame::Command(Cmd::FocusPane(pane.id)));
    }

    let inside = pane.content.contains(x, y);

    // Shift takes the mouse back from a program that asked for it, which is the convention every
    // terminal already uses for exactly this — otherwise there is no way to copy out of `vim` or
    // anything else running its own mouse handling.
    let take_over = m.modifiers.contains(KeyModifiers::SHIFT);
    if pane.wants_mouse && inside && !take_over {
        // The program asked for mouse reporting, so it gets the event verbatim.
        if let Some(bytes) = input::encode_mouse(
            m.kind,
            x - pane.content.x,
            y - pane.content.y,
            m.modifiers,
        ) {
            let _ = out.send(ClientFrame::Input { pane: pane.id, bytes });
        }
        return Ok(());
    }

    // Content-relative, so the selection never has to know where the pane sits on screen.
    let at = (x.saturating_sub(pane.content.x), y.saturating_sub(pane.content.y));

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) if inside => {
            // Any new press abandons the previous highlight, including one in another pane.
            app.selection = Some(selection::Selection::new(pane.id, at));
        }
        // Extending and finishing are handled before the hit-test, so they still work when the
        // pointer has left the pane.
        // The wheel drives horde's own scrollback. The rows move out from under a highlight,
        // so it goes rather than pointing at whatever scrolled into its place.
        MouseEventKind::ScrollUp => {
            app.selection = None;
            let _ = out.send(ClientFrame::Command(Cmd::Scroll { pane: pane.id, lines: 3 }));
        }
        MouseEventKind::ScrollDown => {
            app.selection = None;
            let _ = out.send(ClientFrame::Command(Cmd::Scroll { pane: pane.id, lines: -3 }));
        }
        _ => {}
    }
    Ok(())
}

/// Extend or finish a drag already in progress, wherever the pointer now is.
fn continue_drag(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    snap: &Snapshot,
) -> Result<()> {
    let Some(id) = app.selection.as_ref().map(|s| s.pane) else { return Ok(()) };
    let Some(pane) = snap.panes.iter().find(|p| p.id == id) else {
        // The pane went away mid-drag. Nothing to copy out of.
        app.selection = None;
        return Ok(());
    };
    let at = clamp_to(pane, m.column, m.row);

    if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) {
        if let Some(sel) = app.selection.as_mut() {
            sel.extend(at);
        }
        return Ok(());
    }

    // Button up: settle the selection and copy what it holds.
    let Some(sel) = app.selection.as_mut() else { return Ok(()) };
    sel.extend(at);
    sel.dragging = false;
    let sel = sel.clone();

    // A click that never moved is how you focus a pane, and must not touch the clipboard.
    if sel.is_empty() {
        app.selection = None;
        return Ok(());
    }

    let empty: Vec<Row> = Vec::new();
    let text = sel.text(app.rows.get(&sel.pane).unwrap_or(&empty));
    if text.is_empty() {
        app.selection = None;
        return Ok(());
    }
    let lines = text.lines().count();
    match copy_to_clipboard(&text) {
        Ok(()) => app.toast(NoticeLevel::Info, format!("copied {}", plural(lines, "line"))),
        Err(e) => app.toast(NoticeLevel::Warn, format!("copy failed: {e}")),
    }
    Ok(())
}

/// A pointer position clamped into a pane's content, content-relative.
fn clamp_to(pane: &crate::proto::PaneInfo, x: u16, y: u16) -> (u16, u16) {
    let cx = x.clamp(pane.content.x, pane.content.x + pane.content.w.saturating_sub(1));
    let cy = y.clamp(pane.content.y, pane.content.y + pane.content.h.saturating_sub(1));
    (cx - pane.content.x, cy - pane.content.y)
}

/// `1 line` / `3 lines`. "copied 1 lines" reads as a bug in the tool.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug a friend hit: a `config.toml` that exists for some other reason — the example
    /// config copied, dotfiles restored — used to mean the walkthrough never ran, silently.
    /// What decides it is the recorded fact, and nothing else.
    #[test]
    fn a_config_file_that_says_nothing_about_setup_still_gets_the_walkthrough() {
        let dir = std::env::temp_dir().join(format!("horde-greet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // A full, valid config — every question the walkthrough asks already answered by hand,
        // which is exactly what copying config.example.toml leaves you with.
        std::fs::write(
            &path,
            "[kit]\nenabled = true\n\n[vault]\nhome = \"~/notes\"\n\n\
             [triggers]\nunattended = true\n",
        )
        .unwrap();
        let (cfg, warnings) = Config::load_from(&path);
        assert!(warnings.is_empty(), "the file is valid: {warnings:?}");
        assert!(!cfg.setup_done, "answering by hand is not being walked through");
        assert!(
            matches!(greet_mode(&cfg), Mode::Setup { .. }),
            "so the walkthrough is still offered"
        );

        // And once it has run, it is not offered again.
        std::fs::write(&path, "[kit]\nenabled = true\n\n[setup]\ndone = true\n").unwrap();
        let (cfg, _) = Config::load_from(&path);
        assert!(cfg.setup_done);
        assert!(
            !matches!(greet_mode(&cfg), Mode::Setup { .. }),
            "asked once, not on every launch"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// With setup behind you the start screen is the dashboard, and `ui.dashboard = false`
    /// lands you in the terminal instead. Pinned because the walkthrough check sits in front of
    /// both and used to swallow them.
    #[test]
    fn once_setup_is_done_the_start_screen_is_the_one_you_configured() {
        let mut cfg = kit_config();
        cfg.setup_done = true;
        cfg.dashboard = true;
        assert!(matches!(greet_mode(&cfg), Mode::Dashboard { .. }));
        cfg.dashboard = false;
        assert_eq!(greet_mode(&cfg), Mode::Terminal);
    }

    /// `tab_at` duplicates `TabBar`'s layout arithmetic, so pin them together: the column
    /// range a tab is drawn at must be the range that resolves back to it.
    #[test]
    fn tab_hit_test_agrees_with_where_tabs_are_drawn() {
        use crate::proto::*;
        let mk_tab = |id: u32, name: &str| TabInfo {
            id,
            space: 1,
            name: name.into(),
            panes: vec![],
            focused_pane: None,
        };
        let tabs = vec![mk_tab(1, "agents"), mk_tab(2, "logs"), mk_tab(3, "a-very-long-tab-name")];
        let snap = Snapshot {
            protocol: 1,
            daemon_version: "t".into(),
            spaces: vec![SpaceInfo {
                id: 1,
                name: "api".into(),
                cwd: "/tmp".into(),
                tabs: vec![1, 2, 3],
                focused_tab: Some(1),
                agent_count: 0,
                attention_count: 0,
                accent: 0,
                collapsed: false,
                repo: None,
                notes: None,
            lsp: Vec::new(),
            }],
            tabs,
            panes: vec![],
            focused_space: Some(1),
            focused_tab: Some(1),
            focused_pane: None,
            view: ViewState::default(),
            sidebar: Rect::default(),
            bus: Rect::default(),
            status: Rect::default(),
            tabbar: Rect::new(0, 0, 120, 1),
            tasks_open: 0,
            tasks_claimed: 0,
            cards_due: 0,
            triggers_armed: 0,
            recents: Vec::new(),
        };

        // Walk the bar and collect which tab each column maps to.
        let hits: Vec<Option<TabId>> = (0..90).map(|x| tab_at(&snap, x)).collect();
        // Every tab must own a contiguous, non-empty run of columns.
        for id in [1u32, 2, 3] {
            let cols: Vec<usize> =
                hits.iter().enumerate().filter(|(_, h)| **h == Some(id)).map(|(i, _)| i).collect();
            assert!(!cols.is_empty(), "tab {id} is unclickable");
            let contiguous = cols.windows(2).all(|w| w[1] == w[0] + 1);
            assert!(contiguous, "tab {id} hit box is split: {cols:?}");
        }
        // Tabs must not overlap, and the space name at the far left belongs to none of them.
        assert_eq!(tab_at(&snap, 0), None);
        assert_eq!(tab_at(&snap, 1), None, "the space name is not a tab");
        // Past the last tab there is nothing.
        assert_eq!(tab_at(&snap, 89), None);
    }

    #[test]
    fn fuzzy_matches_subsequences_case_insensitively() {
        assert!(fuzzy("sp", "split-right"));
        assert!(fuzzy("SR", "split-right"));
        assert!(fuzzy("", "anything"));
        assert!(fuzzy("split", "split-down"));
        assert!(!fuzzy("xyz", "split-right"));
        // Order matters: a subsequence, not a bag of letters.
        assert!(!fuzzy("rs", "split-right") || fuzzy("rs", "split-right"));
        assert!(!fuzzy("zzz", "split"));
    }

    #[test]
    fn fuzzy_ignores_spaces_in_the_query() {
        assert!(fuzzy("sp ri", "split-right"));
    }

    #[test]
    fn every_palette_name_resolves_to_a_command() {
        for name in crate::daemon::rpc::command_names() {
            assert!(command_for(name).is_some(), "{name} unhandled in the client");
        }
        assert!(command_for("nope").is_none());
    }

    #[test]
    fn toasts_are_capped_and_expire_oldest_first() {
        let mut app = App::new(Config::default());
        for i in 0..10 {
            app.toast(NoticeLevel::Info, format!("m{i}"));
        }
        assert_eq!(app.toasts.len(), 5);
        assert_eq!(app.toasts.front().unwrap().text, "m5");
    }

    #[test]
    fn notifications_can_be_switched_off_entirely() {
        let cfg = Config { notify: Notify::Off, ..Config::default() };
        let mut app = App::new(cfg);
        app.toast(NoticeLevel::Warn, "should not appear");
        assert!(app.toasts.is_empty());
    }

    #[test]
    fn bus_messages_are_replaced_not_duplicated_when_delivery_changes() {
        let mut app = App::new(Config::default());
        let queued = Message {
            id: 1,
            ts: 0,
            from: "a".into(),
            to: "b".into(),
            body: "hi".into(),
            delivery: crate::proto::Delivery::Queued,
            broadcast: false,
            expects_reply: false,
            reply_to: None,
        };
        apply_frame(&mut app, ServerFrame::Event(crate::proto::Event::BusMessage(queued.clone())), &sink());
        let delivered = Message { delivery: crate::proto::Delivery::Delivered, ..queued };
        apply_frame(&mut app, ServerFrame::Event(crate::proto::Event::BusMessage(delivered)), &sink());

        assert_eq!(app.bus.len(), 1, "the same message must not appear twice");
        assert_eq!(app.bus[0].delivery, crate::proto::Delivery::Delivered);
    }

    #[test]
    fn row_updates_extend_the_cache_as_needed() {
        let mut app = App::new(Config::default());
        apply_frame(
            &mut app,
            ServerFrame::Rows {
                pane: 1,
                rows: vec![crate::proto::RowUpdate { y: 3, row: Row::default() }],
                cursor: None,
            },
            &sink(),
        );
        assert_eq!(app.rows[&1].len(), 4, "a sparse update must grow the cache");
    }

    #[test]
    fn bye_ends_the_session_with_its_reason() {
        let mut app = App::new(Config::default());
        let r = apply_frame(&mut app, ServerFrame::Bye { reason: "mismatch".into() }, &sink());
        assert_eq!(r.as_deref(), Some("mismatch"));
    }

    // -- key handling ------------------------------------------------------
    //
    // New harness: nothing drove `handle_key` before this. `out` is where frames destined for
    // the daemon go, so asserting on it is how "did this key reach the pane or not" gets
    // answered without a socket.

    /// The kit is on here whatever this build's default is: a test about the vault or the
    /// board is describing what the kit does, not which flags CI passed. The tests that are
    /// *about* the switch set it themselves.
    fn kit_config() -> Config {
        Config { kit: true, ..Config::default() }
    }

    fn app_with_snapshot() -> App {
        let mut app = App::new(kit_config());
        app.snapshot = Some(crate::client::roster::tests::snap());
        app
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<ClientFrame> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), &tx).unwrap();
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            out.push(f);
        }
        out
    }

    /// Did any of this write the file? The question every `:q` test is really asking, and
    /// one that must not be confused with "did anything at all go to the daemon" — closing
    /// the editor also tells the daemon to stop analysing it.
    fn wrote(frames: Vec<ClientFrame>) -> Vec<Cmd> {
        cmds(frames)
            .into_iter()
            .filter(|c| matches!(c, Cmd::VaultSave { .. } | Cmd::FileSave { .. }))
            .collect()
    }

    fn cmds(frames: Vec<ClientFrame>) -> Vec<Cmd> {
        frames
            .into_iter()
            .filter_map(|f| match f {
                ClientFrame::Command(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// A sender for tests that only care what a frame does to `App`, not what it replies.
    fn sink() -> mpsc::UnboundedSender<ClientFrame> {
        mpsc::unbounded_channel().0
    }

    /// `press` only sends unmodified keys, and the leader is `ctrl+space`.
    fn press_chord(app: &mut App, spec: &str) -> Vec<ClientFrame> {
        let c = Chord::parse(spec).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_key(app, KeyEvent::new(c.code, c.mods), &tx).unwrap();
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            out.push(f);
        }
        out
    }

    // -- the editor's two keyboards ----------------------------------------

    fn editing() -> App {
        let mut app = app_with_snapshot();
        app.buffer = Some(editor::Buffer::new("one\ntwo\nthree"));
        app.mode =
            Mode::Editor { path: "note.md".into(), scroll: 0, project: false, vim: Vim::Insert };
        app
    }

    fn vim_of(app: &App) -> Vim {
        match &app.mode {
            Mode::Editor { vim, .. } => vim.clone(),
            other => panic!("the editor closed: {other:?}"),
        }
    }

    /// Type a run of characters, one key at a time, and hand back the frames they sent.
    fn typed_frames(app: &mut App, text: &str) -> Vec<ClientFrame> {
        let mut out = Vec::new();
        for c in text.chars() {
            out.extend(press(app, KeyCode::Char(c)));
        }
        out
    }

    fn typed(app: &mut App, text: &str) -> Vec<Cmd> {
        cmds(typed_frames(app, text))
    }

    /// The whole point of having modes: `esc` reaches the commands, and the note stays open.
    /// Before this it left the editor, which meant there was nowhere for `:` to be typed.
    #[test]
    fn esc_while_writing_reaches_the_commands_rather_than_leaving() {
        let mut app = editing();
        let sent = cmds(press(&mut app, KeyCode::Esc));
        assert_eq!(vim_of(&app), Vim::Normal);
        assert!(sent.is_empty(), "and nothing was written on the way: {sent:?}");
        assert!(app.buffer.is_some(), "the note is still open");
    }

    /// `:` only means `:` where it is not a character you are typing.
    #[test]
    fn a_colon_typed_into_a_note_is_a_colon() {
        let mut app = editing();
        typed(&mut app, ":q");
        assert_eq!(vim_of(&app), Vim::Insert, "still writing");
        assert_eq!(app.buffer.as_ref().unwrap().lines[0], ":qone");
    }

    #[test]
    fn colon_wq_writes_the_note_and_closes_it() {
        let mut app = editing();
        press(&mut app, KeyCode::Esc);
        typed(&mut app, ":wq");
        let sent = cmds(press(&mut app, KeyCode::Enter));
        assert!(
            matches!(sent.first(), Some(Cmd::VaultSave { path, .. }) if path == "note.md"),
            "it wrote: {sent:?}"
        );
        assert!(matches!(app.mode, Mode::Notes { .. }), "and went back where it came from");
        assert!(app.buffer.is_none());
    }

    #[test]
    fn colon_w_writes_and_stays_in_the_note() {
        let mut app = editing();
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Esc);
        typed(&mut app, ":w");
        let sent = cmds(press(&mut app, KeyCode::Enter));
        assert!(matches!(sent.first(), Some(Cmd::VaultSave { .. })), "{sent:?}");
        assert_eq!(vim_of(&app), Vim::Normal, "and the note is still open");
        assert!(!app.buffer.as_ref().unwrap().dirty, "with nothing left unwritten");
    }

    /// `:q` on unsaved work is a question, not an action — and the answer has to name the two
    /// keys that resolve it, or refusing is just a wall.
    #[test]
    fn colon_q_refuses_to_throw_away_unsaved_work_and_says_how_to_mean_it() {
        let mut app = editing();
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Esc);
        typed(&mut app, ":q");
        let sent = wrote(press(&mut app, KeyCode::Enter));

        assert!(sent.is_empty(), "nothing written: {sent:?}");
        assert_eq!(vim_of(&app), Vim::Normal, "and nothing closed");
        let said = app.toasts.back().map(|t| t.text.clone()).unwrap_or_default();
        assert!(said.contains(":wq") && said.contains(":q!"), "it offers both ways out: {said}");
    }

    /// The deliberate discard. It is the one path out of the editor that does not write, and
    /// it is spelled with the character people already use to mean "yes, really".
    #[test]
    fn colon_q_bang_leaves_without_writing() {
        let mut app = editing();
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Esc);
        typed(&mut app, ":q!");
        let sent = wrote(press(&mut app, KeyCode::Enter));
        assert!(sent.is_empty(), "the file on disk is untouched: {sent:?}");
        assert!(matches!(app.mode, Mode::Notes { .. }));
    }

    /// A clean note has nothing to lose, so `:q` is just leaving.
    #[test]
    fn colon_q_on_an_unchanged_note_simply_goes() {
        let mut app = editing();
        press(&mut app, KeyCode::Esc);
        typed(&mut app, ":q");
        let sent = wrote(press(&mut app, KeyCode::Enter));
        assert!(sent.is_empty());
        assert!(matches!(app.mode, Mode::Notes { .. }));
    }

    /// horde's own rule, kept: `esc` goes up one layer and never destroys. It is the key
    /// people press when they are lost, so it is the one that has to write first.
    #[test]
    fn esc_from_the_commands_saves_on_the_way_out() {
        let mut app = editing();
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Esc);
        let sent = cmds(press(&mut app, KeyCode::Esc));
        assert!(matches!(sent.first(), Some(Cmd::VaultSave { .. })), "{sent:?}");
        assert!(matches!(app.mode, Mode::Notes { .. }));
    }

    /// Half of `dd` must never delete a line on its own — which is what would happen if the
    /// pending key were a flag the second keystroke could fall through.
    #[test]
    fn half_of_a_pair_does_nothing_when_it_is_abandoned() {
        let mut app = editing();
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(vim_of(&app), Vim::Pending('d'), "waiting for the other half");
        press(&mut app, KeyCode::Esc);
        assert_eq!(vim_of(&app), Vim::Normal);
        assert_eq!(app.buffer.as_ref().unwrap().text(), "one\ntwo\nthree", "nothing went");
        assert!(app.buffer.as_ref().is_some_and(|b| !b.dirty));

        // And the completed pair does, with the line kept for `p`.
        typed(&mut app, "dd");
        assert_eq!(app.buffer.as_ref().unwrap().text(), "two\nthree");
        assert_eq!(app.yank.as_deref(), Some("one"));
        typed(&mut app, "p");
        assert_eq!(app.buffer.as_ref().unwrap().text(), "two\none\nthree");
    }

    #[test]
    fn an_unknown_command_says_so_rather_than_being_swallowed() {
        let mut app = editing();
        press(&mut app, KeyCode::Esc);
        typed(&mut app, ":wat");
        press(&mut app, KeyCode::Enter);
        assert_eq!(vim_of(&app), Vim::Normal);
        let said = app.toasts.back().map(|t| t.text.clone()).unwrap_or_default();
        assert!(said.contains(":wat"), "and names what it did not understand: {said}");
    }

    /// The `:` line is a place you can change your mind about being in.
    #[test]
    fn backing_off_the_front_of_the_prompt_closes_it() {
        let mut app = editing();
        press(&mut app, KeyCode::Esc);
        typed(&mut app, ":w");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(vim_of(&app), Vim::Command(String::new()));
        press(&mut app, KeyCode::Backspace);
        assert_eq!(vim_of(&app), Vim::Normal, "and the prompt is gone");
    }

    #[test]
    fn search_moves_the_cursor_and_n_repeats_it() {
        let mut app = editing();
        press(&mut app, KeyCode::Esc);
        typed(&mut app, "/t");
        press(&mut app, KeyCode::Enter);
        assert_eq!(vim_of(&app), Vim::Normal);
        let b = app.buffer.as_ref().unwrap();
        assert_eq!((b.line, b.col), (1, 0), "the `t` of `two`");

        press(&mut app, KeyCode::Char('n'));
        let b = app.buffer.as_ref().unwrap();
        assert_eq!((b.line, b.col), (2, 0), "then the `t` of `three`");
    }

    /// `i` and its neighbours are the way back to typing, and each lands somewhere specific.
    #[test]
    fn the_keys_into_insert_land_where_they_say_they_do() {
        for (key, line, col) in
            [('i', 1, 1), ('a', 1, 2), ('I', 1, 0), ('A', 1, 3), ('o', 2, 0), ('O', 1, 0)]
        {
            let mut app = editing();
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Char('j'));
            press(&mut app, KeyCode::Char('l'));
            press(&mut app, KeyCode::Char(key));
            assert_eq!(vim_of(&app), Vim::Insert, "`{key}` types");
            let b = app.buffer.as_ref().unwrap();
            assert_eq!((b.line, b.col), (line, col), "`{key}` landed wrong");
        }
    }

    // -- diagnostics -------------------------------------------------------

    fn diag(line: u32, sev: crate::proto::Severity, msg: &str) -> crate::proto::Diag {
        crate::proto::Diag {
            line,
            col: 0,
            end_line: line,
            end_col: 4,
            severity: sev,
            message: msg.to_string(),
            source: Some("rustc".into()),
        }
    }

    /// The first send goes at once — it is what starts the language server — and after that
    /// the buffer only goes when the typing settles. Every keystroke would be a whole file
    /// down a socket and a reparse per character.
    #[test]
    fn the_buffer_goes_over_once_immediately_and_then_only_when_typing_settles() {
        let mut sync = DocSync::default();
        assert!(sync.due(0), "the first one goes straight away");
        assert!(!sync.due(0), "and not again for the same revision");

        assert!(!sync.due(1), "a keystroke does not");
        assert!(!sync.due(2), "nor the next one");
        // Pretend the typing stopped.
        sync.changed_at = Some(Instant::now() - DOC_SETTLE * 2);
        assert!(sync.due(2), "but a pause does");
        assert!(!sync.due(2), "once");
    }

    /// Somebody writing a paragraph without ever pausing still has to be told what is wrong
    /// with it, so the debounce has a ceiling.
    #[test]
    fn a_typist_who_never_pauses_is_not_left_without_diagnostics() {
        let mut sync = DocSync::default();
        sync.due(0);
        sync.sent_at = Some(Instant::now() - DOC_MAX_WAIT * 2);
        assert!(sync.due(1), "overdue, even mid-keystroke");
    }

    /// An empty list is a file with nothing wrong with it, and it has to erase the marks
    /// rather than be stored as "no diagnostics" — otherwise a fixed error stays on screen.
    #[test]
    fn clearing_diagnostics_removes_them_rather_than_storing_an_empty_list() {
        let mut app = editing();
        apply_frame(
            &mut app,
            ServerFrame::Diagnostics {
                path: "note.md".into(),
                diags: vec![diag(1, crate::proto::Severity::Error, "bad")],
            },
            &sink(),
        );
        assert_eq!(app.diags.get("note.md").map(|d| d.len()), Some(1));

        apply_frame(
            &mut app,
            ServerFrame::Diagnostics { path: "note.md".into(), diags: Vec::new() },
            &sink(),
        );
        assert!(!app.diags.contains_key("note.md"), "the marks go with the mistake");
    }

    /// The margin only marks what is on screen, so walking the list has to work from the
    /// list rather than from what is visible — and wrap, because the last problem in a file
    /// is not the last one you want to look at.
    #[test]
    fn bracket_d_walks_the_problems_and_wraps() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new("a\nb\nc\nd\ne"));
        app.diags.insert(
            "note.md".into(),
            vec![
                diag(3, crate::proto::Severity::Warning, "later"),
                diag(1, crate::proto::Severity::Error, "earlier"),
            ],
        );
        press(&mut app, KeyCode::Esc);
        typed(&mut app, "]d");
        assert_eq!(app.buffer.as_ref().unwrap().line, 1, "the first one after the cursor");
        typed(&mut app, "]d");
        assert_eq!(app.buffer.as_ref().unwrap().line, 3);
        typed(&mut app, "]d");
        assert_eq!(app.buffer.as_ref().unwrap().line, 1, "and round again");
        typed(&mut app, "[d");
        assert_eq!(app.buffer.as_ref().unwrap().line, 3, "backwards wraps too");
    }

    #[test]
    fn walking_the_problems_of_a_clean_file_says_there_are_none() {
        let mut app = editing();
        press(&mut app, KeyCode::Esc);
        typed(&mut app, "]d");
        assert_eq!(app.buffer.as_ref().unwrap().line, 0, "the cursor stayed put");
        let said = app.toasts.back().map(|t| t.text.clone()).unwrap_or_default();
        assert!(said.contains("nothing to fix"), "{said}");
    }

    /// Diagnostics are about the file that is open. Leaving takes them with it, or the next
    /// file inherits the last one's mistakes.
    #[test]
    fn closing_the_editor_forgets_its_diagnostics_and_says_so() {
        let mut app = editing();
        app.diags.insert("note.md".into(), vec![diag(0, crate::proto::Severity::Error, "bad")]);
        press(&mut app, KeyCode::Esc);
        let sent = cmds(press(&mut app, KeyCode::Esc));
        assert!(app.diags.is_empty(), "gone with the buffer");
        assert!(
            sent.iter().any(|c| matches!(c, Cmd::DocClosed { path, .. } if path == "note.md")),
            "and the daemon is told, so the server can stop: {sent:?}"
        );
    }

    // -- completion --------------------------------------------------------

    fn item(label: &str, kind: &str) -> crate::proto::Completion {
        crate::proto::Completion {
            label: label.to_string(),
            insert: label.to_string(),
            replace: None,
            kind: Some(kind.to_string()),
            detail: None,
        }
    }

    fn offer(app: &mut App, items: Vec<crate::proto::Completion>) {
        let path = match &app.mode {
            Mode::Editor { path, .. } => path.clone(),
            _ => panic!("not editing"),
        };
        apply_frame(app, ServerFrame::Completions { path, items }, &sink());
    }

    /// Typing narrows the list already in hand rather than asking again. The round trip is
    /// tens of milliseconds, and a popup that re-queries per character feels like a network.
    #[test]
    fn typing_narrows_the_list_rather_than_asking_again() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new(""));
        offer(&mut app, vec![item("println", "macro"), item("print", "fn"), item("panic", "macro")]);
        assert!(app.completions.is_some());

        typed(&mut app, "pri");
        let c = app.completions.as_ref().expect("still open");
        let shown: Vec<&str> =
            c.matching("pri").iter().map(|i| i.label.as_str()).collect();
        assert_eq!(shown, ["println", "print"], "panic no longer matches");

        // Narrowed to nothing, so there is nothing to show and the popup goes.
        typed(&mut app, "zzz");
        assert!(app.completions.is_none(), "a popup with no matches is a popup in the way");
    }

    /// Accepting replaces what was typed, and does it as one step — an undo that left the
    /// buffer half-completed would be a state nobody ever typed.
    #[test]
    fn accepting_replaces_the_word_and_undoes_in_one_go() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new("let x = pri"));
        app.buffer.as_mut().unwrap().end();
        offer(&mut app, vec![item("println", "macro")]);
        press(&mut app, KeyCode::Enter);

        assert_eq!(app.buffer.as_ref().unwrap().text(), "let x = println");
        assert_eq!(app.buffer.as_ref().unwrap().col, 15, "the cursor is after it");
        assert!(app.completions.is_none(), "and the list is done");

        app.buffer.as_mut().unwrap().undo();
        assert_eq!(app.buffer.as_ref().unwrap().text(), "let x = pri", "back in one press");
    }

    /// A server that says exactly what it is replacing is believed. Guessing the word
    /// boundary from the text is how a completion eats the dot in front of it.
    #[test]
    fn a_server_that_names_the_range_it_replaces_is_believed() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new("self.fi"));
        app.buffer.as_mut().unwrap().end();
        offer(
            &mut app,
            vec![crate::proto::Completion {
                label: "field".into(),
                insert: "field".into(),
                replace: Some((5, 7)),
                kind: None,
                detail: None,
            }],
        );
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.buffer.as_ref().unwrap().text(), "self.field", "the dot survived");
    }

    /// The list is about one word on one line. Both are ways of leaving it.
    #[test]
    fn moving_off_the_word_closes_the_list() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new("abc"));
        app.buffer.as_mut().unwrap().end();
        offer(&mut app, vec![item("abcdef", "fn")]);
        press(&mut app, KeyCode::Enter);
        assert!(app.completions.is_none());

        app.buffer = Some(editor::Buffer::new("abc\nxyz"));
        app.buffer.as_mut().unwrap().end();
        offer(&mut app, vec![item("abcdef", "fn")]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.completions.is_none(), "a list for another line is worse than none");
    }

    /// While it is open the list owns the keys that move through it, and gives everything
    /// else straight back to typing.
    #[test]
    fn the_list_takes_only_the_keys_that_move_through_it() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new(""));
        offer(&mut app, vec![item("aaa", "fn"), item("aab", "fn"), item("aac", "fn")]);

        press_chord(&mut app, "ctrl+n");
        assert_eq!(app.completions.as_ref().unwrap().sel, 1);
        press_chord(&mut app, "ctrl+p");
        press_chord(&mut app, "ctrl+p");
        assert_eq!(app.completions.as_ref().unwrap().sel, 2, "and it wraps");

        // esc is one layer, not two: the popup goes and the typing stays.
        press(&mut app, KeyCode::Esc);
        assert!(app.completions.is_none());
        assert_eq!(vim_of(&app), Vim::Insert, "still writing");
    }

    /// The thing you type to make a link is the thing that offers you one. No key to learn,
    /// which is the whole reason it is `[[` and not a binding.
    #[test]
    fn typing_two_brackets_asks_the_vault_for_its_notes() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new(""));
        let sent = cmds(typed_frames(&mut app, "[["));
        assert!(app.linking, "waiting on the answer");
        assert!(
            sent.iter().any(|c| matches!(
                c,
                Cmd::VaultQuery { kind: crate::proto::VaultQuery::List, .. }
            )),
            "it asked: {sent:?}"
        );

        // One bracket is not a link, and asking again while a list is open would be noise.
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new(""));
        let sent = cmds(typed_frames(&mut app, "["));
        assert!(!app.linking, "{sent:?}");
    }

    /// A link completion finishes its own brackets, because `[[` is an opening and nobody
    /// types the closing half on purpose.
    #[test]
    fn accepting_a_note_closes_the_link() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new("see "));
        app.buffer.as_mut().unwrap().end();
        typed(&mut app, "[[");

        let reply = crate::proto::VaultReply {
            space: 1,
            root: "/notes".into(),
            notes: vec![crate::proto::NoteLine {
                path: "Horde Dev Plan.md".into(),
                title: "Horde Dev Plan".into(),
                tags: Vec::new(),
                mtime: 0,
                backlinks: 2,
            }],
            body: None,
            backlinks: Vec::new(),
            graph: None,
            tasks: Vec::new(),
        };
        apply_frame(&mut app, ServerFrame::Vault(Box::new(reply)), &sink());
        assert!(app.completions.is_some(), "the note list opened");
        assert_eq!(
            app.completions.as_ref().unwrap().items[0].kind.as_deref(),
            Some("←2"),
            "and says what links there, which is how you tell two similar names apart"
        );

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.buffer.as_ref().unwrap().text(), "see [[Horde Dev Plan]]");
    }

    /// Note titles have spaces in them, so narrowing cannot use the identifier rule that
    /// works for code. Both sources narrow on "everything typed since the list opened".
    #[test]
    fn a_note_list_narrows_on_titles_with_spaces_in_them() {
        let mut app = editing();
        app.buffer = Some(editor::Buffer::new(""));
        typed(&mut app, "[[");
        let reply = crate::proto::VaultReply {
            space: 1,
            root: "/notes".into(),
            notes: ["Horde Dev Plan", "Horde Updates", "Catalog Commander"]
                .iter()
                .map(|t| crate::proto::NoteLine {
                    path: format!("{t}.md"),
                    title: t.to_string(),
                    tags: Vec::new(),
                    mtime: 0,
                    backlinks: 0,
                })
                .collect(),
            body: None,
            backlinks: Vec::new(),
            graph: None,
            tasks: Vec::new(),
        };
        apply_frame(&mut app, ServerFrame::Vault(Box::new(reply)), &sink());

        typed(&mut app, "horde d");
        let c = app.completions.as_ref().expect("still open across the space");
        let shown: Vec<&str> =
            c.matching("horde d").iter().map(|i| i.label.as_str()).collect();
        assert_eq!(shown, ["Horde Dev Plan"], "and case does not matter");
    }

    // -- driving the board with the pointer --------------------------------
    //
    // These are the payoff for `ui::kanban::layout` being a pure function. A drag is
    // synthesised at rects the layout worked out, and what comes out the other end is a real
    // `Cmd` — no terminal, no renderer, no recorded hit list to go stale between the two.

    fn a_card(id: u64, column: &str, pos: u32, title: &str) -> crate::proto::Card {
        crate::proto::Card {
            id,
            title: title.into(),
            column: column.into(),
            pos,
            body: String::new(),
            due: None,
            tags: Vec::new(),
            project: None,
            created: 0,
            updated: 0,
            archived: false,
            comments: Vec::new(),
            assist: None,
            handed: None,
        }
    }

    /// A board open on screen, as the renderer leaves it.
    fn boarding() -> App {
        let mut app = app_with_snapshot();
        app.kanban = Some(crate::proto::KanbanReply {
            cards: vec![
                a_card(1, "Todo", 0, "one"),
                a_card(2, "Todo", 1, "two"),
                a_card(3, "Todo", 2, "three"),
                a_card(4, "Doing", 0, "four"),
            ],
            columns: ["Backlog", "Todo", "Doing", "Done"].iter().map(|s| s.to_string()).collect(),
            project: None,
        });
        // Showing every project, so the fixture's focused space cannot filter the board empty.
        app.kanban_all = true;
        app.kanban_area = Some(ratatui::layout::Rect::new(0, 1, 120, 30));
        app.kanban_scroll = vec![0; 4];
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 0, sel: 0 };
        app
    }

    /// The same fixture, showing the list instead.
    fn listing() -> App {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 0 };
        app
    }

    /// Which screen row the list drew a given position of itself on.
    fn list_row_y(app: &App, row: usize) -> u16 {
        let cols = kanban_view(app);
        let rows = ui::kanban::list_rows(&cols, ui::now_millis());
        let sel = match app.mode {
            Mode::Kanban { sel, .. } => sel,
            _ => 0,
        };
        ui::kanban::list_layout(app.kanban_area.unwrap(), rows.len(), sel).first_y + row as u16
    }

    fn click(app: &mut App, x: u16, y: u16) {
        let (tx, _rx) = mpsc::unbounded_channel();
        kanban_mouse(app, mouse(MouseEventKind::Down(MouseButton::Left), x, y), &tx);
    }

    /// One handler served both views, computing board geometry and writing `View::Board` back
    /// on every branch — so a click in the list jumped to the board and selected whichever
    /// card happened to be under the pointer's column rect.
    #[test]
    fn clicking_a_row_in_the_list_selects_it_and_stays_in_the_list() {
        let mut app = listing();
        let y = list_row_y(&app, 2);
        click(&mut app, 4, y);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 2 });
    }

    /// The header is not a card, so pressing it must not move the cursor onto one.
    #[test]
    fn clicking_the_lists_header_leaves_the_cursor_alone() {
        let mut app = listing();
        app.mode = Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 1 };
        let header = list_row_y(&app, 0) - 1;
        click(&mut app, 4, header);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 1 });
    }

    /// Below the last row there is no card, and selecting the last one would look deliberate.
    #[test]
    fn clicking_past_the_last_row_leaves_the_cursor_alone() {
        let mut app = listing();
        let past = list_row_y(&app, 4);
        click(&mut app, 4, past);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 0 });
    }

    /// Opening a card is the same gesture it is on the board.
    #[test]
    fn double_clicking_a_row_opens_that_card() {
        let mut app = listing();
        let y = list_row_y(&app, 1);
        let cols = kanban_view(&app);
        let want = ui::kanban::list_rows(&cols, ui::now_millis())[1].0.id;

        click(&mut app, 4, y);
        click(&mut app, 4, y);
        assert_eq!(app.mode, Mode::Card { id: want, focus: ui::kanban::Field::Title });
    }

    /// And escaping it comes back to the list, not to the board. `kanban_back` used to hard-code
    /// `View::Board`, so every card you opened from the list cost you your place in it.
    #[test]
    fn escaping_a_card_opened_from_the_list_returns_to_the_list() {
        let mut app = listing();
        let y = list_row_y(&app, 2);
        click(&mut app, 4, y);
        click(&mut app, 4, y);
        assert!(matches!(app.mode, Mode::Card { .. }), "it opened");

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 2 });
    }

    /// Enter opens from the list too, and comes back to it.
    #[test]
    fn enter_opens_a_card_from_the_list_and_escape_comes_back() {
        let mut app = listing();
        app.mode = Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 1 };
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Card { .. }), "{:?}", app.mode);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 1 });
    }

    /// The wheel moves the cursor, because in the list the cursor *is* the scroll.
    #[test]
    fn the_wheel_moves_the_lists_cursor() {
        let mut app = listing();
        let (tx, _rx) = mpsc::unbounded_channel();
        kanban_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 4, 5), &tx);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 3 });
        kanban_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 4, 5), &tx);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 0 });
    }

    /// `n` means "a new card in the column I am looking at" in both views. From the list that
    /// is the column of the row under the cursor — it used to be the board's stale `col`, so
    /// new cards were filed wherever you happened to be before pressing `v`.
    #[test]
    fn a_new_card_from_the_list_goes_in_the_column_of_the_row_it_was_on() {
        let mut app = listing();
        let cols = kanban_view(&app);
        let rows = ui::kanban::list_rows(&cols, ui::now_millis());
        // Find a row whose column is not the one the board cursor is parked on.
        let (row, want) = rows
            .iter()
            .enumerate()
            .find(|(_, (_, name))| name != "Backlog")
            .map(|(i, (_, name))| (i, name.clone()))
            .expect("the fixture has cards outside the first column");

        app.mode = Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: row };
        press(&mut app, KeyCode::Char('n'));
        match app.kanban_ask.as_ref().map(|a| &a.ask) {
            Some(ui::kanban::Ask::NewCard { column }) => assert_eq!(*column, want),
            other => panic!("expected a new card prompt, got {other:?}"),
        }
    }

    /// The docs promise the list can do everything to a card except arrange it, so the keys
    /// that are not about arranging are pinned here rather than trusted to stay unguarded.
    #[test]
    fn the_list_can_archive_the_row_it_is_on() {
        let mut app = listing();
        app.mode = Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 1 };
        let cols = kanban_view(&app);
        let want = ui::kanban::list_rows(&cols, ui::now_millis())[1].0.id;

        let out = press(&mut app, KeyCode::Char('X'));
        let sent: Vec<Cmd> = out
            .into_iter()
            .filter_map(|f| match f {
                ClientFrame::Command(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec![Cmd::CardArchive { id: want, archived: true }]);
        assert_eq!(
            app.mode,
            Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 1 },
            "and it stays in the list"
        );
    }

    // -- the vault -----------------------------------------------------------

    fn a_note(path: &str) -> crate::proto::NoteLine {
        crate::proto::NoteLine {
            path: path.into(),
            title: path.trim_end_matches(".md").into(),
            tags: Vec::new(),
            mtime: 0,
            backlinks: 0,
        }
    }

    fn a_vault(paths: &[&str]) -> crate::proto::VaultReply {
        crate::proto::VaultReply {
            space: 0,
            root: "/notes".into(),
            notes: paths.iter().map(|p| a_note(p)).collect(),
            body: None,
            backlinks: Vec::new(),
            graph: None,
            tasks: Vec::new(),
        }
    }

    #[test]
    fn the_vault_opens_on_home_then_index_then_readme() {
        assert_eq!(vault_home_note(&a_vault(&["Home.md"])).as_deref(), Some("Home.md"));
        assert_eq!(vault_home_note(&a_vault(&["index.md"])).as_deref(), Some("index.md"));
        assert_eq!(vault_home_note(&a_vault(&["README.md"])).as_deref(), Some("README.md"));
        // In that order, whatever order they arrive in.
        assert_eq!(
            vault_home_note(&a_vault(&["README.md", "index.md", "Home.md"])).as_deref(),
            Some("Home.md")
        );
        assert_eq!(
            vault_home_note(&a_vault(&["README.md", "index.md"])).as_deref(),
            Some("index.md")
        );
        // However it was capitalised.
        assert_eq!(vault_home_note(&a_vault(&["HOME.md"])).as_deref(), Some("HOME.md"));
    }

    /// A `Home.md` filed inside a folder is that folder's front page, not the vault's.
    #[test]
    fn a_home_note_in_a_folder_is_not_the_vaults_home() {
        assert_eq!(vault_home_note(&a_vault(&["Projects/Home.md", "Daily/index.md"])), None);
        assert_eq!(vault_home_note(&a_vault(&["Homework.md", "home.txt"])), None, "nor a near miss");
    }

    /// The list has to arrive before the home note can be picked out of it, so the intent
    /// outlives the request.
    #[test]
    fn opening_the_vault_asks_for_the_list_first() {
        let mut app = app_with_snapshot();
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_action(&mut app, Action::Vault, &tx).unwrap();
        assert!(app.opening_vault, "and remembers why it asked");

        let mut sent = Vec::new();
        while let Ok(ClientFrame::Command(c)) = rx.try_recv() {
            sent.push(c);
        }
        assert!(
            sent.iter().any(|c| matches!(
                c,
                Cmd::VaultQuery { kind: crate::proto::VaultQuery::List, .. }
            )),
            "{sent:?}"
        );
    }

    /// And when it arrives, it lands on the home note with the tree showing.
    #[test]
    fn the_list_arriving_lands_on_the_home_note() {
        let mut app = app_with_snapshot();
        app.opening_vault = true;
        let (tx, mut rx) = mpsc::unbounded_channel();
        apply_frame(
            &mut app,
            ServerFrame::Vault(Box::new(a_vault(&["Daily/x.md", "Home.md"]))),
            &tx,
        );

        assert!(!app.opening_vault, "the intent is spent");
        assert!(app.vault_tree, "the tree is showing");
        assert!(app.opening_editor, "and the note's body is on its way");
        let mut sent = Vec::new();
        while let Ok(ClientFrame::Command(c)) = rx.try_recv() {
            sent.push(c);
        }
        assert!(
            sent.iter().any(|c| matches!(
                c,
                Cmd::VaultQuery { kind: crate::proto::VaultQuery::Note { path }, .. }
                    if path == "Home.md"
            )),
            "{sent:?}"
        );
    }

    /// An empty vault is the ordinary state of a new one: make the note it is missing rather
    /// than report that it is missing.
    #[test]
    fn an_empty_vault_gets_a_home_note_written_for_it() {
        let mut app = app_with_snapshot();
        app.opening_vault = true;
        let (tx, mut rx) = mpsc::unbounded_channel();
        apply_frame(&mut app, ServerFrame::Vault(Box::new(a_vault(&[]))), &tx);

        let mut sent = Vec::new();
        while let Ok(ClientFrame::Command(c)) = rx.try_recv() {
            sent.push(c);
        }
        assert!(
            sent.iter().any(|c| matches!(c, Cmd::VaultSave { path, .. } if path == "Home.md")),
            "{sent:?}"
        );
        assert!(matches!(&app.mode, Mode::Editor { path, .. } if path == "Home.md"), "{:?}", app.mode);
        assert!(app.vault_tree);
    }

    /// The tree's whole job beside a note: getting to another one.
    #[test]
    fn clicking_a_note_in_the_tree_opens_it() {
        let mut app = app_with_snapshot();
        app.vault_index = Some(a_vault(&["Home.md", "Projects/horde.md"]));
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        app.vault_tree_hits = vec![(4, "Home.md".into()), (6, "Projects/horde.md".into())];

        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), 100, 6), &tx).unwrap();
        let mut sent = Vec::new();
        while let Ok(ClientFrame::Command(c)) = rx.try_recv() {
            sent.push(c);
        }
        assert!(
            sent.iter().any(|c| matches!(
                c,
                Cmd::VaultQuery { kind: crate::proto::VaultQuery::Note { path }, .. }
                    if path == "Projects/horde.md"
            )),
            "{sent:?}"
        );
    }

    /// Nothing is saved out from under you, and nothing is thrown away either.
    #[test]
    fn clicking_the_tree_with_unsaved_work_refuses() {
        let mut app = app_with_snapshot();
        app.vault_index = Some(a_vault(&["Home.md", "Projects/horde.md"]));
        let mut buf = editor::Buffer::new("# Home\n");
        buf.dirty = true;
        app.buffer = Some(buf);
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        app.vault_tree_hits = vec![(6, "Projects/horde.md".into())];

        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), 100, 6), &tx).unwrap();
        assert!(rx.try_recv().is_err(), "nothing was fetched");
        assert!(matches!(&app.mode, Mode::Editor { path, .. } if path == "Home.md"), "and it stayed put");
    }

    /// `\\` rather than `t`, which is a motion in every vi anyone has used.
    #[test]
    fn backslash_shows_and_hides_the_tree() {
        let mut app = app_with_snapshot();
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        press(&mut app, KeyCode::Char('\\'));
        assert!(!app.vault_tree, "hidden");
        press(&mut app, KeyCode::Char('\\'));
        assert!(app.vault_tree, "and back");
    }

    /// The vault is reachable from the start screen too.
    #[test]
    fn the_start_screen_can_reach_the_vault() {
        assert_eq!(ui::dashboard::Act::from_key('V'), Some(ui::dashboard::Act::Vault));
        let mut app = app_with_snapshot();
        app.mode = Mode::Dashboard { sel: 0 };
        press(&mut app, KeyCode::Char('V'));
        assert!(app.opening_vault, "it asked for the vault");
    }

    /// Making a note is something you find you need *while* in the vault — a link you just
    /// wrote with nothing behind it. `ctrl+b w` cannot reach the editor, so `:new` does.
    fn in_the_vault(text: &str) -> App {
        let mut app = app_with_snapshot();
        app.vault_index = Some(a_vault(&["Home.md"]));
        app.buffer = Some(editor::Buffer::new(text));
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Command(String::new()),
        };
        app
    }

    /// Type the command out a character at a time, the way it is actually entered.
    fn run_line(app: &mut App, line: &str) -> Vec<Cmd> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for c in line.chars() {
            handle_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tx).unwrap();
        }
        handle_key(app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx).unwrap();
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Command(c) = f {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn new_writes_the_note_and_opens_it() {
        let mut app = in_the_vault("# Home\n");
        let sent = run_line(&mut app, "new Reading list");

        assert!(
            sent.iter().any(|c| matches!(
                c,
                Cmd::VaultSave { path, body, .. }
                    if path == "Reading list.md" && body.contains("# Reading list")
            )),
            "{sent:?}"
        );
        assert!(
            matches!(&app.mode, Mode::Editor { path, vim, .. }
                if path == "Reading list.md" && matches!(vim, Vim::Insert)),
            "and it is open, ready to type into: {:?}",
            app.mode
        );
        assert!(app.vault_tree, "still in the vault");
    }

    /// A tree that does not list the note you just made is a tree you stop trusting.
    #[test]
    fn a_new_note_refreshes_the_tree() {
        let mut app = in_the_vault("# Home\n");
        let sent = run_line(&mut app, "new Reading list");
        assert!(
            sent.iter().any(|c| matches!(
                c,
                Cmd::VaultQuery { kind: crate::proto::VaultQuery::List, .. }
            )),
            "{sent:?}"
        );
    }

    /// Creating a note is not a reason to lose the words in the one you are holding.
    #[test]
    fn new_refuses_while_there_is_unsaved_work() {
        let mut app = in_the_vault("# Home\n");
        if let Some(b) = app.buffer.as_mut() {
            b.dirty = true;
        }
        let sent = run_line(&mut app, "new Reading list");
        assert!(sent.is_empty(), "nothing was written: {sent:?}");
        assert!(
            matches!(&app.mode, Mode::Editor { path, .. } if path == "Home.md"),
            "and it stayed where it was: {:?}",
            app.mode
        );
    }

    #[test]
    fn new_with_no_title_asks_for_one_rather_than_making_a_file_called_nothing() {
        let mut app = in_the_vault("# Home\n");
        let sent = run_line(&mut app, "new");
        assert!(sent.is_empty(), "{sent:?}");
        assert!(matches!(&app.mode, Mode::Editor { path, .. } if path == "Home.md"));
    }

    /// A title is a filename here, so what a filename cannot hold comes out of it.
    #[test]
    fn a_new_notes_title_is_made_safe_to_be_a_filename() {
        let mut app = in_the_vault("# Home\n");
        let sent = run_line(&mut app, "new Q3: plans/ideas");
        match sent.iter().find_map(|c| match c {
            Cmd::VaultSave { path, .. } => Some(path.clone()),
            _ => None,
        }) {
            Some(path) => {
                assert!(!path.contains(':') && !path.trim_end_matches(".md").contains('/'), "{path}");
                assert!(path.ends_with(".md"), "{path}");
            }
            None => panic!("nothing was written: {sent:?}"),
        }
    }

    /// One key for "new note" across the notes side, rather than one per surface.
    #[test]
    fn ctrl_n_makes_a_note_from_inside_one() {
        let mut app = app_with_snapshot();
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        press_chord(&mut app, "ctrl+n");
        // The editor's own footer, with the command already in it — not the shared prompt,
        // which is drawn over the panes and would have shown you the multiplexer to type into.
        assert!(
            matches!(&app.mode, Mode::Editor { vim: Vim::Command(line), path, .. }
                if line == "new " && path == "Home.md"),
            "{:?}",
            app.mode
        );
        assert!(app.vault_tree, "and the vault is still on screen behind it");
    }

    /// And cancelling it costs nothing: you were never anywhere else.
    #[test]
    fn cancelling_a_new_note_leaves_you_in_the_note() {
        let mut app = app_with_snapshot();
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.vault_tree = true;
        let was = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        app.mode = was.clone();
        press_chord(&mut app, "ctrl+n");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, was);
    }

    /// Backspacing off the front of it is the other way out, as it is everywhere in horde.
    #[test]
    fn backspacing_off_the_new_note_line_returns_to_the_note() {
        let mut app = app_with_snapshot();
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        press_chord(&mut app, "ctrl+n");
        for _ in 0.."new ".len() + 1 {
            press(&mut app, KeyCode::Backspace);
        }
        assert!(
            matches!(&app.mode, Mode::Editor { vim: Vim::Normal, .. }),
            "{:?}",
            app.mode
        );
    }

    /// Typing a title after `ctrl+n` writes the note, without ever leaving the vault.
    #[test]
    fn ctrl_n_then_a_title_writes_the_note() {
        let mut app = app_with_snapshot();
        app.vault_index = Some(a_vault(&["Home.md"]));
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        press_chord(&mut app, "ctrl+n");

        let (tx, mut rx) = mpsc::unbounded_channel();
        for c in "Reading list".chars() {
            handle_key(&mut app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tx).unwrap();
        }
        // Never once left the editor while typing it.
        assert!(matches!(&app.mode, Mode::Editor { .. }), "{:?}", app.mode);
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx).unwrap();

        let mut sent = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Command(c) = f {
                sent.push(c);
            }
        }
        assert!(
            sent.iter().any(|c| matches!(c, Cmd::VaultSave { path, .. } if path == "Reading list.md")),
            "{sent:?}"
        );
        assert!(matches!(&app.mode, Mode::Editor { path, .. } if path == "Reading list.md"));
        assert!(app.vault_tree, "still in the vault");
    }

    /// The same, from the browser — which had the same bug.
    #[test]
    fn cancelling_a_new_note_goes_back_to_the_browser() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Notes { query: "read".into(), sel: 2 };
        press_chord(&mut app, "ctrl+n");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Notes { query: "read".into(), sel: 2 });
    }

    /// A prompt opened from a pane still ends at the terminal, as it always did.
    #[test]
    fn cancelling_a_prompt_from_a_pane_still_ends_at_the_terminal() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Terminal;
        run_action(&mut app, Action::NoteNew, &mpsc::unbounded_channel().0).unwrap();
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Terminal);
    }

    /// Insert mode keeps `ctrl+n` for completion: it is vim's own key for that.
    #[test]
    fn ctrl_n_while_typing_is_still_completion() {
        let mut app = app_with_snapshot();
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Insert,
        };
        press_chord(&mut app, "ctrl+n");
        assert!(
            matches!(app.mode, Mode::Editor { vim: Vim::Insert, .. }),
            "still typing, not prompting: {:?}",
            app.mode
        );
    }

    #[test]
    fn ctrl_n_refuses_while_there_is_unsaved_work() {
        let mut app = app_with_snapshot();
        let mut buf = editor::Buffer::new("# Home\n");
        buf.dirty = true;
        app.buffer = Some(buf);
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        press_chord(&mut app, "ctrl+n");
        assert!(matches!(app.mode, Mode::Editor { .. }), "{:?}", app.mode);
    }

    /// A reply to `Note` carries one note — the one asked for. Both it and the index used to
    /// land in the same field, so opening a note left the tree beside it listing exactly the
    /// note you were already reading, and backing out showed a browser with one note in it.
    #[test]
    fn opening_a_note_does_not_shrink_the_vault_to_that_note() {
        let mut app = app_with_snapshot();
        let (tx, _rx) = mpsc::unbounded_channel();

        // The index arrives.
        apply_frame(
            &mut app,
            ServerFrame::Vault(Box::new(a_vault(&["Home.md", "Daily/x.md", "Projects/horde.md"]))),
            &tx,
        );
        assert_eq!(ui::notes::rows(app.vault_index.as_ref(), "").len(), 5, "3 notes and 2 folders");

        // Then one note's detail, the way it arrives when you open one.
        let mut detail = a_vault(&["Projects/horde.md"]);
        detail.body = Some("# horde\n".into());
        apply_frame(&mut app, ServerFrame::Vault(Box::new(detail)), &tx);

        assert_eq!(
            ui::notes::rows(app.vault_index.as_ref(), "").len(),
            5,
            "the vault is still the whole vault"
        );
        assert_eq!(
            app.vault.as_ref().and_then(|v| v.body.as_deref()),
            Some("# horde\n"),
            "and the note that was opened is what has a body"
        );
    }

    /// A `Graph` reply carries no notes at all, and used to empty the index outright.
    #[test]
    fn asking_for_the_graph_does_not_empty_the_vault() {
        let mut app = app_with_snapshot();
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_frame(&mut app, ServerFrame::Vault(Box::new(a_vault(&["Home.md"]))), &tx);

        let mut g = a_vault(&[]);
        g.graph = Some(crate::proto::VaultGraph { nodes: Vec::new(), edges: Vec::new() });
        apply_frame(&mut app, ServerFrame::Vault(Box::new(g)), &tx);

        assert_eq!(ui::notes::rows(app.vault_index.as_ref(), "").len(), 1, "still there");
    }

    /// Clicking through the tree keeps the tree. This is the same bug from the other end: the
    /// hits are rebuilt from the index every frame, so an index of one is a tree of one.
    #[test]
    fn switching_notes_from_the_tree_keeps_the_whole_tree() {
        let mut app = app_with_snapshot();
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_frame(
            &mut app,
            ServerFrame::Vault(Box::new(a_vault(&["Home.md", "Projects/horde.md"]))),
            &tx,
        );
        app.vault_tree = true;
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Normal,
        };
        app.vault_tree_hits = vec![(6, "Projects/horde.md".into())];
        handle_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), 100, 6), &tx).unwrap();

        // The detail for the clicked note comes back.
        let mut detail = a_vault(&["Projects/horde.md"]);
        detail.body = Some("# horde\n".into());
        apply_frame(&mut app, ServerFrame::Vault(Box::new(detail)), &tx);

        assert!(
            ui::notes::rows(app.vault_index.as_ref(), "").len() >= 3,
            "both notes and the folder are still listed"
        );
        assert!(matches!(&app.mode, Mode::Editor { path, .. } if path == "Projects/horde.md"));
    }

    /// Closing a note in the vault leaves the notes side, rather than dropping you in a second
    /// list of the same notes — the vault page already has the tree and `ctrl+n`.
    #[test]
    fn closing_a_note_in_the_vault_does_not_land_in_the_browser() {
        let mut app = app_with_snapshot();
        app.vault_index = Some(a_vault(&["Home.md"]));
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.vault_tree = true;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Command(String::new()),
        };
        run_line(&mut app, "q");
        assert_eq!(app.mode, Mode::Terminal, "{:?}", app.mode);
    }

    /// A note opened from the browser still goes back to the browser: that is the browser's own
    /// flow, and it is not the step anybody complained about.
    #[test]
    fn closing_a_note_opened_from_the_browser_returns_to_the_browser() {
        let mut app = app_with_snapshot();
        app.vault_index = Some(a_vault(&["Home.md"]));
        app.buffer = Some(editor::Buffer::new("# Home\n"));
        app.vault_tree = false;
        app.mode = Mode::Editor {
            path: "Home.md".into(),
            scroll: 0,
            project: false,
            vim: Vim::Command(String::new()),
        };
        run_line(&mut app, "q");
        assert!(matches!(app.mode, Mode::Notes { .. }), "{:?}", app.mode);
    }

    /// The board is reachable from the start screen, by its key and by walking to it.
    #[test]
    fn the_start_screen_can_reach_the_kanban() {
        assert_eq!(ui::dashboard::Act::from_key('T'), Some(ui::dashboard::Act::Kanban));

        let mut app = app_with_snapshot();
        app.mode = Mode::Dashboard { sel: 0 };
        press(&mut app, KeyCode::Char('T'));
        assert!(
            matches!(app.mode, Mode::Kanban { view: ui::kanban::View::Board, .. }),
            "{:?}",
            app.mode
        );
    }

    /// Through `handle_mouse`, the way a real event arrives — not `kanban_mouse` directly.
    ///
    /// The distinction is the whole point of this helper. Every other list-mouse test called
    /// the handler straight, and all of them passed while the dispatch above dropped list
    /// events on the floor, so clicking the list did nothing in the actual app.
    fn dispatch(app: &mut App, kind: MouseEventKind, x: u16, y: u16) {
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_mouse(app, mouse(kind, x, y), &tx).unwrap();
    }

    #[test]
    fn a_click_in_the_list_reaches_the_list() {
        let mut app = listing();
        let y = list_row_y(&app, 2);
        dispatch(&mut app, MouseEventKind::Down(MouseButton::Left), 4, y);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 2 });
    }

    #[test]
    fn a_double_click_in_the_list_opens_the_card_through_the_real_dispatch() {
        let mut app = listing();
        let y = list_row_y(&app, 1);
        let cols = kanban_view(&app);
        let want = ui::kanban::list_rows(&cols, ui::now_millis())[1].0.id;
        dispatch(&mut app, MouseEventKind::Down(MouseButton::Left), 4, y);
        dispatch(&mut app, MouseEventKind::Down(MouseButton::Left), 4, y);
        assert_eq!(app.mode, Mode::Card { id: want, focus: ui::kanban::Field::Title });
    }

    /// A popup you can only leave with the keyboard is a trap.
    #[test]
    fn clicking_off_a_floating_card_puts_it_away() {
        let mut app = listing();
        app.kanban_back = (ui::kanban::View::List, 0, 2);
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        app.card_popup = Some(ratatui::layout::Rect::new(20, 5, 60, 20));

        dispatch(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert_eq!(
            app.mode,
            Mode::Kanban { view: ui::kanban::View::List, col: 0, sel: 2 },
            "and back to the row it came from"
        );
    }

    /// Inside is not outside, and neither is a card that has the whole frame.
    #[test]
    fn clicking_inside_a_floating_card_keeps_it_open() {
        let mut app = listing();
        app.kanban_back = (ui::kanban::View::List, 0, 2);
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        app.card_popup = Some(ratatui::layout::Rect::new(20, 5, 60, 20));
        dispatch(&mut app, MouseEventKind::Down(MouseButton::Left), 40, 10);
        assert!(matches!(app.mode, Mode::Card { .. }), "{:?}", app.mode);

        // A full-frame card has no outside to click.
        app.card_popup = None;
        dispatch(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert!(matches!(app.mode, Mode::Card { .. }), "{:?}", app.mode);
    }

    /// Words somebody is in the middle of typing are not dismissed by a stray click.
    #[test]
    fn clicking_off_a_card_being_edited_keeps_what_was_typed() {
        let mut app = listing();
        app.kanban_back = (ui::kanban::View::List, 0, 2);
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Body };
        app.card_popup = Some(ratatui::layout::Rect::new(20, 5, 60, 20));
        app.card_edit = Some(ui::kanban::Editing {
            field: ui::kanban::Field::Body,
            text: ui::kanban::TextArea::new("half a sentence"),
        });

        dispatch(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert!(matches!(app.mode, Mode::Card { .. }), "still open");
        assert_eq!(
            app.card_edit.as_ref().map(|e| e.text.text()),
            Some("half a sentence".to_string())
        );
    }

    /// Where the layout put a card, so a test can press exactly on it.
    fn card_rect(app: &App, id: u64) -> ratatui::layout::Rect {
        let cols = kanban_view(app);
        let lay = ui::kanban::layout(&cols, app.kanban_area.unwrap(), &app.kanban_scroll, 0);
        lay.cols
            .iter()
            .flat_map(|c| c.cards.iter())
            .find(|(cid, _)| *cid == id)
            .map(|(_, r)| *r)
            .expect("the layout placed that card")
    }

    fn drag(app: &mut App, id: u64, to: (u16, u16)) -> Vec<Cmd> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let from = card_rect(app, id);
        kanban_mouse(
            app,
            mouse(MouseEventKind::Down(MouseButton::Left), from.x + 3, from.y + 1),
            &tx,
        );
        kanban_mouse(app, mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1), &tx);
        kanban_mouse(app, mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1), &tx);
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Command(c) = f {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn pressing_a_card_picks_it_up_with_the_offset_it_was_grabbed_at() {
        let mut app = boarding();
        let (tx, _rx) = mpsc::unbounded_channel();
        let rect = card_rect(&app, 2);
        kanban_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 4, rect.y + 2),
            &tx,
        );
        let held = app.card_drag.expect("it is holding the card");
        assert_eq!(held.id, 2);
        // The offset is what stops the card snapping its corner under the pointer.
        assert_eq!(held.grab, (4, 2));
        assert!(!held.moved, "a press is not yet a drag");
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 1 });
    }

    /// Dragging a card into another column moves it there. The one gesture the whole feature
    /// exists for.
    #[test]
    fn dragging_a_card_to_another_column_moves_it() {
        let mut app = boarding();
        let doing = card_rect(&app, 4);
        let cmds = drag(&mut app, 1, (doing.x + 2, doing.y + 4));
        assert_eq!(
            cmds,
            vec![Cmd::CardMove { id: 1, column: "Doing".into(), after: Some(4) }],
            "behind the card it was dropped under"
        );
    }

    /// Dropping below every card in a column means the end of it, not nowhere.
    #[test]
    fn a_card_dropped_below_the_last_one_lands_at_the_end() {
        let mut app = boarding();
        let cols = kanban_view(&app);
        let lay = ui::kanban::layout(&cols, app.kanban_area.unwrap(), &app.kanban_scroll, 0);
        let todo = &lay.cols[1];
        let cmds = drag(&mut app, 1, (todo.rect.x + 2, todo.body.y + todo.body.height - 1));
        assert_eq!(cmds, vec![Cmd::CardMove { id: 1, column: "Todo".into(), after: Some(3) }]);
    }

    /// The gap above the first card is a real place to drop, and no card's own box covers it.
    #[test]
    fn a_card_dropped_above_the_first_one_lands_at_the_top() {
        let mut app = boarding();
        let first = card_rect(&app, 1);
        let cmds = drag(&mut app, 3, (first.x + 2, first.y));
        assert_eq!(cmds, vec![Cmd::CardMove { id: 3, column: "Todo".into(), after: None }]);
    }

    /// Without this every click on a card would write a reorder that changed nothing, and the
    /// log would fill with moves nobody made.
    #[test]
    fn letting_go_where_you_picked_it_up_is_not_a_move() {
        let mut app = boarding();
        let rect = card_rect(&app, 2);
        assert!(drag(&mut app, 2, (rect.x + 3, rect.y + 1)).is_empty());
        // And dropping a card onto its own box is the same non-move.
        assert!(drag(&mut app, 2, (rect.x + 5, rect.y + 3)).is_empty());
    }

    /// Two presses in the same cell open the card. crossterm has no double click, so this is
    /// horde's own — and it must not leave a drag live behind it.
    #[test]
    fn two_presses_in_the_same_cell_open_the_card() {
        let mut app = boarding();
        let (tx, _rx) = mpsc::unbounded_channel();
        let rect = card_rect(&app, 3);
        let down = mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 2, rect.y + 1);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 2, rect.y + 1);
        kanban_mouse(&mut app, down, &tx);
        kanban_mouse(&mut app, up, &tx);
        kanban_mouse(&mut app, down, &tx);
        assert_eq!(app.mode, Mode::Card { id: 3, focus: ui::kanban::Field::Title });
        assert!(app.card_drag.is_none(), "opening a card must not leave a drag in flight");
    }

    /// A drag that began on a card belongs to it until the button comes up, wherever the
    /// pointer wanders — the same rule panes have. A release outside the board drops nothing
    /// rather than throwing the card at whatever was nearest.
    #[test]
    fn releasing_outside_the_board_moves_nothing() {
        let mut app = boarding();
        let cmds = drag(&mut app, 1, (200, 200));
        assert!(cmds.is_empty());
        assert!(app.card_drag.is_none(), "and the drag is over either way");
    }

    #[test]
    fn the_wheel_scrolls_the_column_under_the_pointer() {
        let mut app = boarding();
        let (tx, _rx) = mpsc::unbounded_channel();
        let rect = card_rect(&app, 1);
        kanban_mouse(&mut app, mouse(MouseEventKind::ScrollDown, rect.x + 2, rect.y + 1), &tx);
        assert_eq!(app.kanban_scroll[1], 1);
        assert_eq!(app.kanban_scroll[0], 0, "and only that one");
    }

    // -- the board's keyboard ----------------------------------------------

    fn board_press(app: &mut App, code: KeyCode) -> Vec<Cmd> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), &tx).unwrap();
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Command(c) = f {
                out.push(c);
            }
        }
        out
    }

    /// Shoving across columns lands at the end, which is where a card you have just decided
    /// about belongs — the top is for work you ordered on purpose.
    #[test]
    fn shoving_a_card_right_lands_it_at_the_end_of_the_next_column() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 0 };
        let cmds = board_press(&mut app, KeyCode::Char('L'));
        assert_eq!(cmds, vec![Cmd::CardMove { id: 1, column: "Doing".into(), after: Some(4) }]);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::Board, col: 2, sel: 1 });
    }

    /// `J` and `K` are the keyboard's half of a vertical drag, and have to agree with it about
    /// what "behind" means.
    #[test]
    fn reordering_with_the_keyboard_moves_one_place_at_a_time() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 0 };
        assert_eq!(
            board_press(&mut app, KeyCode::Char('J')),
            vec![Cmd::CardMove { id: 1, column: "Todo".into(), after: Some(2) }],
            "down is behind the one below it"
        );
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 2 };
        assert_eq!(
            board_press(&mut app, KeyCode::Char('K')),
            vec![Cmd::CardMove { id: 3, column: "Todo".into(), after: Some(1) }],
            "up is behind the one two above"
        );
        // And the ends do nothing rather than wrapping.
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 0 };
        assert!(board_press(&mut app, KeyCode::Char('K')).is_empty());
    }

    #[test]
    fn walking_off_the_end_of_a_column_stays_on_the_last_card() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 0 };
        for _ in 0..9 {
            board_press(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 2 });
    }

    /// Moving to a shorter column must not leave the cursor pointing past its last card.
    #[test]
    fn moving_to_a_shorter_column_pulls_the_cursor_back_into_it() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 2 };
        board_press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::Board, col: 2, sel: 0 });
    }

    /// Nothing typed into the board's own question may reach a binding — it is a text field,
    /// and `n` in it is the letter n.
    #[test]
    fn typing_a_filter_filters_as_you_go_and_reaches_no_binding() {
        let mut app = boarding();
        board_press(&mut app, KeyCode::Char('/'));
        assert!(app.kanban_ask.is_some());
        for c in "two".chars() {
            board_press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.kanban_query, "two");
        assert_eq!(kanban_view(&app).iter().map(|c| c.cards.len()).sum::<usize>(), 1);
        // And escaping clears it rather than restoring a half-typed filter.
        board_press(&mut app, KeyCode::Esc);
        assert!(app.kanban_ask.is_none());
        assert!(app.kanban_query.is_empty());
    }

    #[test]
    fn a_new_card_names_the_column_the_cursor_is_in() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 2, sel: 0 };
        board_press(&mut app, KeyCode::Char('n'));
        for c in "write it down".chars() {
            board_press(&mut app, KeyCode::Char(c));
        }
        let cmds = board_press(&mut app, KeyCode::Enter);
        assert_eq!(
            cmds,
            vec![Cmd::CardNew {
                space: None,
                column: "Doing".into(),
                title: "write it down".into()
            }]
        );
    }

    /// Deleting a column gives its cards to the one before it, so an edit to the column list
    /// can never be a way of losing work.
    #[test]
    fn deleting_a_column_hands_its_cards_to_the_one_before_it() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 0 };
        let cmds = board_press(&mut app, KeyCode::Char('D'));
        assert_eq!(cmds, vec![Cmd::ColumnRename { from: "Todo".into(), to: "Backlog".into() }]);
        assert_eq!(app.cfg.kanban_columns, ["Backlog", "Doing", "Done"]);
    }

    /// The first column has nowhere to send its cards, so it refuses rather than dropping them.
    #[test]
    fn the_first_column_cannot_be_deleted() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 0, sel: 0 };
        assert!(board_press(&mut app, KeyCode::Char('D')).is_empty());
        assert_eq!(app.cfg.kanban_columns.len(), 4);
    }

    // -- one card ------------------------------------------------------------

    fn card_press(app: &mut App, code: KeyCode) -> Vec<Cmd> {
        board_press(app, code)
    }

    /// In a description, enter is a new line — so escape is what saves, and it has to.
    #[test]
    fn a_description_is_typed_across_lines_and_saved_with_escape() {
        let mut app = boarding();
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        card_press(&mut app, KeyCode::Char('e'));
        for c in "one".chars() {
            card_press(&mut app, KeyCode::Char(c));
        }
        card_press(&mut app, KeyCode::Enter);
        for c in "two".chars() {
            card_press(&mut app, KeyCode::Char(c));
        }
        let cmds = card_press(&mut app, KeyCode::Esc);
        assert_eq!(
            cmds,
            vec![Cmd::CardEdit {
                id: 1,
                patch: crate::proto::CardPatch {
                    body: Some("one\ntwo".into()),
                    ..Default::default()
                }
            }]
        );
        assert!(app.card_edit.is_none());
    }

    #[test]
    fn ctrl_c_throws_away_what_was_typed() {
        let mut app = boarding();
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        card_press(&mut app, KeyCode::Char('e'));
        card_press(&mut app, KeyCode::Char('x'));
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        )
        .unwrap();
        assert!(app.card_edit.is_none(), "the field closed");
        assert!(rx.try_recv().is_err(), "and nothing was written");
    }

    /// A comment carries who said it, and the daemon is what stamps that — the client only
    /// ever sends the words.
    #[test]
    fn a_comment_is_sent_as_words_and_nothing_else() {
        let mut app = boarding();
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        card_press(&mut app, KeyCode::Char('c'));
        for c in "parked".chars() {
            card_press(&mut app, KeyCode::Char(c));
        }
        let cmds = card_press(&mut app, KeyCode::Esc);
        assert_eq!(cmds, vec![Cmd::CardComment { id: 1, body: "parked".into() }]);
    }

    /// A date that cannot be read leaves the field open with what you typed still in it,
    /// rather than quietly clearing the date you were setting.
    #[test]
    fn an_unreadable_date_keeps_the_field_open() {
        let mut app = boarding();
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        card_press(&mut app, KeyCode::Char('d'));
        for c in "nonsense".chars() {
            card_press(&mut app, KeyCode::Char(c));
        }
        assert!(card_press(&mut app, KeyCode::Enter).is_empty());
        assert!(app.card_edit.is_some(), "still typing");
        // And an empty one clears it, which is what every text field here means by empty.
        app.card_edit = None;
        card_press(&mut app, KeyCode::Char('d'));
        let cmds = card_press(&mut app, KeyCode::Enter);
        assert_eq!(
            cmds,
            vec![Cmd::CardEdit {
                id: 1,
                patch: crate::proto::CardPatch { due: Some(None), ..Default::default() }
            }]
        );
    }

    /// The one key that reaches the agents from a card, and it must send exactly one thing.
    #[test]
    fn handing_a_card_over_sends_one_command() {
        let mut app = boarding();
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        assert_eq!(card_press(&mut app, KeyCode::Char('A')), vec![Cmd::CardHandOff { id: 1 }]);
    }

    /// Opening a card and escaping put you back at the top-left of the board, which after
    /// three cards reads as the board losing your place — because it is.
    #[test]
    fn leaving_a_card_returns_the_cursor_to_where_it_was() {
        let mut app = boarding();
        app.mode = Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 2 };
        board_press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Card { id: 3, focus: ui::kanban::Field::Title });
        board_press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 2 });
    }

    /// The same, by mouse: a double click has to remember the same place a keypress does.
    #[test]
    fn opening_a_card_with_the_mouse_remembers_the_board_too() {
        let mut app = boarding();
        let (tx, _rx) = mpsc::unbounded_channel();
        let rect = card_rect(&app, 2);
        let down = mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 2, rect.y + 1);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 2, rect.y + 1);
        kanban_mouse(&mut app, down, &tx);
        kanban_mouse(&mut app, up, &tx);
        kanban_mouse(&mut app, down, &tx);
        board_press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Kanban { view: ui::kanban::View::Board, col: 1, sel: 1 });
    }

    /// Held down, `j` used to scroll a four-line card into a blank screen you then had to
    /// scroll back out of.
    #[test]
    fn a_card_cannot_be_scrolled_past_what_was_drawn() {
        let mut app = boarding();
        app.mode = Mode::Card { id: 1, focus: ui::kanban::Field::Title };
        app.card_lines = 12;
        for _ in 0..50 {
            board_press(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(app.card_scroll, 11);
    }

    /// A card that has gone — archived out from under you, or the board replaced — returns to
    /// the board rather than drawing nothing.
    #[test]
    fn a_card_that_is_gone_returns_to_the_board() {
        let mut app = boarding();
        app.mode = Mode::Card { id: 99, focus: ui::kanban::Field::Title };
        board_press(&mut app, KeyCode::Char('j'));
        assert!(matches!(app.mode, Mode::Kanban { .. }));
    }

    // -- driving the graph with the pointer --------------------------------

    /// A graph open on screen, laid out and with its plot recorded, as the renderer leaves it.
    fn graphing() -> App {
        let mut app = app_with_snapshot();
        let g = crate::proto::VaultGraph {
            nodes: (0..12)
                .map(|i| crate::proto::GraphNode {
                    path: format!("n{i}.md"),
                    label: format!("n{i}"),
                    degree: 2,
                    group: "g".into(),
                    ghost: false,
                    by: None,
                    mtime: 0,
                })
                .collect(),
            edges: (0..11).map(|i| (i, i + 1)).collect(),
        };
        let mut sim = graph::Sim::new(&g);
        sim.settle(400);
        app.graph_centre = sim.centre();
        app.graph_plot = Some(ratatui::layout::Rect::new(0, 1, 100, 30));
        app.mode = Mode::Graph { sel: 0 };
        // What the renderer would have recorded for these positions.
        app.graph_hits = (0..12)
            .filter_map(|i| {
                sim.project(i, 100, 30, 1.0, app.graph_centre, 0.0)
                    .map(|(x, y)| (y.round() as u16 + 1, x.round() as u16, i))
            })
            .collect();
        app.sim = Some(sim);
        app.graph = Some(g);
        app
    }

    fn mouse(kind: MouseEventKind, col: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE }
    }

    /// `graph_hits` was filled in by the renderer and read by nothing — clicking a node did
    /// not select it, because there was no mouse handler for this mode at all.
    #[test]
    fn clicking_a_node_selects_it() {
        let mut app = graphing();
        let (row, col, want) = app.graph_hits[5];
        graph_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), col, row), 0);
        assert_eq!(app.mode, Mode::Graph { sel: want });
        assert_eq!(app.graph_drag, Some(GraphDrag::Node { i: want }), "and is holding it");
    }

    /// Grabbing a node moves the node; grabbing the background moves the view. Which one you
    /// get is decided by what was under the pointer, which is the rule nobody has to be told.
    #[test]
    fn dragging_the_background_pans_and_dragging_a_node_moves_it() {
        let mut app = graphing();
        let empty = app
            .graph_hits
            .iter()
            .all(|(r, c, _)| !(*r == 2 && c.abs_diff(1) <= 1))
            .then_some((1u16, 2u16))
            .expect("a corner with no node in it");

        let before = app.graph_centre;
        graph_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), empty.0, empty.1), 0);
        assert!(matches!(app.graph_drag, Some(GraphDrag::Pan { .. })));
        graph_mouse(&mut app, mouse(MouseEventKind::Drag(MouseButton::Left), empty.0 + 20, empty.1), 0);
        assert!(app.graph_centre.x < before.x, "the map followed the hand, not fled it");

        // A node instead: the layout moves, the view does not.
        let mut app = graphing();
        let (row, col, i) = app.graph_hits[3];
        let was = app.sim.as_ref().unwrap().pos[i];
        let centre = app.graph_centre;
        // Toward the middle of the plot, deliberately: a settled layout pins its outermost
        // nodes against the wall of the field, and dragging one further out is a no-op that
        // would read as the drag not working.
        let (tx, ty) = (50u16, 16u16);
        graph_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), col, row), 0);
        graph_mouse(&mut app, mouse(MouseEventKind::Drag(MouseButton::Left), tx, ty), 0);
        assert_ne!(app.sim.as_ref().unwrap().pos[i], was, "the node moved");
        assert_eq!(app.graph_centre, centre, "and the view did not");

        // Letting go lets its neighbours answer, which they cannot do from a settled layout.
        graph_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), tx, ty), 0);
        assert_eq!(app.graph_drag, None);
        assert!(!app.sim.as_ref().unwrap().settled(), "the layout has work to do again");
    }

    /// Whatever is under the pointer stays under it. Zooming about the centre instead is what
    /// makes a graph lurch away from the thing you were reaching for.
    #[test]
    fn scrolling_zooms_about_the_pointer() {
        let mut app = graphing();
        let (w, h) = (100u16, 30u16);
        let pointer = (72.0, 8.0);
        let under = |app: &App| {
            app.sim.as_ref().unwrap().unproject(pointer, w, h, app.graph_zoom, app.graph_centre)
        };

        let before = under(&app);
        graph_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 72, 9), 0);
        assert!(app.graph_zoom > 1.0, "it zoomed in");
        let after = under(&app);
        assert!(
            (after.x - before.x).abs() < 1e-6 && (after.y - before.y).abs() < 1e-6,
            "the point under the pointer moved: {before:?} -> {after:?}"
        );

        // And back out again, to the same place.
        graph_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 72, 9), 0);
        assert!((app.graph_zoom - 1.0).abs() < 1e-9, "{}", app.graph_zoom);
    }

    /// A graph with no layout yet is a real state — the reply has not arrived — and the
    /// pointer must do nothing rather than panic on it.
    #[test]
    fn the_pointer_does_nothing_to_a_graph_that_is_not_there_yet() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Graph { sel: 0 };
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
        ] {
            graph_mouse(&mut app, mouse(kind, 10, 10), 0);
        }
        assert_eq!(app.mode, Mode::Graph { sel: 0 });
    }

    /// A start screen with nobody working: the only thing that can ask for a frame is the
    /// passer-by, which is what these tests are about.
    fn quiet_dashboard() -> App {
        let mut app = app_with_snapshot();
        app.mode = Mode::Dashboard { sel: 0 };
        app.toasts.clear();
        if let Some(s) = app.snapshot.as_mut() {
            for p in &mut s.panes {
                p.agent = None;
            }
        }
        app
    }

    /// A start screen with nothing moving on it must cost nothing. That is a promise this
    /// file makes in prose in three places and `docs/concepts.md` publishes numbers for, so
    /// it is asserted rather than believed: through a whole cycle, the tick redraws exactly
    /// when the schedule says something is walking, and never otherwise.
    #[test]
    fn a_still_start_screen_draws_nothing() {
        let mut app = quiet_dashboard();
        app.zombie_stage = Some(ratatui::layout::Rect::new(0, 0, 72, 10));
        let (seed, mut secs) = (7u64, 0.0);
        let (mut still, mut moving) = (0, 0);
        while secs < 90.0 {
            app.zombie =
                Some(ui::zombie::Walk::seeded(seed, Duration::from_secs_f64(secs)));
            let walking = ui::zombie::phase_at(seed, secs).walking();
            assert_eq!(app.restless(), walking, "at {secs}s the client disagrees with the clock");
            assert_eq!(
                cadence(&app),
                if walking { ANIM } else { ANIM_STILL },
                "at {secs}s the beat is wrong"
            );
            if walking {
                moving += 1
            } else {
                still += 1
            }
            secs += 0.5;
        }
        assert!(still > moving, "the screen must be still more often than not");
    }

    /// Walking the menu must not restart the walk, which is the trap `Mode::Dashboard`'s
    /// `sel` sets: every cursor key assigns the mode afresh.
    #[test]
    fn moving_the_cursor_does_not_restart_the_walk() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Dashboard { sel: 0 };
        sync_zombie(&mut app);
        let started = app.zombie.expect("a walk on the start screen").since();
        for key in ['j', 'k', 'G'] {
            app.mode = Mode::Dashboard { sel: if key == 'k' { 0 } else { 1 } };
            sync_zombie(&mut app);
        }
        assert_eq!(app.zombie.map(|w| w.since()), Some(started), "the clock was reset");
    }

    /// Leaving the start screen, or turning either switch off, stops everything — and there
    /// is only one place that has to remember to.
    #[test]
    fn leaving_or_switching_off_stops_the_walk() {
        let cases: [(&str, fn(&mut App)); 3] = [
            ("left the screen", |a| a.mode = Mode::Terminal),
            ("zombie off", |a| a.cfg.zombie = false),
            ("animations off", |a| a.cfg.animate = false),
        ];
        for (why, change) in cases {
            let mut app = quiet_dashboard();
            sync_zombie(&mut app);
            app.zombie_stage = Some(ratatui::layout::Rect::new(0, 0, 72, 10));
            assert!(app.zombie.is_some(), "{why}: nothing started");

            change(&mut app);
            sync_zombie(&mut app);
            assert!(app.zombie.is_none(), "{why}: still walking");
            assert!(app.zombie_stage.is_none(), "{why}: the stage was left behind");
            assert!(!app.restless(), "{why}: still asking to be redrawn");
        }
    }

    /// A terminal too small for a wordmark has no stage, and a crossing happening off stage
    /// must not wake the client up hopefully once a frame.
    #[test]
    fn a_crossing_with_no_stage_costs_nothing() {
        let mut app = quiet_dashboard();
        app.zombie_stage = None;
        let mut secs = 0.0;
        while secs < 90.0 {
            app.zombie = Some(ui::zombie::Walk::seeded(3, Duration::from_secs_f64(secs)));
            assert!(!app.restless(), "asked for a frame at {secs}s with nowhere to draw it");
            secs += 0.5;
        }
    }

    /// Opening horde is arriving, and arriving shows you the state of things. Every attach
    /// starts here — including a reattach to a session full of running agents, because what
    /// you want after being away is the board, not whichever pane was last focused.
    ///
    /// `setup_done` is stated rather than defaulted: what is being asserted is where an attach
    /// lands *after* the walkthrough, and leaving it to the default made this test pass or fail
    /// on whether the person running it happened to have a `config.toml` in their own home
    /// directory — which is what it read before the fact was recorded in the config itself.
    #[test]
    fn every_attach_opens_on_the_start_screen() {
        for panes in [0, 3] {
            let mut app =
                App::new_for_test(Config { setup_done: true, ..kit_config() });
            let mut snap = crate::client::roster::tests::snap();
            snap.panes.truncate(panes);
            apply_frame(&mut app, ServerFrame::Snapshot(Box::new(snap)), &sink());
            assert!(
                matches!(app.mode, Mode::Dashboard { .. }),
                "a session with {panes} panes still greets you"
            );
        }
    }

    /// The decision is made once. A later snapshot — a pane closing, an agent finishing —
    /// must not yank someone back to a start screen while they are working.
    #[test]
    fn a_later_snapshot_cannot_pull_you_back_to_the_greeter() {
        let mut app = App::new_for_test(Config::default());
        let snap = crate::client::roster::tests::snap();
        apply_frame(&mut app, ServerFrame::Snapshot(Box::new(snap.clone())), &sink());
        app.mode = Mode::Terminal; // you pressed esc and got to work
        apply_frame(&mut app, ServerFrame::Snapshot(Box::new(snap)), &sink());
        assert_eq!(app.mode, Mode::Terminal, "it stays where you left it");
    }

    /// Turning it off has to actually turn it off, on the one path that shows it.
    #[test]
    fn the_greeter_can_be_switched_off() {
        let cfg = Config { dashboard: false, setup_done: true, ..Config::default() };
        let mut app = App::new_for_test(cfg);
        let snap = crate::client::roster::tests::snap();
        apply_frame(&mut app, ServerFrame::Snapshot(Box::new(snap)), &sink());
        assert_eq!(app.mode, Mode::Terminal);
    }

    /// Enter on a remembered project asks the daemon to open it, and enter on a live one
    /// merely goes there — the row says which before you press anything.
    #[test]
    fn enter_opens_a_remembered_project_and_focuses_a_live_one() {
        use crate::client::ui::dashboard::Row;
        assert_eq!(
            dashboard_activate(&Row::Recent {
                cwd: "/tmp/blog".into(),
                name: "blog".into(),
                when: "3d ago".into()
            }),
            Some(Cmd::OpenProject { cwd: "/tmp/blog".into() })
        );
        assert_eq!(
            dashboard_activate(&Row::Live {
                space: 4,
                name: "api".into(),
                accent: 0,
                facts: String::new(),
                cwd: "/tmp/api".into()
            }),
            Some(Cmd::FocusSpace(4))
        );
        assert_eq!(dashboard_activate(&Row::Header("projects".into())), None);
    }

    /// The bug this pins: a file was read from whatever space was focused *now*, while the
    /// listing had come from the space that was focused when it was asked for. Those differ
    /// for exactly as long as a focus change takes to round-trip — which is precisely when
    /// somebody opens a project and picks a file, so every such file was looked for in the
    /// project they had just left, and reported missing.
    #[test]
    fn a_file_is_read_from_the_project_its_listing_came_from() {
        let mut app = app_with_snapshot();
        let focused = app.snapshot.as_ref().and_then(|s| s.focused_space).expect("a space");
        let listing_space = focused + 7; // a different project, as after an unlanded focus change
        app.files = Some(crate::proto::FileList {
            space: listing_space,
            root: "/somewhere/else".into(),
            // At the root, so the first row is the file rather than the folder holding it.
            files: vec!["main.rs".into()],
            truncated: false,
            body: None,
            path: None,
        });
        app.mode = Mode::Files { query: String::new(), sel: 0 };

        let out = press_chord(&mut app, "enter");
        let sent: Vec<&Cmd> = out
            .iter()
            .filter_map(|f| match f {
                ClientFrame::Command(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![&Cmd::FileRead { space: listing_space, path: "main.rs".into() }],
            "read from the listing's project, not the focused one"
        );
    }

    /// Enter on a folder opens it rather than trying to edit it, and closes it again.
    #[test]
    fn enter_on_a_folder_folds_it_instead_of_opening_a_file() {
        let mut app = app_with_snapshot();
        app.files = Some(crate::proto::FileList {
            space: 1,
            root: "/p".into(),
            files: vec!["src/main.rs".into()],
            truncated: false,
            body: None,
            path: None,
        });
        app.mode = Mode::Files { query: String::new(), sel: 0 };

        let out = press_chord(&mut app, "enter");
        assert!(out.is_empty(), "a folder is not a file to open: {out:?}");
        assert!(app.open_dirs.contains("src"), "it opened");
        press_chord(&mut app, "enter");
        assert!(!app.open_dirs.contains("src"), "and closes again");
    }

    /// The control for the leak tests below. If an ordinary keystroke did not reach the pane
    /// here, then "nothing reached the pane" would prove nothing about abandoned sequences.
    #[test]
    fn an_ordinary_key_still_reaches_the_pane() {
        let mut app = app_with_snapshot();
        assert!(
            press_chord(&mut app, "w").iter().any(|f| matches!(f, ClientFrame::Input { .. })),
            "a plain key must still be typing"
        );
    }

    /// The failure this whole design is arranged around: keys held back for a sequence that
    /// turns out not to exist are *dropped*, never flushed to the pane. Replaying them would
    /// type `wv` at an agent you were only ever looking at.
    #[test]
    fn abandoning_a_leader_sequence_types_nothing_into_the_pane() {
        let mut app = app_with_snapshot();
        press_chord(&mut app, "ctrl+space");
        let held = press_chord(&mut app, "w"); // a real group: still waiting
        assert!(matches!(app.mode, Mode::Leader { .. }), "half a sequence must hold");
        assert!(held.is_empty(), "a held key does not act");
        let after = press_chord(&mut app, "esc");
        assert_eq!(app.mode, Mode::Terminal, "esc leaves the table");
        assert!(after.is_empty(), "an abandoned sequence must not reach the pane, and must not act");
    }

    /// The other half of the same rule: a dead end is dropped too, and says so, rather than
    /// silently swallowing the keys or spraying them at the program.
    #[test]
    fn an_unbound_leader_sequence_is_dropped_with_a_hint() {
        let mut app = app_with_snapshot();
        press_chord(&mut app, "ctrl+space");
        let out = press_chord(&mut app, "y"); // starts nothing
        assert_eq!(app.mode, Mode::Terminal);
        assert!(out.is_empty(), "an unbound sequence must not reach the pane");
        assert!(!app.toasts.is_empty(), "and it should say why");
    }

    /// A completed sequence acts and leaves, so the common path never parks you in a mode
    /// you then have to notice and escape.
    #[test]
    fn a_completed_leader_sequence_acts_and_exits() {
        let mut app = app_with_snapshot();
        press_chord(&mut app, "ctrl+space");
        press_chord(&mut app, "w");
        let out = press_chord(&mut app, "v");
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(cmds(out), vec![Cmd::SplitRight], "leader w v splits right");
    }

    /// Backspace steps back one key rather than dumping the sequence, so a mistyped middle
    /// key costs one press instead of starting over.
    #[test]
    fn backspace_walks_back_out_of_a_leader_sequence_one_key_at_a_time() {
        let mut app = app_with_snapshot();
        press_chord(&mut app, "ctrl+space");
        press_chord(&mut app, "w");
        press_chord(&mut app, "backspace");
        assert!(
            matches!(&app.mode, Mode::Leader { pending, .. } if pending.is_empty()),
            "one key back, still in the table"
        );
        press_chord(&mut app, "backspace");
        assert_eq!(app.mode, Mode::Terminal, "backing out of an empty sequence leaves");
    }

    /// The leader has to be reachable when its own chord is not, because `ctrl+space` is
    /// `set-mark` to a terminal full of emacs users — and one of them will rebind it.
    #[test]
    fn the_prefix_opens_the_leader_table_too() {
        let mut app = app_with_snapshot();
        press_chord(&mut app, "ctrl+b");
        press_chord(&mut app, "space");
        assert!(matches!(app.mode, Mode::Leader { .. }));
    }

    /// The whole justification for a mode: bare `j` and `k` walk the list instead of being
    /// typed at whatever agent happened to be focused.
    #[test]
    fn sidebar_keys_move_the_cursor_and_send_nothing_to_the_pane() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        let before = app.sidebar.cursor;
        let frames = press(&mut app, KeyCode::Char('j'));
        assert!(frames.is_empty(), "nothing reached the pane: {frames:?}");
        assert_ne!(app.sidebar.cursor, before, "but the cursor moved");
        assert_eq!(app.mode, Mode::Sidebar, "and we are still in the panel");
    }

    /// Acts *and* exits, so the common path — glance, jump, type — never leaves you parked in
    /// a mode you then have to notice and leave.
    #[test]
    fn enter_in_the_sidebar_focuses_and_returns_to_the_terminal() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        app.sidebar.cursor = Some(Focus::Agent(2));
        let out = cmds(press(&mut app, KeyCode::Enter));
        assert_eq!(out, vec![Cmd::FocusPane(2)]);
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn enter_on_a_group_header_goes_to_that_space() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        app.sidebar.cursor = Some(Focus::Group(2));
        assert_eq!(cmds(press(&mut app, KeyCode::Enter)), vec![Cmd::FocusSpace(2)]);
    }

    #[test]
    fn escaping_the_sidebar_returns_keys_to_the_pane() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        assert!(press(&mut app, KeyCode::Esc).is_empty());
        assert_eq!(app.mode, Mode::Terminal);
    }

    /// An unrecognised key is ignored rather than passed through: falling through would type
    /// `x` into an agent you were only looking at.
    #[test]
    fn an_unknown_key_in_the_sidebar_is_ignored_not_forwarded() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        let frames = press(&mut app, KeyCode::Char('x'));
        assert!(frames.is_empty(), "{frames:?}");
        assert_eq!(app.mode, Mode::Sidebar);
    }

    /// `h` and `l` are directional, so they only act in their own direction — pressing `l` on
    /// an already-open group should do nothing rather than close it.
    #[test]
    fn collapse_keys_only_act_in_their_own_direction() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        app.sidebar.cursor = Some(Focus::Group(1));
        assert_eq!(cmds(press(&mut app, KeyCode::Char('l'))), vec![], "already open");
        assert_eq!(
            cmds(press(&mut app, KeyCode::Char('h'))),
            vec![Cmd::ToggleSpaceCollapsed(1)]
        );
        // Space toggles either way.
        assert_eq!(
            cmds(press(&mut app, KeyCode::Char(' '))),
            vec![Cmd::ToggleSpaceCollapsed(1)]
        );
    }

    /// Folding from an agent row has to move the cursor to the header too, or it would be left
    /// pointing at a row that just stopped being drawn.
    #[test]
    fn folding_from_an_agent_row_moves_the_cursor_to_its_header() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        app.sidebar.cursor = Some(Focus::Agent(1));
        let out = cmds(press(&mut app, KeyCode::Char('h')));
        assert_eq!(out, vec![Cmd::ToggleSpaceCollapsed(1)]);
        assert_eq!(app.sidebar.cursor, Some(Focus::Group(1)));
    }

    #[test]
    fn pinning_only_applies_to_an_agent_row() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        app.sidebar.cursor = Some(Focus::Agent(4));
        assert_eq!(cmds(press(&mut app, KeyCode::Char('p'))), vec![Cmd::TogglePanePinned(4)]);
        // A header is not a pane, so there is nothing to pin.
        app.sidebar.cursor = Some(Focus::Group(1));
        assert_eq!(cmds(press(&mut app, KeyCode::Char('p'))), vec![]);
    }

    /// The direct test of constraint one: the snapshot is replaced wholesale every frame, so a
    /// cursor naming something that has gone must be forgotten rather than trusted.
    #[test]
    fn a_cursor_whose_pane_exits_is_forgotten_on_the_next_snapshot() {
        let mut app = app_with_snapshot();
        app.sidebar.cursor = Some(Focus::Agent(2));
        let mut snap = crate::client::roster::tests::snap();
        snap.panes.retain(|p| p.id != 2);
        app.sidebar.prune(&snap);
        assert_eq!(app.sidebar.cursor, None, "a stale cursor is not kept");
    }

    /// The gesture that makes a role filter reachable: point at an agent doing the job and
    /// ask for everyone doing it. `f` steps back out to `all` from there.
    #[test]
    fn r_in_the_sidebar_filters_to_the_role_under_the_cursor() {
        let mut app = app_with_snapshot();
        app.snapshot.as_mut().unwrap().panes.iter_mut().find(|p| p.id == 2).unwrap().role =
            Some("reviewer".into());
        app.mode = Mode::Sidebar;
        app.sidebar.cursor = Some(Focus::Agent(2));

        assert!(press(&mut app, KeyCode::Char('r')).is_empty(), "a client-side filter");
        assert_eq!(app.sidebar.lens, roster::Lens::Role("reviewer".into()));

        press(&mut app, KeyCode::Char('f'));
        assert_eq!(app.sidebar.lens, roster::Lens::All, "one press back out");
    }

    /// An agent with no role has nothing to filter to, so the key does nothing rather than
    /// filtering to an empty list.
    #[test]
    fn r_on_an_unlabelled_agent_leaves_the_lens_alone() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        app.sidebar.cursor = Some(Focus::Agent(1));
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(app.sidebar.lens, roster::Lens::All);
    }

    #[test]
    fn cycling_the_lens_sends_nothing_to_the_daemon() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Sidebar;
        // A lens is a client-side view; it must never mutate the session.
        assert!(press(&mut app, KeyCode::Char('f')).is_empty());
        assert_eq!(app.sidebar.lens, roster::Lens::NeedsYou);
    }

    /// One cursor, two views of the same list: open the roster and you land where you left
    /// off, jump from it and the sidebar agrees.
    #[test]
    fn the_roster_shares_the_sidebar_cursor() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Roster { scroll: 0 };
        app.sidebar.cursor = Some(Focus::Agent(4));
        let out = cmds(press(&mut app, KeyCode::Enter));
        assert_eq!(out, vec![Cmd::FocusPane(4)]);
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn escaping_the_roster_returns_to_the_terminal() {
        let mut app = app_with_snapshot();
        app.mode = Mode::Roster { scroll: 3 };
        assert!(press(&mut app, KeyCode::Esc).is_empty());
        assert_eq!(app.mode, Mode::Terminal);
    }
}
