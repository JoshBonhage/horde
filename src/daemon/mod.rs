//! The daemon: owns every PTY, every emulator, and the session shape.
//!
//! One task owns the `Session` outright and everything reaches it through a channel. That
//! avoids holding a mutex across awaits, and makes the tick loop the only place panes are
//! pumped — so pane damage is consumed exactly once per frame.

pub mod agents;
pub mod bus;
pub mod digest;
pub mod handoff;
pub mod journal;
pub mod layout;
pub mod logfile;
pub mod manifest;
pub mod notify;
pub mod pane;
pub mod persist;
pub mod pty;
pub mod rpc;
pub mod state;
pub mod tasks;
pub mod triggers;
pub mod upgrade;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};

use crate::config::{socket_path, Config};
use crate::framing;
use crate::proto::{
    ClientFrame, Cmd, CursorPos, Dir, Event, NoticeLevel, PaneId, Request, Response, RowUpdate,
    ServerFrame, PROTOCOL_VERSION,
};
use state::Session;

/// Render cadence while a client is attached. 16ms coalesces output bursts into one frame
/// without visible lag.
const TICK_ATTACHED: Duration = Duration::from_millis(16);
/// Cadence with nobody watching. There are no frames to draw, so the only work left is
/// draining pty output into the emulators and running detection — and waking 60 times a
/// second to do that costs real battery for no benefit. Pty reads happen on their own
/// threads into unbounded channels, so nothing stalls in between.
const TICK_DETACHED: Duration = Duration::from_millis(150);
/// How often to look at what each agent is doing. Probing the foreground process shells out,
/// so it runs far less often than rendering. Timed rather than counted in ticks, so it is
/// unaffected by the cadence switching above.
const DETECT_INTERVAL: Duration = Duration::from_millis(640);
/// Quiet period before the session shape is written to disk.
const SAVE_DELAY: Duration = Duration::from_millis(1000);
/// Quiet period after the last resize before programs are told to redraw.
///
/// Long enough that dragging a window edge counts as one gesture rather than forty, short enough
/// that letting go feels immediate.
const RESIZE_SETTLE: Duration = Duration::from_millis(120);

type ClientId = u64;

pub enum DaemonMsg {
    Rpc { req: Request, reply: oneshot::Sender<Response> },
    Attach { id: ClientId, cols: u16, rows: u16, out: mpsc::UnboundedSender<ServerFrame> },
    Frame { id: ClientId, frame: ClientFrame },
    Detached { id: ClientId },
}

struct Client {
    out: mpsc::UnboundedSender<ServerFrame>,
    /// Panes this client has not received a full grid for yet.
    needs_full: Vec<PaneId>,
}

pub struct Engine {
    pub cfg: Config,
    pub session: Session,
    pub bus: bus::Bus,
    pub board: tasks::Board,
    pub triggers: triggers::Store,
    pub journal: journal::Journal,
    /// Pane names as of the start of this tick. An exit event is emitted after the pane has
    /// already been removed, so the name has to have been captured before that.
    pane_names: HashMap<PaneId, String>,
    /// When this daemon started, unix millis. The fallback window for a first digest.
    pub started: u64,
    /// When you last read a digest. The window a digest covers, in other words — it advances
    /// only on a read, so ignoring digests widens the window instead of losing the history.
    pub last_seen: u64,
    /// When horde last reached out to you, unix millis. A separate marker from `last_seen` on
    /// purpose: an alert reports a window without consuming it, so the digest waiting when you
    /// get back is still the whole story. See [`notify`].
    pub last_alert: u64,
    pub agents: agents::Detector,
    clients: HashMap<ClientId, Client>,
    /// Set when the shape changed and clients need a fresh snapshot.
    dirty_shape: bool,
    /// Set when a pane appeared, so detection runs on the next tick instead of waiting for
    /// the slow cadence.
    detect_soon: bool,
    /// When the last resize arrived, while a drag is still delivering them.
    ///
    /// Cleared once the flurry stops, at which point every pane is told to redraw. A program
    /// that repainted halfway through a drag painted for a size that is already stale, and
    /// nothing else in horde can prompt it to try again — see [`pane::Pane::force_redraw`].
    resize_settling: Option<std::time::Instant>,
    pending_events: Vec<Event>,
}

impl Engine {
    /// Queue an event for delivery to attached clients on the next tick.
    pub fn emit(&mut self, ev: Event) {
        self.pending_events.push(ev);
    }

    pub fn notice(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.emit(Event::Notice { level, text: text.into() });
    }

    /// Mark the session shape as changed so clients get a new snapshot.
    pub fn touch(&mut self) {
        self.dirty_shape = true;
    }

    /// Ask for a detection pass on the next tick, after spawning a pane.
    pub fn detect_now(&mut self) {
        self.detect_soon = true;
    }

    // Field-splitting wrappers. `self.agents.scan(&mut self.session, ...)` cannot borrow
    // two fields of `self` through a method call, so destructure instead.
    fn detect(&mut self) -> Vec<Event> {
        let Engine { agents, session, cfg, .. } = self;
        agents.scan(session, cfg)
    }

    pub fn mark_seen(&mut self, pane: PaneId) {
        let Engine { agents, session, .. } = self;
        agents.mark_seen(session, pane);
    }

    /// Deliver anything that was waiting for a busy agent to come free.
    ///
    /// Parked with the rest of the bus (`bus::ENABLED`). This is the one path that injects into
    /// a pane without anyone asking *at that moment* — the asking happened earlier — so leaving
    /// it running would mean a paused bus still typed at agents, which is the whole complaint.
    /// Anything already queued stays queued, and shows as queued, until the bus is back.
    fn flush_bus(&mut self) -> Vec<Event> {
        if !bus::ENABLED {
            return Vec::new();
        }
        let Engine { bus, session, cfg, .. } = self;
        bus.flush_queued(session, cfg)
    }

