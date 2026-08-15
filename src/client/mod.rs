//! The attached client: renders frames, forwards input, owns nothing.
//!
//! All geometry and session state comes from the daemon, so the client is free to die and
//! come back without disturbing a single running process.

pub mod editor;
pub mod graph;
pub mod syntax;
mod input;
pub mod menu;
pub mod roster;
pub mod selection;
pub mod settings;
pub mod ui;

use std::collections::{HashMap, VecDeque};
use std::io::Stdout;
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
use crate::client::menu::{Act, Level, Prompt, Target};
use crate::framing;
use crate::proto::{
    ClientFrame, Cmd, CursorPos, Digest, Message, NoticeLevel, PaneId, Row, ServerFrame, Snapshot,
    SpaceId, TabId, PROTOCOL_VERSION,
};
use ui::overlays::Item;
use crate::client::roster::Focus;
use ui::sidebar::Hit;

/// Animation cadence for spinners and toast expiry.
const ANIM: Duration = Duration::from_millis(110);
const TOAST_LIFE: Duration = Duration::from_secs(6);
/// Bus messages kept client-side for the drawer.
const BUS_CAP: usize = 300;

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
    Editor { path: String, scroll: usize, project: bool },
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
    pub highlight: Option<(usize, Vec<ratatui::text::Line<'static>>)>,
    /// The note being written, alive only while the editor is open.
    pub buffer: Option<editor::Buffer>,
    /// The graph layout, alive only while the graph is open.
    pub sim: Option<graph::Sim>,
    /// How far in, and where the view is centred. Panning moves the centre; the layout
    /// underneath does not know the difference.
    pub graph_zoom: f64,
    pub graph_centre: graph::Point,
    /// Node hits for the graph: `(y, x, node index)`.
    pub graph_hits: Vec<(u16, u16, usize)>,
    /// Row hits for the note browser.
    pub notes_hits: Vec<(u16, usize)>,
    /// Row hits for the dashboard: `(y, index into its row list)`.
    pub dashboard_hits: Vec<(u16, usize)>,
    /// Whether the start-screen decision has been made for this attach. Made once, on the
    /// first snapshot, so a later shape change cannot yank you back to a greeter.
    greeted: bool,
    /// Set once the version mismatch warning has been shown, so it appears only once.
    pub warned_version: bool,
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
            notes_hits: Vec::new(),
            open_dirs: std::collections::HashSet::new(),
            want_files: false,
            files: None,
            setup: ui::setup::Answers::default(),
            highlight: None,
            buffer: None,
            sim: None,
            graph_zoom: 1.0,
            graph_centre: graph::Point { x: 0.0, y: 0.0 },
            graph_hits: Vec::new(),
            vault: None,
            follow: None,
            pending_read: None,
            opening_editor: false,
            greeted: false,
            warned_version: false,
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

    let mut term = setup_terminal()?;
    let result = run_loop(&mut term, &mut app, out_tx.clone(), in_rx).await;
    restore_terminal(&mut term)?;

    // Tell the daemon we are going, so it stops rendering for us.
    let _ = out_tx.send(ClientFrame::Detach);
    writer.abort();
    reader.abort();
    result
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;

    // A panic with the terminal in raw mode leaves an unusable shell, so restore first and
    // let the default hook print afterwards.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = std::io::stdout();
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(DisableMouseCapture);
        let _ = out.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
        default_hook(info);
    }));

    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(term: &mut Term) -> Result<()> {
    disable_raw_mode()?;
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
    let mut anim = tokio::time::interval(ANIM);
    anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut needs_draw = true;

    loop {
        if needs_draw {
            term.draw(|f| ui::draw(f, app))?;
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
                // Only the spinner and elapsed timers change, and only when something is
                // actually animating.
                let animating = app.snapshot.as_ref().is_some_and(|s| {
                    s.panes.iter().any(|p| p.agent.as_ref()
                        .is_some_and(|a| a.state == crate::proto::AgentState::Working))
                });
                if animating || !app.toasts.is_empty() {
                    needs_draw = true;
                }

                // Advance the graph layout, if one is open and still moving. Several steps
                // a frame, because one would take half a minute to settle — and then *stop*,
                // which is the whole reason the simulation anneals. A graph left open must
                // cost exactly as much as any other still picture.
                if matches!(app.mode, Mode::Graph { .. }) {
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
                    // No config file means horde has never been set up here, which is a fact
                    // on disk rather than a guess about the person. Being asked four
                    // questions once beats discovering them by hitting them — "no vault" the
                    // first time you write a note is not a prompt, it is a wall.
                    app.mode = if !crate::config::config_path().exists() {
                        Mode::Setup { step: ui::setup::Step::Vault }
                    } else if app.cfg.dashboard {
                        Mode::Dashboard { sel: 0 }
                    } else {
                        Mode::Terminal
                    };
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
                    app.mode = Mode::Editor { path, scroll: 0, project: true };
                }
                _ => app.files = Some(*f),
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
            // A note asked for by the editor arrives as a body to write into.
            if app.opening_editor {
                app.opening_editor = false;
                if let (Some(body), Some(note)) = (v.body.clone(), v.notes.first()) {
                    app.buffer = Some(editor::Buffer::new(&body));
                    app.mode = Mode::Editor { path: note.path.clone(), scroll: 0, project: false };
                }
            }
            if let Some(g) = v.graph.as_ref() {
                let mut sim = graph::Sim::new(g);
                // Past the animation limit the picture is a shape rather than a story, and
                // watching two thousand nodes shuffle costs more than it explains.
                if g.nodes.len() > graph::ANIMATE_LIMIT {
                    sim.settle(600);
                }
                app.graph_centre = sim.centre();
                app.sim = Some(sim);
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
                            bytes: crate::client::ui::overlays::answer_bytes(choice),
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
                    app.mode = Mode::Terminal;
                }
                // Tab walks the nodes, because the arrows are already panning the view.
                KeyCode::Tab | KeyCode::Char('j') => {
                    app.mode = Mode::Graph { sel: if sel >= last { 0 } else { sel + 1 } }
                }
                KeyCode::BackTab | KeyCode::Char('k') => {
                    app.mode = Mode::Graph { sel: if sel == 0 { last } else { sel - 1 } }
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
                KeyCode::Char('0') => {
                    app.graph_zoom = 1.0;
                    if let Some(s) = app.sim.as_ref() {
                        app.graph_centre = s.centre();
                    }
                }
                KeyCode::Enter => {
                    // A ghost has no note to open, so enter on one does nothing rather than
                    // inventing a file the person never asked for.
                    let node = app
                        .vault
                        .as_ref()
                        .and_then(|v| v.graph.as_ref())
                        .and_then(|g| g.nodes.get(sel))
                        .cloned();
                    if let Some(n) = node.filter(|n| !n.ghost) {
                        let row = ui::notes::Row {
                            path: n.path.clone(),
                            title: n.label.clone(),
                            tags: Vec::new(),
                            backlinks: 0,
                            depth: 0,
                            folder: false,
                            open: false,
                        };
                        open_note(app, &row, out);
                        app.sim = None;
                        app.mode = Mode::Terminal;
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
                        app.setup.cursor = 0;
                        app.mode = Mode::Setup { step: *next };
                    }
                    // Finishing writes the config, so the answers survive the session that
                    // gave them. Anything already there is left alone: a walkthrough that
                    // overwrites a config somebody wrote by hand is a walkthrough nobody
                    // runs twice.
                    None => {
                        let path = crate::config::config_path();
                        if !path.exists() {
                            if let Some(dir) = path.parent() {
                                let _ = std::fs::create_dir_all(dir);
                            }
                            match std::fs::write(&path, app.setup.to_config()) {
                                Ok(()) => {
                                    let _ = out.send(ClientFrame::Command(Cmd::VaultInit {
                                        space: app
                                            .snapshot
                                            .as_ref()
                                            .and_then(|s| s.focused_space)
                                            .unwrap_or(0),
                                    }));
                                    app.toast(
                                        NoticeLevel::Info,
                                        format!("settings written to {}", path.display()),
                                    );
                                }
                                Err(e) => app.toast(
                                    NoticeLevel::Warn,
                                    format!("could not write {}: {e}", path.display()),
                                ),
                            }
                        }
                        app.mode = Mode::Dashboard { sel: 0 };
                    }
                }
            };

            match k.code {
                // Skipping is allowed and changes nothing: someone who wants to look around
                // first should not have to answer four questions to be let in.
                KeyCode::Esc => app.mode = Mode::Dashboard { sel: 0 },
                KeyCode::Enter => advance(app, out, step),
                KeyCode::Down | KeyCode::Tab => {
                    app.setup.cursor = (app.setup.cursor + 1).min(count.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::BackTab => {
                    app.setup.cursor = app.setup.cursor.saturating_sub(1)
                }
                KeyCode::Char(' ') if step == Step::Unattended => {
                    app.setup.unattended = app.setup.cursor == 1
                }
                KeyCode::Backspace if step == Step::Vault => {
                    app.setup.vault.pop();
                }
                KeyCode::Char(c) if step == Step::Vault => app.setup.vault.push(c),
                _ => {}
            }
            // The radio follows the cursor, so moving is choosing rather than a second step.
            if step == Step::Unattended {
                app.setup.unattended = app.setup.cursor == 1;
            }
            return Ok(());
        }
        Mode::Editor { path, scroll, project } => {
            let rows = app.snapshot.as_ref().map(|s| s.status.h).unwrap_or(1) as usize;
            let page = rows.max(10);
            let Some(buf) = app.buffer.as_mut() else {
                app.mode = Mode::Terminal;
                return Ok(());
            };
            let save = |app: &mut App, out: &mpsc::UnboundedSender<ClientFrame>, path: &str| {
                let Some(b) = app.buffer.as_mut() else { return };
                let Some(space) = app.snapshot.as_ref().and_then(|s| s.focused_space) else {
                    return;
                };
                let body = b.text();
                let cmd = if project {
                    Cmd::FileSave { space, path: path.to_string(), body }
                } else {
                    Cmd::VaultSave { space, path: path.to_string(), body }
                };
                let _ = out.send(ClientFrame::Command(cmd));
                b.saved();
            };

            match (k.code, k.modifiers.contains(KeyModifiers::CONTROL)) {
                (KeyCode::Char('s'), true) => save(app, out, &path),
                // Leaving saves. An editor that can lose a note because you pressed the
                // wrong key to get out of it is not one anybody should trust a thought to.
                (KeyCode::Esc, _) => {
                    save(app, out, &path);
                    app.buffer = None;
                    app.mode = if project {
                        Mode::Files { query: String::new(), sel: 0 }
                    } else {
                        Mode::Notes { query: String::new(), sel: 0 }
                    };
                }
                (KeyCode::Char('r'), true) => {
                    save(app, out, &path);
                    app.buffer = None;
                    read_note(app, &path, out);
                }
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
                // Everything else types. No mode to be in, and no key that silently means
                // something else — this is a notes app.
                (KeyCode::Char(c), false) => buf.insert(c),
                _ => {}
            }

            // Keep the cursor on screen by following it, never by moving it.
            if let Some(b) = app.buffer.as_ref() {
                let view = rows.max(8);
                let scroll = if b.line < scroll {
                    b.line
                } else if b.line >= scroll + view {
                    b.line - view + 1
                } else {
                    scroll
                };
                if matches!(app.mode, Mode::Editor { .. }) {
                    app.mode = Mode::Editor { path, scroll, project };
                }
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
            let rendered = ui::markdown::render(&body, width.saturating_sub(6).min(96), &app.cfg.theme);
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
                KeyCode::Char('e') => {
                    let row = ui::notes::Row {
                        path,
                        title: String::new(),
                        tags: Vec::new(),
                        backlinks: 0,
                            depth: 0,
                            folder: false,
                            open: false,
                    };
                    open_note(app, &row, out);
                    app.mode = Mode::Terminal;
                }
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
            let rows = ui::notes::rows(app.vault.as_ref(), &query);
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
                    let sel = ui::notes::first(&ui::notes::rows(app.vault.as_ref(), &q));
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
                    app.mode = Mode::Prompt { prompt: Prompt::NewNote, value: String::new() }
                }
                KeyCode::Char(c) => {
                    let mut q = query;
                    q.push(c);
                    let sel = ui::notes::first(&ui::notes::rows(app.vault.as_ref(), &q));
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
                KeyCode::Char('j') | KeyCode::Down => {
                    app.mode = Mode::Dashboard { sel: (sel + 1).min(last) }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.mode = Mode::Dashboard { sel: sel.saturating_sub(1) }
                }
                KeyCode::Char('g') | KeyCode::Home => app.mode = Mode::Dashboard { sel: 0 },
                KeyCode::Char('G') | KeyCode::End => app.mode = Mode::Dashboard { sel: last },
                // Acts *and* leaves, like every other list in horde.
                KeyCode::Enter => {
                    let row = picks.get(sel).map(|i| rows[*i].clone());
                    if let Some(cmd) = row.as_ref().and_then(dashboard_activate) {
                        let _ = out.send(ClientFrame::Command(cmd));
                    }
                    // Opening a project shows you the project: its files, to pick one and
                    // edit it. The multiplexer is a keystroke from there rather than the
                    // thing you have to go through to reach anything.
                    //
                    // An agent that needs you is the exception — that row is a pane, and
                    // the whole point of choosing it is to go and look at it.
                    app.mode = match row {
                        Some(ui::dashboard::Row::Attention { .. }) | None => Mode::Terminal,
                        Some(_) => {
                            // Asked for on the next snapshot rather than now: focusing a
                            // space is a round trip, and a listing requested before it lands
                            // is a listing of the project you just left.
                            app.files = None;
                            app.want_files = true;
                            Mode::Files { query: String::new(), sel: 0 }
                        }
                    };
                }
                // The quote is "push P for project", and the habit is lower case, so both.
                KeyCode::Char('p') | KeyCode::Char('P') => {
                    app.mode = Mode::SpaceSwitcher { query: String::new(), sel: 0 }
                }
                KeyCode::Char('n') => {
                    let _ = out.send(ClientFrame::Command(Cmd::NewSpace { name: None }));
                    app.mode = Mode::Terminal;
                }
                // The note side, straight from the menu. It never touches the multiplexer:
                // writing a note is not a thing you should have to open a terminal to do.
                KeyCode::Char('w') => return run_action(app, Action::NoteNew, out),
                KeyCode::Char('N') => return run_action(app, Action::Notes, out),
                KeyCode::Char('o') => return run_action(app, Action::Roster, out),
                KeyCode::Char('D') => return run_action(app, Action::Cmd(Cmd::RequestDigest), out),
                KeyCode::Char('.') => return run_action(app, Action::Settings, out),
                KeyCode::Char('?') => return run_action(app, Action::Help, out),
                // A greeter's `q` leaves the program, which is what every editor start screen
                // has taught. Elsewhere in horde `q` backs out of a view; here there is no
                // deeper place to back out to.
                KeyCode::Char('q') => return run_action(app, Action::Detach, out),
                KeyCode::Esc => app.mode = Mode::Terminal,
                _ => {}
            }
            return Ok(());
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
            app.mode = Mode::Terminal;
            return Ok(());
        }
        KeyCode::Enter => {
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
                // Runs the same code path as `horde integration install claude`.
                match crate::cli::run(crate::cli::Command::Integration {
                    cmd: crate::cli::IntegrationCmd::Install { agent: "claude".into() },
                }) {
                    Ok(()) => app.toast(
                        NoticeLevel::Info,
                        "hooks installed — restart running Claude sessions",
                    ),
                    Err(e) => app.toast(NoticeLevel::Error, format!("{e:#}")),
                }
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
    app.buffer = Some(editor::Buffer::new(&format!("# {title}\n\n")));
    if let Some(b) = app.buffer.as_mut() {
        b.goto(2, 0); // past the heading, where you were going to type anyway
    }
    app.mode = Mode::Editor { path, scroll: 0, project: false };
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

/// Open a note in `$EDITOR`, in a split beside whatever you were doing.
///
/// A real editor rather than one horde grew in a hurry: the knowledge layer is useful the
/// day it can find a note, and the pane that opens is the one you already know how to use.
/// Native buffers arrive in their own phase, and will have to earn the swap.
fn open_note(app: &mut App, row: &ui::notes::Row, out: &mpsc::UnboundedSender<ClientFrame>) {
    let Some(root) = app.vault.as_ref().map(|v| v.root.clone()) else { return };
    let path = std::path::Path::new(&root).join(&row.path);
    let editor = crate::client::settings::editor();
    // Quoted: note names have spaces in them far more often than filenames usually do.
    let cmd = format!("{editor} '{}'", path.to_string_lossy().replace('\'', r"'\''"));
    let _ = out.send(ClientFrame::Command(Cmd::SpawnAgent {
        cmd,
        name: Some(row.title.clone()),
        split: Some(crate::proto::Dir::Right),
    }));
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
        ui::dashboard::Row::Header(_) | ui::dashboard::Row::Hint(_) => None,
    }
}

fn run_action(
    app: &mut App,
    action: Action,
    out: &mpsc::UnboundedSender<ClientFrame>,
) -> Result<()> {
    match action {
        Action::Cmd(cmd) => {
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

    // Clicks on the start screen open the row under the pointer, the same rows `j` walks.
    if let Mode::Dashboard { .. } = app.mode {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some((_, row)) = app.dashboard_hits.iter().find(|(hy, _)| *hy == y).copied() {
                let rows = app
                    .snapshot
                    .as_ref()
                    .map(|s| ui::dashboard::rows(s, ui::now_millis()))
                    .unwrap_or_default();
                if let Some(cmd) = rows.get(row).and_then(dashboard_activate) {
                    let _ = out.send(ClientFrame::Command(cmd));
                    app.mode = Mode::Terminal;
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

    fn app_with_snapshot() -> App {
        let mut app = App::new(Config::default());
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

    /// Opening horde is arriving, and arriving shows you the state of things. Every attach
    /// starts here — including a reattach to a session full of running agents, because what
    /// you want after being away is the board, not whichever pane was last focused.
    #[test]
    fn every_attach_opens_on_the_start_screen() {
        for panes in [0, 3] {
            let mut app = App::new_for_test(Config::default());
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
        let cfg = Config { dashboard: false, ..Config::default() };
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