    /// Tell one idle agent that there is work on the board.
    ///
    /// The board is deliberately pull-based — nobody assigns work, whoever is free takes it.
    /// But "pull-based" implemented as "nobody is ever told" leaves an idle agent with no
    /// reason to ever look, so tasks sit on a board next to agents doing nothing. This closes
    /// that gap without turning the board into a push queue: the nudge is advisory, and
    /// `claim` remains the compare-and-set, so nothing about exclusivity depends on who got
    /// told.
    ///
    /// Three deliberate limits:
    ///
    /// - **One agent, not a broadcast.** Ten agents woken for one task means nine turns spent
    ///   discovering an empty board.
    /// - **`Done` only for agents already working the board.** A `done` agent is normally
    ///   holding a result the human has not read, and pulling it into board work would bury
    ///   that. But an agent that finishes a board task while unfocused becomes `done` rather
    ///   than `idle` — so excluding `done` outright stalled the loop after exactly one task
    ///   each, which running it is how I found out. An agent that has owned a board task has
    ///   its result recorded on the board, so nothing is buried by giving it more.
    /// - **Once per idle period.** Keyed on the agent's `since`, so an agent that ignores the
    ///   nudge is not asked again until it has actually done something. Ten tasks added at
    ///   once produce one nudge, not ten.
    ///
    /// Parked for now behind [`tasks::autonomous`]: everything below the gate is intact and
    /// still under test, but nothing tells an agent about the board until the switch is back on.
    fn nudge_for_tasks(&mut self) -> Vec<Event> {
        if !tasks::autonomous() {
            return Vec::new();
        }
        let open = self.board.open_count();
        if !self.cfg.task_nudge || open == 0 {
            return Vec::new();
        }

        // An agent already holding a task does not need more.
        let holding: Vec<String> = self
            .board
            .all()
            .iter()
            .filter(|t| t.is_claimed())
            .filter_map(|t| t.owner.clone())
            .collect();

        // Anyone who has ever owned a task is a board worker, and stays in the loop even when
        // finishing leaves them `done`.
        let board_workers: Vec<String> =
            self.board.all().iter().filter_map(|t| t.owner.clone()).collect();

        // Agents told about the board that have not acted yet. They are about to consume
        // tasks, so they count against the work available — otherwise "one per pass" simply
        // wakes every idle agent over successive passes, which is the waste this is meant to
        // avoid. Observed: one task, four idle agents, four nudges.
        let already_told = self
            .session
            .panes
            .values()
            .filter_map(|p| p.agent.as_ref())
            .filter(|a| eligible_state(a, &board_workers))
            .filter(|a| a.nudged_since == Some(a.since))
            .count();

        // Never wake more agents than there is work for, but do wake several when there is:
        // five tasks and three idle agents should end up with three agents working.
        if open <= holding.len() + already_told {
            return Vec::new();
        }

        // Whoever has been idle longest is the most available, and picking by `since` rather
        // than by pane id spreads successive tasks across the fleet.
        let pick = self
            .session
            .panes
            .values()
            .filter_map(|p| p.agent.as_ref().map(|a| (p.id, a)))
            .filter(|(_, a)| eligible_state(a, &board_workers))
            .filter(|(_, a)| a.queued.is_empty())
            .filter(|(_, a)| !holding.contains(&a.name))
            .filter(|(_, a)| a.nudged_since != Some(a.since))
            .min_by_key(|(_, a)| a.since)
            .map(|(id, a)| (id, a.name.clone(), a.since));

        let Some((pane, name, since)) = pick else { return Vec::new() };
        // Marked before sending, so a failure cannot produce a nudge loop.
        if let Some(a) = self.session.panes.get_mut(&pane).and_then(|p| p.agent.as_mut()) {
            a.nudged_since = Some(since);
        }

        let body = format!(
            "{open} task{} waiting on the board. Run `horde task claim` to take the next one, \
             do it, then `horde task done --result \"<what happened>\"`. Repeat while it keeps \
             returning work.",
            if open == 1 { "" } else { "s" }
        );
        let Engine { bus, session, cfg, .. } = self;
        match bus.send(session, cfg, None, &name, &body, false, false, None) {
            Ok(m) => vec![Event::BusMessage(m)],
            Err(_) => Vec::new(),
        }
    }
}

pub async fn run(cfg: Config, warnings: Vec<String>) -> Result<()> {
    run_inner(cfg, warnings, false).await
}

/// Start as the successor in a live handoff: adopt the predecessor's panes, then take over
/// its socket. See [`upgrade`].
pub async fn run_imported(cfg: Config, warnings: Vec<String>) -> Result<()> {
    run_inner(cfg, warnings, true).await
}

async fn run_inner(cfg: Config, warnings: Vec<String>, importing: bool) -> Result<()> {
    // While importing, the predecessor still owns the real socket path, so bind a staging
    // path and move it into place once it says go.
    let socket = if importing { upgrade::staging_socket() } else { socket_path() };
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    // An importing daemon may find a staging socket left by an aborted attempt.
    if importing {
        let _ = std::fs::remove_file(&socket);
    }
    ensure_socket_free(&socket).await?;

    // Unix sockets cap the path at ~104 bytes on macOS, and the raw OS error for that is
    // just "path must be shorter than SUN_LEN", which explains nothing.
    if socket.as_os_str().len() > 100 {
        return Err(anyhow!(
            "socket path is too long for the OS ({} bytes, limit ~100): {}\n\
             set HORDE_SOCKET to somewhere shorter, e.g. HORDE_SOCKET=/tmp/horde.sock",
            socket.as_os_str().len(),
            socket.display()
        ));
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("could not bind {}", socket.display()))?;

    // Take the handoff socket before anything else can consume descriptor 3.
    let import = if importing {
        Some(upgrade::inherited_socket().context("picking up the handoff socket")?)
    } else {
        None
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let engine = tokio::spawn(engine_loop(cfg, warnings, rx, import));

    let accept_tx = tx.clone();
    let accept = tokio::spawn(async move {
        let mut next_id: ClientId = 1;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let id = next_id;
                    next_id += 1;
                    let tx = accept_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_conn(stream, id, tx).await {
                            log_line(&format!("connection {id} ended: {e}"));
                        }
                    });
                }
                Err(e) => {
                    log_line(&format!("accept failed: {e}"));
                    return;
                }
            }
        }
    });

    // SIGHUP means "the terminal went away", which is precisely the case the daemon exists
    // to survive. Ignore it rather than dying with the terminal that started us.
    //
    // `setsid` in the spawner already means we should never receive one, but a daemon
    // started by hand from a shell (`horde daemon &`) has no such protection, and losing
    // every agent because a window closed is not a failure worth risking twice.
    let mut sighup = signal(SignalKind::hangup()).context("installing SIGHUP handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let hup = tokio::spawn(async move {
        loop {
            sighup.recv().await;
            log_line("ignoring SIGHUP — the daemon outlives its terminal");
        }
    });

    let result = tokio::select! {
        r = engine => r.map_err(|e| anyhow!("engine panicked: {e}")).and_then(|r| r),
        _ = accept => Ok(()),
        // Both of these are orderly shutdowns, so the engine saves state on its way out.
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = sigterm.recv() => {
            log_line("SIGTERM — shutting down");
            Ok(())
        }
    };
    hup.abort();
    // Leave no stale socket behind for the next start to trip over. A daemon that handed
    // over must not remove the path — its successor is listening on it now.
    if !HANDED_OVER.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = std::fs::remove_file(socket_path());
    }
    let _ = std::fs::remove_file(upgrade::staging_socket());
    result
}

/// Remove the socket if it is stale, or refuse to start if a daemon already owns it.
async fn ensure_socket_free(socket: &PathBuf) -> Result<()> {
    if !socket.exists() {
        return Ok(());
    }
    match UnixStream::connect(socket).await {
        Ok(_) => Err(anyhow!(
            "a horde daemon is already running on {} (run `horde stop` first)",
            socket.display()
        )),
        Err(_) => {
            // Nothing is listening, so the file is left over from a crash.
            std::fs::remove_file(socket)
                .with_context(|| format!("could not remove stale socket {}", socket.display()))?;
            Ok(())
        }
    }
}

/// Set once this process has committed a handoff, so shutdown does not delete the socket its
/// successor now owns.
pub static HANDED_OVER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

async fn engine_loop(
    cfg: Config,
    warnings: Vec<String>,
    mut rx: mpsc::UnboundedReceiver<DaemonMsg>,
    import: Option<std::os::unix::net::UnixStream>,
) -> Result<()> {
    let session = Session::new(&cfg);
    let agents = agents::Detector::new(&cfg);
    let mut eng = Engine {
        session,
        bus: bus::Bus::new(crate::config::bus_log_path()),
        board: tasks::Board::new(crate::config::tasks_path()),
        triggers: triggers::Store::new(crate::config::triggers_path()),
        journal: journal::Journal::new(crate::config::journal_path()),
        pane_names: HashMap::new(),
        started: now_millis(),
        last_seen: 0,
        last_alert: 0,
        agents,
        cfg,
        clients: HashMap::new(),
        dirty_shape: true,
        detect_soon: true,
        resize_settling: None,
        pending_events: Vec::new(),
    };

    // Nothing replays the daemon log, so it only needs bounding — done once at startup, where
    // a size check costs nothing, rather than on every line.
    logfile::rotate_plain(&crate::config::log_path(), logfile::MAX_BYTES);

    let mut import = import;
    match &mut import {
        Some(sock) => {
            // Adopt the predecessor's panes. A failure here means rolling back is still
            // possible on their side, so report it and exit rather than starting empty and
            // pretending everything is fine.
            match import_session(&mut eng, sock) {
                Ok(n) => log_line(&format!("handoff: adopted {n} panes")),
                Err(e) => {
                    log_line(&format!("handoff: import failed: {e:#}"));
                    return Err(e);
                }
            }
        }
        None => match persist::load(&crate::config::state_path()) {
            Ok(Some(saved)) => {
                if let Err(e) = persist::restore(&mut eng, saved) {
                    log_line(&format!("restore failed, starting fresh: {e}"));
                }
            }
            Ok(None) => {}
            Err(e) => log_line(&format!("could not read saved state: {e}")),
        },
    }
    if eng.session.spaces.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let cfg = eng.cfg.clone();
        eng.session.create_space(&cfg, None, &cwd)?;
    }

    for w in warnings {
        eng.notice(NoticeLevel::Warn, w);
    }

    // Undelivered messages from the previous run are about to be re-homed. Say so, because
    // an agent receiving a message from an hour ago is confusing without the context.
    match eng.bus.orphan_count() {
        0 => {}
        n => eng.notice(
            NoticeLevel::Info,
            format!(
                "{n} message{} from before the restart {} waiting to be delivered",
                if n == 1 { "" } else { "s" },
                if n == 1 { "is" } else { "are" }
            ),
        ),
    }

    // Everything is rebuilt: tell the predecessor to stand down and take over its socket.
    if let Some(sock) = &mut import {
        upgrade::complete_import(sock).context("completing the handoff")?;
    }

    let mut attached = false;
    let mut ticker = new_ticker(attached);
    let mut last_detect = std::time::Instant::now();
    let mut save_at: Option<std::time::Instant> = None;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                if handle_msg(&mut eng, msg) {
                    break;
                }
                save_at = Some(std::time::Instant::now() + SAVE_DELAY);
            }
            _ = ticker.tick() => {
                let detect = last_detect.elapsed() >= DETECT_INTERVAL;
                if detect {
                    last_detect = std::time::Instant::now();
                }
                tick(&mut eng, detect);

                if save_at.is_some_and(|at| std::time::Instant::now() >= at) {
                    save_at = None;
                    if let Err(e) = persist::save(&eng, &crate::config::state_path()) {
                        log_line(&format!("could not save state: {e}"));
                    }
                }
            }
        }

        // Drop to the slow cadence the moment the last client leaves, and back up the
        // instant one arrives.
        //
        // Pending pane output counts as a reason to run fast even with nobody watching: a
        // backlogged pane only advances once per tick, so at the detached cadence a long
        // message to a slow agent would trickle out over seconds. Ticking fast while there is
        // a backlog costs nothing once it clears.
        let want_fast = !eng.clients.is_empty() || eng.session.has_pending_output();
        if want_fast != attached {
            attached = want_fast;
            ticker = new_ticker(attached);
        }
    }

    if !HANDED_OVER.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = persist::save(&eng, &crate::config::state_path());
    }
    Ok(())
}

/// Rebuild the session from a predecessor's manifest and descriptors.
fn import_session(eng: &mut Engine, sock: &mut std::os::unix::net::UnixStream) -> Result<usize> {
    let (manifest, fds) = handoff::recv(sock)?;
    if fds.len() != manifest.panes.len() {
        return Err(anyhow!(
            "manifest lists {} panes but {} descriptors arrived",
            manifest.panes.len(),
            fds.len()
        ));
    }
    let cfg = eng.cfg.clone();
    let theme = cfg.theme.clone();
    let count = eng.session.import(&cfg, &theme, manifest, fds)?;
    eng.touch();
    eng.detect_now();
    Ok(count)
}

/// Returns true when the daemon should shut down.
fn handle_msg(eng: &mut Engine, msg: DaemonMsg) -> bool {
    match msg {
        DaemonMsg::Rpc { req, reply } => {
            let stop = req.method == "server.stop";
            // Handoff is handled here rather than in the dispatcher: on success this process
            // must stop touching the session and exit, which is not something a normal
            // method return can express.
            if req.method == "server.handoff" {
                let exe = req
                    .params
                    .get("exe")
                    .and_then(|v| v.as_str())
                    .map(std::path::PathBuf::from);
                match upgrade::run(eng, exe) {
                    Ok(()) => {
                        HANDED_OVER.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = reply.send(Response::ok(
                            req.id,
                            serde_json::json!({ "handed_over": true }),
                        ));
                        return true;
                    }
                    Err(e) => {
                        let _ = reply.send(Response::err(req.id, "failed", format!("{e:#}")));
                        return false;
                    }
                }
            }
            let resp = rpc::dispatch(eng, req);
            let _ = reply.send(resp);
            if stop {
                return true;
            }
        }
        DaemonMsg::Attach { id, cols, rows, out } => {
            let panes: Vec<PaneId> = eng.session.panes.keys().copied().collect();
            eng.clients.insert(id, Client { out, needs_full: panes });
            let cfg = eng.cfg.clone();
            eng.session.set_client_size(&cfg, cols, rows);
            eng.dirty_shape = true;

            // Coming back to five panes of scrollback tells you nothing. Say what changed,
            // and leave the window open so `horde digest` still has the detail — the toast
            // is a pointer, not the report.
            let since = if eng.last_seen == 0 { eng.started } else { eng.last_seen };
            if let Some(line) = digest::build(eng, since).headline() {
                eng.notice(NoticeLevel::Info, format!("{line} — see `horde digest`"));
            }
        }
        DaemonMsg::Detached { id } => {
            eng.clients.remove(&id);
        }
        DaemonMsg::Frame { id, frame } => handle_client_frame(eng, id, frame),
    }
    false
}

fn handle_client_frame(eng: &mut Engine, id: ClientId, frame: ClientFrame) {
    let cfg = eng.cfg.clone();
    match frame {
        ClientFrame::Ping => {}
        ClientFrame::Detach => {
            eng.clients.remove(&id);
        }
        ClientFrame::Resize { cols, rows } => {
            // Applied immediately so the layout tracks the drag, but remembered as pending so
            // the tick can settle it afterwards. Dragging a window edge delivers dozens of
            // sizes a second, and a program that repaints for each of them is repainting for a
            // size that is already out of date.
            eng.session.set_client_size(&cfg, cols, rows);
            eng.resize_settling = Some(std::time::Instant::now());
            // Every pane moved, so nothing the client has cached is still valid.
            let panes: Vec<PaneId> = eng.session.panes.keys().copied().collect();
            for p in &panes {
                if let Some(pane) = eng.session.panes.get_mut(p) {
                    pane.request_full_repaint();
                }
            }
            if let Some(c) = eng.clients.get_mut(&id) {
                c.needs_full = panes;
            }
            eng.dirty_shape = true;
        }
        ClientFrame::Input { pane, bytes } => {
            if let Some(p) = eng.session.panes.get_mut(&pane) {
                let _ = p.write_input(&bytes);
            }
            // Typing at a pane counts as looking at it, which clears a `done` badge.
            eng.mark_seen(pane);
        }
        ClientFrame::Focus { pane } => {
            if eng.session.focus_pane(pane) {
                eng.mark_seen(pane);
                eng.dirty_shape = true;
            }
        }
        // Handled here rather than in `apply_cmd`: this is the one command that returns a
        // value, and only the caller should get it.
        ClientFrame::Command(Cmd::RequestDigest) => {
            let since = if eng.last_seen == 0 { eng.started } else { eng.last_seen };
            let d = digest::build(eng, since);
            // Opening the overlay is looking, so the window advances — same rule as the CLI.
            eng.last_seen = now_millis();
            eng.touch();
            if let Some(c) = eng.clients.get(&id) {
                let _ = c.out.send(ServerFrame::Digest(Box::new(d)));
            }
        }
        ClientFrame::Command(cmd) => apply_cmd(eng, cmd),
    }
}

pub fn apply_cmd(eng: &mut Engine, cmd: Cmd) {
    let cfg = eng.cfg.clone();
    // Errors are collected rather than reported inline: `eng.session` is borrowed for the
    // duration of most arms, so `eng.notice` cannot be called from inside them.
    let mut problems: Vec<(NoticeLevel, String)> = Vec::new();
    let mut seen: Option<PaneId> = None;

    match cmd {
        Cmd::SplitRight | Cmd::SplitDown => {
            let dir = if cmd == Cmd::SplitRight { Dir::Right } else { Dir::Down };
            if let Err(e) = eng.session.split(&cfg, None, dir, None) {
                problems.push((NoticeLevel::Warn, e.to_string()));
            }
        }
        Cmd::ClosePane => {
            if let Some(p) = eng.session.focused_pane() {
                let _ = eng.session.close_pane(&cfg, p);
            }
        }
        Cmd::FocusDir(d) => {
            eng.session.focus_dir(d);
            seen = eng.session.focused_pane();
        }
        Cmd::Resize { dir, cells } => {
            eng.session.resize_pane(&cfg, dir, cells);
        }
        Cmd::ToggleZoom => {
            eng.session.toggle_zoom(&cfg);
        }
        Cmd::SwapDir(d) => {
            eng.session.swap_dir(&cfg, d);
        }
        Cmd::NewTab => {
            if let Some(space) = eng.session.focused_space {
                if let Err(e) = eng.session.create_tab(&cfg, space, None) {
                    problems.push((NoticeLevel::Error, e.to_string()));
                }
            }
        }
        Cmd::NextTab => {
            eng.session.cycle_tab(1);
        }
        Cmd::PrevTab => {
            eng.session.cycle_tab(-1);
        }
        Cmd::GotoTab(i) => {
            eng.session.goto_tab(i);
        }
        Cmd::CloseTab => {
            if let Some(t) = eng.session.focused_tab() {
                let _ = eng.session.close_tab(&cfg, t);
            }
        }
        Cmd::NewSpace { name } => {
            // Inherit the current space's directory: a new space is nearly always more
            // work on the same project, not work where the daemon happened to start.
            let cwd = eng
                .session
                .focused_space
                .and_then(|s| eng.session.space(s))
                .map(|s| s.cwd.clone())
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            if let Err(e) = eng.session.create_space(&cfg, name.as_deref(), &cwd) {
                problems.push((NoticeLevel::Error, e.to_string()));
            }
        }
        Cmd::FocusSpace(id) => {
            eng.session.focus_space(id);
        }
        Cmd::NextSpace => {
            eng.session.cycle_space(1);
        }
        Cmd::PrevSpace => {
            eng.session.cycle_space(-1);
        }
        Cmd::ToggleSidebar => eng.session.toggle_sidebar(&cfg),
        Cmd::ToggleBus => eng.session.toggle_bus(&cfg),
        Cmd::Redraw => {
            // The escape hatch. A program can always miss a resize, and until now the only
            // cure was resizing the window again to jog it.
            let ids: Vec<PaneId> = eng.session.panes.keys().copied().collect();
            for id in ids {
                if let Some(p) = eng.session.panes.get_mut(&id) {
                    let _ = p.force_redraw();
                }
            }
            for p in eng.session.panes.values_mut() {
                p.request_full_repaint();
            }
            eng.touch();
        }
        Cmd::JumpAttention => match eng.session.next_attention() {
            Some(p) => {
                eng.session.focus_pane(p);
                seen = Some(p);
            }
            None => problems.push((NoticeLevel::Info, "no agent needs attention".into())),
        },
        Cmd::Scroll { pane, lines } => {
            if let Some(p) = eng.session.panes.get_mut(&pane) {
                p.scroll(lines);
            }
        }
        Cmd::ScrollBottom { pane } => {
            if let Some(p) = eng.session.panes.get_mut(&pane) {
                p.scroll_bottom();
            }
        }
        Cmd::FocusPane(p) => {
            if eng.session.focus_pane(p) {
                seen = Some(p);
            }
        }
        Cmd::RenamePane { pane, name } => {
            if let Some(p) = eng.session.panes.get_mut(&pane) {
                p.name = if name.is_empty() { None } else { Some(name) };
            }
        }
        Cmd::SpawnAgent { cmd, name, split } => {
            let dir = split.unwrap_or(Dir::Right);
            match eng.session.split(&cfg, None, dir, Some(&cmd)) {
                Ok(id) => {
                    if let (Some(n), Some(p)) = (name, eng.session.panes.get_mut(&id)) {
                        p.name = Some(n);
                    }
                }
                Err(e) => problems.push((NoticeLevel::Warn, e.to_string())),
            }
        }
        Cmd::RenameSpace { space, name } => {
            eng.session.rename_space(space, &name);
        }
        Cmd::RenameTab { tab, name } => {
            eng.session.rename_tab(tab, &name);
        }
        Cmd::CloseSpace(id) => {
            let _ = eng.session.close_space(&cfg, id);
        }
        Cmd::FocusTab(id) => {
            eng.session.focus_tab(id);
            seen = eng.session.focused_pane();
        }
        Cmd::NewTabIn(space) => {
            if let Err(e) = eng.session.create_tab(&cfg, space, None) {
                problems.push((NoticeLevel::Error, e.to_string()));
            }
        }
        // Answered in `handle_client_frame`, which knows which client asked. Reaching here
        // means it came from the control API, where `digest` is the method to use.
        Cmd::RequestDigest => {}
        Cmd::ApplyLayout { preset } => {
            if let Err(e) = eng.session.apply_preset(&cfg, &preset) {
                problems.push((NoticeLevel::Warn, e.to_string()));
            }
        }
        Cmd::SetSpaceAccent { space, slot } => {
            eng.session.set_space_accent(space, slot);
        }
        Cmd::SetPaneRole { pane, role } => {
            eng.session.set_pane_role(pane, &role);
        }
        Cmd::ToggleSpaceCollapsed(space) => {
            eng.session.toggle_space_collapsed(space, None);
        }
        Cmd::TogglePanePinned(pane) => {
            eng.session.toggle_pane_pinned(pane, None);
        }
    }

    if let Some(p) = seen {
        eng.mark_seen(p);
    }
    // Any of the arms above can have created a pane; a spare detection pass is cheap.
    eng.detect_soon = true;
    for (level, text) in problems {
        eng.notice(level, text);
    }
    eng.dirty_shape = true;
}

fn new_ticker(attached: bool) -> tokio::time::Interval {
    let mut t = tokio::time::interval(if attached { TICK_ATTACHED } else { TICK_DETACHED });
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    t
}

/// One frame: pump panes, optionally run detection, broadcast.
fn tick(eng: &mut Engine, detect_due: bool) {
    let theme = eng.cfg.theme.clone();
    let pane_ids: Vec<PaneId> = eng.session.panes.keys().copied().collect();
    eng.pane_names =
        pane_ids.iter().map(|id| (*id, bus::Bus::sender_name(&eng.session, Some(*id)))).collect();
    for id in &pane_ids {
        if let Some(p) = eng.session.panes.get_mut(id) {
            p.pump(&theme);
        }
    }

    // A freshly spawned pane gets looked at on the very next tick rather than waiting out
    // the interval, so a new agent appears in the sidebar immediately.
    if detect_due || eng.detect_soon {
        eng.detect_soon = false;
        let before = agent_fingerprint(&eng.session);
        let mut events = eng.detect();
        // A message held back for a busy agent may now be deliverable.
        events.extend(eng.flush_bus());
        // Then, if anyone is free and the board is not empty, say so. A no-op while the board's
        // autonomous half is parked; see `tasks::autonomous()`.
        events.extend(eng.nudge_for_tasks());
        let changed = !events.is_empty();
        for ev in events {
            eng.pending_events.push(ev);
        }

        // Agent state, names, and elapsed timers all travel in the snapshot, so a detection
        // pass has to refresh it. Without this the sidebar keeps whatever it last saw and
        // only catches up when something unrelated happens to dirty the shape.
        //
        // The fingerprint comparison matters on top of `has_agents`: an agent that
        // *disappears* produces no event and leaves no agent behind to force a refresh, so
        // the sidebar would go on showing one that has exited.
        let after = agent_fingerprint(&eng.session);
        let has_agents = !after.is_empty();
        if changed || has_agents || before != after {
            eng.dirty_shape = true;
        }
    }

    let cfg = eng.cfg.clone();
    let exited = eng.session.reap_exited(&cfg);
    for p in &exited {
        eng.pending_events.push(Event::PaneExited { pane: *p, status: 0 });
        eng.dirty_shape = true;
    }

    // An agent that went away still holds whatever it claimed. Hand it back, or the board
    // quietly stalls on work nobody is doing. This runs after reaping, and every tick
    // rather than only on detection passes, because a pane can close without detection
    // having a say.
    //
    // Parked with the rest of the autonomous half (`tasks::autonomous()`): a task moving state
    // with nobody asking is the behaviour being reworked. Nothing moves on a paused board
    // anyway — a claim left behind while it is paused stays claimed, visible in `task list`,
    // and is sorted out when the board comes back.
    if tasks::autonomous() && eng.board.claimed_count() > 0 {
        let live: Vec<String> = eng
            .session
            .panes
            .keys()
            .map(|p| bus::Bus::sender_name(&eng.session, Some(*p)))
            .collect();
        for t in eng.board.reclaim_absent(&live) {
            log_line(&format!("task #{} returned to the board", t.id));
            eng.pending_events.push(Event::Notice {
                level: NoticeLevel::Info,
                text: format!("task #{} is open again — its agent left", t.id),
            });
            eng.dirty_shape = true;
        }
    }
    // A drag has stopped delivering sizes: give every program one clean chance to repaint at
    // the size it actually has now.
    if eng.resize_settling.is_some_and(|t| t.elapsed() >= RESIZE_SETTLE) {
        eng.resize_settling = None;
        let ids: Vec<PaneId> = eng.session.panes.keys().copied().collect();
        for id in ids {
            if let Some(p) = eng.session.panes.get_mut(&id) {
                let _ = p.force_redraw();
            }
        }
        eng.dirty_shape = true;
    }

    // Before the notifier, so a firing is something this pass can already tell you about.
    let fired = triggers::fire_due(eng);
    if !fired.is_empty() {
        eng.pending_events.extend(fired);
        eng.dirty_shape = true;
    }

    // With nobody attached, this is the only way anything gets out. Called every tick rather
    // than only on detection passes because its own quiet window is what limits it, and that
    // check is cheaper than deciding when to run the check.
    notify::consider(eng);

    if !exited.is_empty() && eng.session.spaces.is_empty() {
        // The last pane closed; recreate a space so horde is never left unusable.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = eng.session.create_space(&cfg, None, &cwd);
        eng.dirty_shape = true;
    }

    broadcast(eng);
}

/// Whether an agent's state means it is free to take board work.
///
/// `idle` always counts. `done` counts only for an agent that has already owned a task: it
/// finished board work while unfocused, and its result is on the board rather than only on its
/// screen. For anyone else `done` means "the human has not read this yet", which is not
/// something to interrupt.
fn eligible_state(a: &state::AgentRuntime, board_workers: &[String]) -> bool {
    match a.state {
        crate::proto::AgentState::Idle => true,
        crate::proto::AgentState::Done => board_workers.contains(&a.name),
        _ => false,
    }
}

/// Cheap summary of every agent, so a detection pass can tell whether anything the client
/// can see has changed — including an agent disappearing, which emits no event.
fn agent_fingerprint(session: &Session) -> Vec<(PaneId, String, crate::proto::AgentState)> {
    let mut v: Vec<_> = session
        .panes
        .values()
        .filter_map(|p| p.agent.as_ref().map(|a| (p.id, a.name.clone(), a.state)))
        .collect();
    v.sort_by_key(|(id, _, _)| *id);
    v
}

fn broadcast(eng: &mut Engine) {
    // Journal before anything can drop the events: the detached path clears them, and the
    // detached path is exactly when a digest is being accumulated.
    if !eng.pending_events.is_empty() {
        // Names come from the start-of-tick map, not from the session: a pane that exited was
        // already reaped, and "builder exited" is the useful line, not "pane2 exited".
        let events = std::mem::take(&mut eng.pending_events);
        let names = std::mem::take(&mut eng.pane_names);
        for ev in &events {
            eng.journal
                .record(ev, |id| names.get(&id).cloned().unwrap_or_else(|| format!("pane{id}")));
        }
        eng.pane_names = names;
        eng.pending_events = events;
    }

    if eng.clients.is_empty() {
        // Nothing attached: drain dirty rows anyway so they cannot pile up unboundedly.
        let ids: Vec<PaneId> = eng.session.panes.keys().copied().collect();
        for id in ids {
            if let Some(p) = eng.session.panes.get_mut(&id) {
                p.take_dirty();
            }
        }
        eng.pending_events.clear();
        return;
    }

    let cfg = eng.cfg.clone();
    let snapshot = if eng.dirty_shape {
        eng.dirty_shape = false;
        let mut s = eng.session.snapshot(&cfg);
        s.tasks_open = eng.board.open_count();
        s.tasks_claimed = eng.board.claimed_count();
        s.triggers_armed = if eng.cfg.unattended { eng.triggers.armed_count() } else { 0 };
        Some(Box::new(s))
    } else {
        None
    };

    // Only panes on screen are worth sending; the rest keep running invisibly.
    let visible: Vec<PaneId> = eng.session.visible_panes();
    let focused = eng.session.focused_pane();

    // Take dirty rows once, then fan the same payload out to every client.
    let mut updates: Vec<(PaneId, Vec<RowUpdate>, Option<CursorPos>)> = Vec::new();
    for id in &visible {
        let Some(p) = eng.session.panes.get_mut(id) else { continue };
        let dirty = p.take_dirty();
        if dirty.is_empty() {
            continue;
        }
        let rows: Vec<RowUpdate> = dirty
            .iter()
            .filter_map(|&y| p.row(y).map(|r| RowUpdate { y, row: r.clone() }))
            .collect();
        let mut cursor = p.cursor();
        cursor.visible = cursor.visible && Some(*id) == focused;
        updates.push((*id, rows, Some(cursor)));
    }

    // Which panes each client still needs in full, claimed before building payloads so the
    // session borrow and the clients borrow never overlap.
    let per_client: Vec<(ClientId, Vec<PaneId>)> = eng
        .clients
        .iter_mut()
        .map(|(cid, c)| {
            let need: Vec<PaneId> =
                c.needs_full.iter().copied().filter(|p| visible.contains(p)).collect();
            c.needs_full.retain(|p| !visible.contains(p));
            (*cid, need)
        })
        .collect();

    let union: HashSet<PaneId> = per_client.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    let mut full_grids: HashMap<PaneId, (Vec<RowUpdate>, CursorPos)> = HashMap::new();
    for p in union {
        let Some(pane) = eng.session.panes.get(&p) else { continue };
        let rows: Vec<RowUpdate> = pane
            .mirror()
            .iter()
            .enumerate()
            .map(|(y, r)| RowUpdate { y: y as u16, row: r.clone() })
            .collect();
        let mut cursor = pane.cursor();
        cursor.visible = cursor.visible && Some(p) == focused;
        full_grids.insert(p, (rows, cursor));
    }

    let events = std::mem::take(&mut eng.pending_events);
    let mut gone = Vec::new();

    for (cid, need_full) in per_client {
        let Some(client) = eng.clients.get(&cid) else { continue };
        let ok = (|| {
            if let Some(snap) = &snapshot {
                client.out.send(ServerFrame::Snapshot(snap.clone())).ok()?;
            }
            for p in &need_full {
                if let Some((rows, cursor)) = full_grids.get(p) {
                    client
                        .out
                        .send(ServerFrame::Rows {
                            pane: *p,
                            rows: rows.clone(),
                            cursor: Some(*cursor),
                        })
                        .ok()?;
                }
            }
            for (p, rows, cursor) in &updates {
                // A pane just sent in full is already current; skip the duplicate.
                if need_full.contains(p) {
                    continue;
                }
                client
                    .out
                    .send(ServerFrame::Rows { pane: *p, rows: rows.clone(), cursor: *cursor })
                    .ok()?;
            }
            for ev in &events {
                client.out.send(ServerFrame::Event(ev.clone())).ok()?;
            }
            Some(())
        })();
        if ok.is_none() {
            gone.push(cid);
        }
    }

    for c in gone {
        eng.clients.remove(&c);
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// A connection speaks newline JSON until it asks to `attach`, after which it switches to
/// postcard frames in both directions for the rest of its life.
async fn serve_conn(
    stream: UnixStream,
    id: ClientId,
    tx: mpsc::UnboundedSender<DaemonMsg>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err("", "bad_request", format!("invalid JSON: {e}"));
                write_json(&mut write_half, &resp).await?;
                continue;
            }
        };

        if req.method == "attach" {
            let protocol = req.params.get("protocol").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let cols = req.params.get("cols").and_then(|v| v.as_u64()).unwrap_or(120) as u16;
            let rows = req.params.get("rows").and_then(|v| v.as_u64()).unwrap_or(40) as u16;

            if protocol != PROTOCOL_VERSION {
                // Both halves ship in one binary, so this only bites across versions.
                let bye = ServerFrame::Bye {
                    reason: format!(
                        "protocol mismatch: client speaks v{protocol}, daemon speaks \
                         v{PROTOCOL_VERSION}. Run `horde stop`, then `horde`."
                    ),
                };
                framing::write_frame(&mut write_half, &bye).await?;
                return Ok(());
            }

            let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerFrame>();
            tx.send(DaemonMsg::Attach { id, cols, rows, out: out_tx })
                .map_err(|_| anyhow!("engine gone"))?;

            // Writer task drains render frames to the socket.
            let writer = tokio::spawn(async move {
                while let Some(frame) = out_rx.recv().await {
                    if framing::write_frame(&mut write_half, &frame).await.is_err() {
                        break;
                    }
                }
            });

            let read_result = async {
                loop {
                    let frame: ClientFrame = framing::read_frame(&mut reader).await?;
                    let detached = matches!(frame, ClientFrame::Detach);
                    tx.send(DaemonMsg::Frame { id, frame })
                        .map_err(|_| anyhow!("engine gone"))?;
                    if detached {
                        return Ok::<(), anyhow::Error>(());
                    }
                }
            }
            .await;

            let _ = tx.send(DaemonMsg::Detached { id });
            writer.abort();
            return read_result;
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(DaemonMsg::Rpc { req, reply: reply_tx }).map_err(|_| anyhow!("engine gone"))?;
        match reply_rx.await {
            Ok(resp) => write_json(&mut write_half, &resp).await?,
            Err(_) => return Ok(()),
        }
    }
}

async fn write_json<W: AsyncWriteExt + Unpin>(w: &mut W, resp: &Response) -> Result<()> {
    let mut buf = serde_json::to_vec(resp)?;
    buf.push(b'\n');
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Append a line to the daemon log. The daemon has no terminal of its own, so anything
/// worth knowing about goes here.
pub fn log_line(msg: &str) {
    use std::io::Write;
    let path = crate::config::log_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {msg}", clock_string());
    }
}

/// `HH:MM:SS` in UTC, without pulling in a date crate for log lines nobody diffs.
fn clock_string() -> String {
    let secs = now_millis() / 1000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an engine with one real pane, as the daemon would have.
    pub(super) fn engine() -> Engine {
        let cfg = Config::default();
        let session = Session::new(&cfg);
        let agents = agents::Detector::new(&cfg);
        let mut eng = Engine {
            session,
            bus: bus::Bus::new(std::env::temp_dir().join("horde-test-bus.jsonl")),
            board: tasks::Board::new(std::env::temp_dir().join("horde-test-tasks.jsonl")),
            triggers: triggers::Store::new(
                std::env::temp_dir().join("horde-test-triggers.jsonl"),
            ),
            journal: journal::Journal::new(std::env::temp_dir().join("horde-test-journal.jsonl")),
            pane_names: HashMap::new(),
            started: now_millis(),
            last_seen: 0,
            last_alert: 0,
            agents,
            cfg,
            clients: HashMap::new(),
            dirty_shape: true,
            detect_soon: true,
            resize_settling: None,
            pending_events: Vec::new(),
        };
        let cfg = eng.cfg.clone();
        eng.session.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        eng
    }

    fn kill_all(eng: &mut Engine) {
        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

    /// The bug this guards: agent state, names, and elapsed timers all reach the client
    /// inside the snapshot. A detection pass that updates them without marking the shape
    /// dirty leaves the sidebar showing whatever it last saw — indefinitely, until
    /// something unrelated happens to dirty it.
    #[test]
    fn a_live_agent_refreshes_the_snapshot_every_detection_pass() {
        let mut eng = engine();
        let pane = *eng.session.panes.keys().next().unwrap();

        // Establish the agent the way an installed hook does. That also keeps it alive
        // through the scan, since a fresh hook report outranks screen detection.
        let Engine { agents, session, .. } = &mut eng;
        agents.report(session, pane, crate::proto::AgentState::Working, None);
        assert!(eng.session.panes[&pane].agent.is_some());

        eng.dirty_shape = false;
        eng.detect_soon = false;
        tick(&mut eng, true);
        assert!(
            eng.dirty_shape,
            "a working agent's elapsed timer only advances if the snapshot is resent"
        );
        // The agent survived the scan, and reaches the client through the snapshot.
        let cfg = eng.cfg.clone();
        let info = eng
            .session
            .snapshot(&cfg)
            .panes
            .into_iter()
            .find(|p| p.id == pane)
            .and_then(|p| p.agent)
            .expect("a hook-reported agent must survive screen detection");
        assert_eq!(info.state, crate::proto::AgentState::Working);
        assert_eq!(info.authority, "hook");
        kill_all(&mut eng);
    }

    /// An agent that goes away emits no state-change event and leaves nothing behind to
    /// force a refresh, so without the fingerprint check the sidebar would keep listing it.
    #[test]
    fn an_agent_disappearing_also_refreshes_the_snapshot() {
        let mut eng = engine();
        let pane = *eng.session.panes.keys().next().unwrap();

        // An agent with no hook backing, in a pane running a plain shell: detection is
        // right to remove it, and the client has to be told.
        eng.session.panes.get_mut(&pane).unwrap().agent = Some(state::AgentRuntime {
            kind: "claude".into(),
            name: "builder".into(),
            state: crate::proto::AgentState::Idle,
            since: std::time::Instant::now(),
            authority: "screen".into(),
            reason: "t".into(),
            seen: false,
            session_id: None,
            queued: Vec::new(),
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                alerted_since: None,
        });

        eng.dirty_shape = false;
        eng.detect_soon = false;
        tick(&mut eng, true);
        assert!(eng.session.panes[&pane].agent.is_none(), "the scan should have removed it");
        assert!(eng.dirty_shape, "the sidebar must be told the agent is gone");
        kill_all(&mut eng);
    }

    /// With no agents there is nothing time-varying to push, so an idle session stays quiet
    /// rather than sending a snapshot every detection pass forever.
    #[test]
    fn an_idle_session_with_no_agents_does_not_refresh_needlessly() {
        let mut eng = engine();
        eng.dirty_shape = false;
        eng.detect_soon = false;
        tick(&mut eng, true);
        assert!(!eng.dirty_shape, "nothing changed, so nothing should be resent");
        kill_all(&mut eng);
    }

    /// A newly spawned pane is looked at on the next tick rather than waiting out the slow
    /// cadence, so a new agent appears immediately instead of up to DETECT_EVERY later.
    #[test]
    fn a_spawn_requests_a_prompt_detection_pass() {
        let mut eng = engine();
        eng.detect_soon = false;
        apply_cmd(&mut eng, Cmd::SplitRight);
        assert!(eng.detect_soon, "spawning a pane must ask for a detection pass");

        // And that pass happens on the very next tick, not only on the cadence.
        eng.dirty_shape = false;
        tick(&mut eng, false); // detection not due
        assert!(!eng.detect_soon, "the requested pass should have run");
        kill_all(&mut eng);
    }

    // -- board nudges ---------------------------------------------------
    // The board is pull-based, so the only thing making it work autonomously is that an idle
    // agent gets told. These tests pin the three limits that keep telling from becoming spam.

    /// A fresh engine with `n` agent panes, all idle, plus a board.
    ///
    /// `tag` keeps each test on its own log files: these run in parallel, and a shared board
    /// file would leak one test's tasks into another's assertions.
    ///
    /// Visible to the rest of the daemon so [`super::notify`] can build on it rather than keep
    /// a second copy of the same twenty lines in step with this one.
    pub(super) fn engine_with_idle_agents(tag: &str, n: usize) -> Engine {
        let p = std::env::temp_dir().join(format!("horde-nudge-{tag}-tasks.jsonl"));
        let _ = std::fs::remove_file(&p);
        let mut eng = engine();
        // On here the way `unattended` is on in the trigger suite: the nudge ships off while the
        // board's autonomous half is parked, and these tests are what keeps it from rotting.
        eng.cfg.task_nudge = true;
        eng.board = tasks::Board::new(p);
        eng.bus =
            bus::Bus::new(std::env::temp_dir().join(format!("horde-nudge-{tag}-bus.jsonl")));
        let cfg = eng.cfg.clone();
        let first = *eng.session.panes.keys().next().unwrap();
        let mut ids = vec![first];
        for _ in 1..n {
            ids.push(eng.session.split(&cfg, Some(first), Dir::Right, None).unwrap());
        }
        for (i, id) in ids.iter().enumerate() {
            let pane = eng.session.panes.get_mut(id).unwrap();
            pane.agent = Some(state::AgentRuntime {
                kind: "claude".into(),
                name: format!("worker{i}"),
                state: crate::proto::AgentState::Idle,
                // Staggered, so "idle longest" is well defined.
                since: std::time::Instant::now() - Duration::from_secs(60 - i as u64),
                authority: "hook".into(),
                reason: "t".into(),
                seen: false,
                session_id: None,
                queued: Vec::new(),
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                alerted_since: None,
            });
        }
        eng
    }

    fn nudge_bodies(events: &[Event]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::BusMessage(m) => Some((m.to.clone(), m.body.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_idle_agent_is_told_when_the_board_has_work() {
        let mut eng = engine_with_idle_agents("told", 1);
        eng.board.add("write the tests", "user").unwrap();
        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, "worker0");
        assert!(sent[0].1.contains("horde task claim"), "it must name the command: {sent:?}");
        kill_all(&mut eng);
    }

    /// Ten tasks added at once must not cost ten turns.
    #[test]
    fn a_burst_of_tasks_produces_one_nudge_not_one_each() {
        let mut eng = engine_with_idle_agents("burst", 1);
        for i in 0..10 {
            eng.board.add(&format!("job {i}"), "user").unwrap();
        }
        let first = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(first.len(), 1);
        // Repeated passes while it stays idle add nothing.
        for _ in 0..5 {
            assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        }
        kill_all(&mut eng);
    }

    /// Waking every agent for one task wastes every turn but one.
    #[test]
    fn only_one_agent_is_woken_per_pass() {
        let mut eng = engine_with_idle_agents("one-only", 3);
        eng.board.add("single job", "user").unwrap();
        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "one task, one agent: {sent:?}");
        // The one idle longest is the most available.
        assert_eq!(sent[0].0, "worker0");
        kill_all(&mut eng);
    }

    /// The bug this pins, found by running it rather than by reasoning about it: "one agent
    /// per pass" is not the same as "one agent". Over successive detection passes every idle
    /// agent got told about a single task — four nudges for one job, three turns wasted.
    #[test]
    fn one_task_wakes_one_agent_even_across_many_passes() {
        let mut eng = engine_with_idle_agents("across-passes", 4);
        eng.board.add("the only job", "user").unwrap();
        let mut total = 0;
        for _ in 0..10 {
            total += nudge_bodies(&eng.nudge_for_tasks()).len();
        }
        assert_eq!(total, 1, "one task must not wake four agents");
        kill_all(&mut eng);
    }

    /// The other half: real work for everyone should reach everyone.
    #[test]
    fn enough_tasks_for_everyone_wakes_everyone() {
        let mut eng = engine_with_idle_agents("all-busy", 3);
        for i in 0..5 {
            eng.board.add(&format!("job {i}"), "user").unwrap();
        }
        let mut told: Vec<String> = Vec::new();
        for _ in 0..10 {
            for (to, _) in nudge_bodies(&eng.nudge_for_tasks()) {
                told.push(to);
            }
        }
        told.sort();
        told.dedup();
        assert_eq!(told.len(), 3, "five jobs, three agents: all three should work: {told:?}");
        kill_all(&mut eng);
    }

    /// A `done` agent is holding a result nobody has read. Sending it off to do board work
    /// would bury that, so it is left alone.
    #[test]
    fn an_agent_with_an_unread_result_is_not_reassigned() {
        let mut eng = engine_with_idle_agents("done", 1);
        if let Some(a) =
            eng.session.panes.values_mut().find_map(|p| p.agent.as_mut())
        {
            a.state = crate::proto::AgentState::Done;
        }
        eng.board.add("job", "user").unwrap();
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        kill_all(&mut eng);
    }

    /// The bug that broke the loop: an agent finishing a board task while unfocused lands in
    /// `done`, not `idle`. Excluding `done` meant each agent took exactly one task and then
    /// went quiet, with work still on the board. A board worker stays in the loop.
    #[test]
    fn a_board_worker_that_finished_while_unfocused_is_given_more() {
        let mut eng = engine_with_idle_agents("done-worker", 1);
        eng.board.add("first", "user").unwrap();
        eng.board.add("second", "user").unwrap();
        eng.board.claim("worker0", Some(1)).unwrap();
        eng.board.done("worker0", Some(1), Some("finished")).unwrap();

        // It finished unfocused, so detection calls that `done`.
        if let Some(a) = eng.session.panes.values_mut().find_map(|p| p.agent.as_mut()) {
            a.state = crate::proto::AgentState::Done;
            a.since = std::time::Instant::now();
        }
        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "the remaining task should reach it: {sent:?}");
        kill_all(&mut eng);
    }

    #[test]
    fn an_agent_already_holding_a_task_is_left_to_it() {
        let mut eng = engine_with_idle_agents("holding", 1);
        eng.board.add("job one", "user").unwrap();
        eng.board.add("job two", "user").unwrap();
        eng.board.claim("worker0", Some(1)).unwrap();
        assert!(
            nudge_bodies(&eng.nudge_for_tasks()).is_empty(),
            "it has work; a second task can wait for someone free"
        );
        kill_all(&mut eng);
    }

    #[test]
    fn an_empty_board_nudges_nobody() {
        let mut eng = engine_with_idle_agents("empty", 2);
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        kill_all(&mut eng);
    }

    #[test]
    fn nudging_can_be_turned_off() {
        let mut eng = engine_with_idle_agents("off", 1);
        eng.cfg.task_nudge = false;
        eng.board.add("job", "user").unwrap();
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        kill_all(&mut eng);
    }

    /// Having done something, an agent becomes available again — and by then the nudge is
    /// useful rather than noise.
    #[test]
    fn a_new_idle_period_earns_a_fresh_nudge() {
        let mut eng = engine_with_idle_agents("fresh", 1);
        eng.board.add("job one", "user").unwrap();
        eng.board.add("job two", "user").unwrap();
        assert_eq!(nudge_bodies(&eng.nudge_for_tasks()).len(), 1);

        // It worked and came back to idle: `since` moves, so it is eligible again.
        if let Some(a) = eng.session.panes.values_mut().find_map(|p| p.agent.as_mut()) {
            a.since = std::time::Instant::now();
            a.queued.clear();
        }
        assert_eq!(
            nudge_bodies(&eng.nudge_for_tasks()).len(),
            1,
            "a second idle period should be told about the remaining work"
        );
        kill_all(&mut eng);
    }
}
