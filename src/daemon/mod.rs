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
pub mod question;
pub mod repo;
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
    ServerFrame, SpaceId, PROTOCOL_VERSION,
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
    /// Branch and dirty state per directory, refreshed on its own slow cadence because each
    /// answer costs a fork. Read by every snapshot, written by nothing else.
    pub repos: repo::Cache,
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
    /// This is the one path that injects into a pane without anyone asking *at that moment* —
    /// the asking happened when the message was sent, and this is the delivery finally
    /// becoming possible.
    fn flush_bus(&mut self) -> Vec<Event> {
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
    /// Gated on `agents.board` and `agents.task_nudge`: everything below the gate is intact and
    /// still under test, but nothing tells an agent about the board until the switch is back on.
    fn nudge_for_tasks(&mut self) -> Vec<Event> {
        if !self.cfg.task_nudge {
            return Vec::new();
        }

        // One pass per project, and at most one agent woken in each.
        //
        // Per project because that is the unit work belongs to: a task added in one repository
        // is meaningless to an agent sitting in another, and the first version of this — which
        // walked every idle agent in the session — handed work across projects constantly. The
        // symptom was agents "working randomly"; the cause was that the board had no scope and
        // the nudge had no scope to respect.
        let spaces: Vec<(SpaceId, String)> =
            self.session.spaces.iter().map(|s| (s.id, s.name.clone())).collect();
        let mut events = Vec::new();
        for (space_id, space_name) in spaces {
            if let Some(ev) = self.nudge_one(space_id, &space_name) {
                events.push(ev);
            }
        }
        events
    }

    /// Tell one enlisted agent in `space` that its project has work waiting.
    fn nudge_one(&mut self, space: SpaceId, space_name: &str) -> Option<Event> {
        let open = self.board.offered_to(space_name);
        if open == 0 {
            return None;
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

        // Enlisted agents in this project, and nowhere else.
        //
        // Enlistment is the second half of the fix. Scope stops work crossing projects;
        // this stops it reaching an agent that never volunteered for any. An agent you opened
        // to think with, sitting idle in the same repository as a fleet, is not a worker.
        let candidates: Vec<(PaneId, String, std::time::Instant)> = self
            .session
            .panes
            .values()
            .filter(|p| p.space == space && p.board && p.exited.is_none())
            .filter_map(|p| p.agent.as_ref().map(|a| (p.id, a)))
            .filter(|(_, a)| eligible_state(a, &board_workers))
            .filter(|(_, a)| a.queued.is_empty())
            .filter(|(_, a)| !holding.contains(&a.name))
            .map(|(id, a)| (id, a.name.clone(), a.since))
            .collect();

        // Agents told about the board that have not acted yet. They are about to consume
        // tasks, so they count against the work available — otherwise "one per pass" simply
        // wakes every idle agent over successive passes, which is the waste this is meant to
        // avoid. Observed: one task, four idle agents, four nudges.
        let already_told = self
            .session
            .panes
            .values()
            .filter(|p| p.space == space && p.board)
            .filter_map(|p| p.agent.as_ref())
            .filter(|a| eligible_state(a, &board_workers))
            .filter(|a| a.nudged_since == Some(a.since))
            .count();

        // Never wake more agents than there is work for, but do wake several when there is:
        // five tasks and three idle agents should end up with three agents working.
        if open <= holding.len() + already_told {
            return None;
        }

        // Whoever has been idle longest is the most available, and picking by `since` rather
        // than by pane id spreads successive tasks across the fleet.
        let (pane, name, since) = candidates
            .into_iter()
            .filter(|(id, _, since)| {
                self.session
                    .panes
                    .get(id)
                    .and_then(|p| p.agent.as_ref())
                    .is_some_and(|a| a.nudged_since != Some(*since))
            })
            .min_by_key(|(_, _, since)| *since)?;

        // Marked before sending, so a failure cannot produce a nudge loop.
        if let Some(a) = self.session.panes.get_mut(&pane).and_then(|p| p.agent.as_mut()) {
            a.nudged_since = Some(since);
        }

        let body = format!(
            "{open} task{} waiting on the {space_name} board. Run `horde task claim` to take \
             the next one, do it, then `horde task done --result \"<what happened>\"`. Repeat \
             while it keeps returning work.",
            if open == 1 { "" } else { "s" }
        );
        let Engine { bus, session, cfg, .. } = self;
        match bus.send(session, cfg, None, &name, &body, false, false, None) {
            Ok(m) => Some(Event::BusMessage(m)),
            Err(_) => None,
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

async fn run_inner(cfg: Config, mut warnings: Vec<String>, importing: bool) -> Result<()> {
    // Before anything opens a descriptor. A daemon that inherits macOS's 256 runs a session
    // fine and then dies during `horde upgrade`, which needs a dup per pane simultaneously —
    // failing the one operation whose whole promise is that it is safe.
    let (before, after) = crate::platform::raise_file_limit();
    log_line(&format!("open file limit: {before} -> {after}"));
    if after < 512 {
        warnings.push(format!(
            "this system caps horde at {after} open files; a large session may fail to upgrade"
        ));
    }

    // The daemon inherits its working directory from wherever `horde` was launched, which in the
    // ordinary case is the project you are about to open panes on. Worth one toast: a repository
    // on a Windows drive is not broken, only slow enough that horde gets the blame for it.
    if let Ok(cwd) = std::env::current_dir() {
        if crate::platform::on_windows_drive(&cwd) {
            warnings.push(crate::platform::windows_drive_hint(&cwd.display().to_string()));
        }
    }

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

    // Annotated rather than pre-empted. A Windows drive is the likeliest reason a bind fails on
    // an otherwise fine path — those filesystems do not carry the socket type — but "likeliest"
    // is not "certain", and refusing up front would break anyone whose mount happens to work.
    // So the check costs nothing until something has already gone wrong, and then it names the
    // one thing the error message never will.
    let listener = UnixListener::bind(&socket).map_err(|e| {
        let base = anyhow!("could not bind {}: {e}", socket.display());
        if crate::platform::on_windows_drive(&socket) {
            base.context(
                "that path is on a Windows drive, which cannot host a unix socket — set \
                 HORDE_SOCKET to a path under your Linux home, e.g. HORDE_SOCKET=$HOME/.horde.sock",
            )
        } else {
            base
        }
    })?;

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
        repos: repo::Cache::default(),
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

    // Git state, on detection's cadence but with its own much longer staleness window inside
    // the cache. Piggybacking on `detect_due` rather than taking a timer of its own keeps the
    // fork-per-directory work on one clock.
    if detect_due {
        refresh_repos(eng);
        // On detection's cadence deliberately: exhaustion is read off the same screen snapshot
        // detection already takes, and a model that has just refused will still be refusing a
        // second later. Checking it every tick would buy nothing and cost a scan per pane.
        advance_spent_models(eng);
        nudge_handover(eng);
        succeed_exhausted(eng);
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
        // autonomous half is opt-in; see `agents.task_nudge` and `agents.board`.
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
    // Unconditional, unlike the nudge: handing a dead agent's task back is correctness, not
    // autonomy. Nothing new is started by it, and a claim left behind by a closed pane would
    // otherwise sit there blocking the task forever.
    if eng.board.claimed_count() > 0 {
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

/// Re-read the branch and dirty state of every directory the session can show.
///
/// Space cwds *and* pane cwds, which are not the same question once worktrees exist: two
/// agents in one project are on two different branches, and only the pane knows which.
///
/// The cache decides for itself what is stale, so calling this often is cheap; what it costs
/// is bounded by the number of distinct directories, not by how often it is asked.
/// How long after a switch to ignore exhaustion text.
///
/// The message that caused the switch is still in the scrollback afterwards. Without a pause,
/// one rate limit would walk an agent through every model in its list within a few ticks and
/// report the profile spent when only one model ever refused.
const SWITCH_QUIET: Duration = Duration::from_secs(30);

/// Match a phrase against a terminal screen, ignoring where the terminal broke the lines.
///
/// A pane in a multiplexer is narrow, and every agent TUI wraps to fit. `"Approaching usage
/// limit"` arrives as `Approaching` on one line and `usage limit` on the next; at the widths a
/// sidebar leaves, opencode splits *inside* words, so `esc to interrupt` becomes
/// `in`/`te`/`rr`/`up`/`t` down five lines. A plain `contains` finds neither, which is exactly
/// how the previous opencode manifest came to match nothing at all.
///
/// So both sides have their whitespace removed before comparing. That reads as a blunt
/// instrument and is the right one here: the alternative is every pattern silently depending on
/// the reader's pane width.
fn screen_says(screen: &str, phrase: &str) -> bool {
    fn squash(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }
    !phrase.trim().is_empty() && squash(screen).contains(&squash(phrase))
}

/// Spawn a successor for an agent that ran out without handing over.
///
/// The net under [`nudge_handover`]. That path spends an agent's last usable turn on writing its
/// own brief, which is always better — but it only works if a warning appeared and the agent was
/// in a state to act on it. An agent that stopped mid-sentence gets this instead.
///
/// horde has to write the brief here, and can only say what it watched: which agent this
/// replaces, where it was working, what git thinks changed, and the last thing on its screen.
/// That is less than the agent knew. It is also far more than a successor starting cold.
fn succeed_exhausted(eng: &mut Engine) {
    let cfg = eng.cfg.clone();
    if cfg.handover.exhausted.is_empty() {
        return;
    }
    let Some(profile_name) = cfg.handover.profile.clone() else { return };
    let Some(profile) = cfg.models.get(&profile_name).cloned() else {
        log_line(&format!("handover: no model profile {profile_name:?} to succeed with"));
        return;
    };
    let Some(cmd) = profile.command(0) else { return };

    // Chosen before spawning anything, so one pass never starts two.
    let mut candidate: Option<(PaneId, String, usize)> = None;
    for (id, pane) in eng.session.panes.iter() {
        let Some(agent) = pane.agent.as_ref() else { continue };
        if pane.succeeded || agent.class != crate::proto::AgentClass::Agent {
            continue;
        }
        if pane.succession_depth >= cfg.handover.max_chain {
            continue;
        }
        let screen = pane.detection_snapshot(cfg.detection_lines).join("\n");
        if cfg.handover.exhausted.iter().any(|pat| screen_says(&screen, pat)) {
            candidate = Some((*id, agent.name.clone(), pane.succession_depth));
            break;
        }
    }
    let Some((dead, name, depth)) = candidate else { return };

    // Counted against the same cap as everything else horde starts on its own initiative.
    let live = super::daemon::triggers::live_spawned(eng);
    if live >= cfg.max_spawned {
        if let Some(p) = eng.session.panes.get_mut(&dead) {
            p.succeeded = true; // Do not retry every tick against a cap that will not move.
        }
        log_line(&format!(
            "{name} ran out, but horde already runs {live} agents (triggers.max_spawned)"
        ));
        return;
    }

    let brief = compose_brief(eng, dead, &name);
    let successor = format!("{name}-next");
    let pane = match eng.session.split(&cfg, Some(dead), crate::proto::Dir::Right, Some(&cmd)) {
        Ok(p) => p,
        Err(e) => {
            log_line(&format!("could not start a successor for {name}: {e}"));
            return;
        }
    };
    if let Some(p) = eng.session.panes.get_mut(&pane) {
        p.name = Some(successor.clone());
        // Stamped so the cap counts it, and so a successor that also runs out is one step
        // further along a chain that has to end.
        p.spawned_by = Some(0);
        p.succession_depth = depth + 1;
        p.model = Some(pane::ModelRun {
            profile: profile_name.clone(),
            index: 0,
            switched: None,
        });
    }
    if let Some(p) = eng.session.panes.get_mut(&dead) {
        p.succeeded = true;
    }

    let by = format!("horde (for {name})");
    eng.bus.hold_for(&successor, &brief, &by);
    log_line(&format!("{name} ran out; started {successor} on {profile_name} to take over"));
    // Journalled so the digest can say the work changed hands. Waking to work done by a model
    // nobody chose, with nothing recording it, is this feature's worst outcome.
    eng.journal
        .note(journal::Kind::Notified, &format!("{name} ran out; {successor} took over"));
    eng.touch();
    eng.detect_now();
}

/// Everything horde knows about a dying agent, as a briefing for the one replacing it.
fn compose_brief(eng: &mut Engine, pane: PaneId, name: &str) -> String {
    let mut out = format!(
        "You are taking over from {name}, which ran out mid-task and could not brief you itself.\n"
    );
    let Some(p) = eng.session.panes.get(&pane) else { return out };
    let cwd = p.cwd.clone();
    // Screen read here, while the immutable borrow is still held; the git lookup below needs
    // the cache mutably and the two cannot overlap.
    let tail: Vec<String> = p
        .detection_snapshot(40)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    out.push_str(&format!("Working directory: {}\n", cwd.display()));

    // If the agent did leave a note, that beats everything below it — say so first.
    let note = cwd.join(format!(".horde/handoff-{name}.md"));
    if note.exists() {
        out.push_str(&format!("It left notes at {} — read those first.\n", note.display()));
    }

    if let Some(facts) = eng.repos.get(&cwd) {
        out.push_str(&format!(
            "Git: branch {}, working tree {}.\n",
            facts.branch,
            if facts.dirty { "DIRTY — it stopped mid-edit, check `git diff` before changing anything" } else { "clean" }
        ));
    }

    // The last thing it was doing, which is usually the most useful single fact.
    if !tail.is_empty() {
        out.push_str("\nThe last of its screen:\n");
        for l in tail {
            out.push_str(&format!("  {}\n", l.trim_end()));
        }
    }

    out.push_str(
        "\nRead before writing. Its work is unfinished, not wrong, and undoing it costs more \
         than finishing it.",
    );
    out
}

/// Tell an agent that is nearly out of budget to hand over, while it still can.
///
/// This is the half of succession the agent must do itself, because it is the only participant
/// that knows what it was doing — and the only moment it can is *before* it runs out. Afterwards
/// it cannot spawn, cannot write a note, cannot answer. So horde watches for the warning and
/// spends the agent's last usable turn on the handover rather than on work it will not finish.
///
/// horde does not spawn the successor here. The agent does, because the brief it writes about
/// its own half-finished work beats anything reconstructed from a screen.
fn nudge_handover(eng: &mut Engine) {
    let cfg = eng.cfg.clone();
    if cfg.handover.warning.is_empty() {
        return;
    }
    let Some(profile) = cfg.handover.profile.clone() else { return };

    let mut tell: Vec<(PaneId, String)> = Vec::new();
    for (id, pane) in eng.session.panes.iter_mut() {
        // Only something there is a conversation with. A dev server has no turn to spend.
        let Some(agent) = pane.agent.as_ref() else { continue };
        if pane.handover_told || agent.class != crate::proto::AgentClass::Agent {
            continue;
        }
        let name = agent.name.clone();
        let screen = pane.detection_snapshot(cfg.detection_lines).join("\n");
        if !cfg.handover.warning.iter().any(|w| screen_says(&screen, w)) {
            continue;
        }
        pane.handover_told = true;
        let body = cfg
            .handover
            .instruct
            .clone()
            .unwrap_or_else(|| crate::config::DEFAULT_INSTRUCT.to_string())
            .replace("{name}", &name)
            .replace("{profile}", &profile);
        tell.push((*id, body));
    }

    for (pane, body) in tell {
        let name = super::daemon::bus::Bus::sender_name(&eng.session, Some(pane));
        log_line(&format!("{name}: nearly out of budget — told to hand over"));
        eng.journal
            .note(journal::Kind::Notified, &format!("{name} told to hand over before running out"));
        let Engine { bus, session, .. } = eng;
        // Through the bus so it lands at the agent's prompt rather than mid-stream, and is
        // queued if it is busy — the same gating every other message gets.
        if let Err(e) = bus.send(session, &cfg, None, &name, &body, false, false, None) {
            log_line(&format!("{name}: could not send the handover instruction: {e}"));
        }
    }
}

/// Move any agent whose model has stopped serving it onto the next one in its profile.
///
/// horde cannot see an HTTP 429 — it has no HTTP client and that is deliberate. What it can see
/// is the pane, and an agent renders the provider's error into it. So exhaustion is read the
/// same way every other agent state is read: as text on a screen.
///
/// The switch is *typed into the running agent* rather than done by restarting it. A restart
/// would cost the agent's plan and everything it had read, which is a far higher price than the
/// rate limit itself.
fn advance_spent_models(eng: &mut Engine) {
    let cfg = eng.cfg.clone();
    if cfg.models.is_empty() {
        return;
    }
    let mut switches: Vec<(PaneId, String, String)> = Vec::new();
    let mut spent: Vec<(PaneId, String)> = Vec::new();

    for (id, pane) in eng.session.panes.iter_mut() {
        // Read everything needed from the pane before taking the mutable borrow on `model`:
        // the screen snapshot borrows the pane immutably and the two cannot overlap.
        let Some((profile_name, index, switched)) =
            pane.model.as_ref().map(|m| (m.profile.clone(), m.index, m.switched))
        else {
            continue;
        };
        if switched.is_some_and(|t| t.elapsed() < SWITCH_QUIET) {
            continue;
        }
        let Some(profile) = cfg.models.get(&profile_name) else { continue };
        let Some(switch) = profile.switch.as_ref() else { continue };

        let screen = pane.detection_snapshot(cfg.detection_lines).join("\n");
        if !profile.exhausted.iter().any(|pat| screen_says(&screen, pat)) {
            continue;
        }
        let Some(run) = pane.model.as_mut() else { continue };

        match profile.order.get(index + 1) {
            Some(next) => {
                run.index = index + 1;
                run.switched = Some(std::time::Instant::now());
                switches.push((*id, switch.replace("{model}", next), next.clone()));
            }
            // Deliberately not wrapping. A fleet that has spent every free model should say so;
            // going back to the one that just refused is a loop that looks like work.
            None => {
                run.switched = Some(std::time::Instant::now());
                spent.push((*id, profile_name.clone()));
            }
        }
    }

    for (pane, command, model) in switches {
        let name = super::daemon::bus::Bus::sender_name(&eng.session, Some(pane));
        // Journalled, because the alternative is waking to work done by a model you did not
        // choose with nothing saying so. Provenance is the thing this feature most easily loses.
        log_line(&format!("{name}: model spent, switching to {model}"));
        eng.journal.note(journal::Kind::Notified, &format!("{name} switched to {model}"));
        let Engine { bus, session, .. } = eng;
        if let Err(e) = bus.send(session, &cfg, None, &name, &command, false, false, None) {
            log_line(&format!("{name}: could not send the model switch: {e}"));
        }
    }
    for (pane, profile) in spent {
        let name = super::daemon::bus::Bus::sender_name(&eng.session, Some(pane));
        log_line(&format!("{name}: every model in profile {profile:?} is spent"));
        eng.journal
            .note(journal::Kind::Notified, &format!("{name} exhausted profile {profile}"));
    }
}

fn refresh_repos(eng: &mut Engine) {
    let mut dirs: Vec<std::path::PathBuf> =
        eng.session.spaces.iter().map(|s| s.cwd.clone()).collect();
    dirs.extend(eng.session.panes.values().map(|p| p.cwd.clone()));
    dirs.sort();
    dirs.dedup();
    for d in &dirs {
        eng.repos.get(d);
    }
    eng.repos.retain(|k| dirs.iter().any(|d| d == k));
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
        let mut s = eng.session.snapshot(&cfg, &eng.repos);
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
        let mut cursor = p.cursor();
        cursor.visible = cursor.visible && Some(*id) == focused;

        // A moved cursor is an update in its own right, not just a passenger on a changed row.
        // Typing a space onto a blank cell rebuilds an identical row, so nothing is dirty — and
        // skipping the pane here left the cursor a column behind until some later keystroke
        // altered a character, at which point it jumped two columns at once.
        let moved = p.last_sent_cursor != Some(cursor);
        if dirty.is_empty() && !moved {
            continue;
        }
        p.last_sent_cursor = Some(cursor);
        let rows: Vec<RowUpdate> = dirty
            .iter()
            .filter_map(|&y| p.row(y).map(|r| RowUpdate { y, row: r.clone() }))
            .collect();
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
    /// A temp path unique to this test binary.
    ///
    /// The logs a test engine writes were fixed names in `$TMPDIR`, which two checkouts of horde
    /// — or a second `cargo test` while the first is running — share. The board and bus recover
    /// state from those files on construction, so a test asserting a count would read another
    /// process's leftovers. Scoping by pid makes the collision impossible rather than unlikely.
    pub(super) fn test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("horde-test-{}-{name}", std::process::id()))
    }

    pub(super) fn engine() -> Engine {
        engine_with_shell(None)
    }

    /// An engine whose one pane runs `shell`, or the configured default when `None`.
    ///
    /// Worth the parameter: the default is the developer's own `$SHELL`, which prints a prompt
    /// at a width nobody can predict and does so *concurrently* with the test. Anything
    /// asserting on cursor columns has to run something silent — `cat` — or it is really
    /// asserting on how fast zsh started.
    pub(super) fn engine_with_shell(shell: Option<&str>) -> Engine {
        let mut cfg = Config::default();
        if let Some(s) = shell {
            cfg.shell = s.to_string();
        }
        // Present for every test engine so the env path is exercised rather than bypassed.
        cfg.env.insert("HORDE_ENV_TEST".into(), "sk-or-test".into());
        let session = Session::new(&cfg);
        let agents = agents::Detector::new(&cfg);
        let mut eng = Engine {
            session,
            bus: bus::Bus::new(test_path("bus.jsonl")),
            board: tasks::Board::new(test_path("tasks.jsonl")),
            triggers: triggers::Store::new(
                test_path("triggers.jsonl"),
            ),
            journal: journal::Journal::new(test_path("journal.jsonl")),
            pane_names: HashMap::new(),
            started: now_millis(),
            last_seen: 0,
            last_alert: 0,
            agents,
            repos: repo::Cache::default(),
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

    /// Type `bytes` into a pane and pump until the emulator has taken them in.
    ///
    /// Waits on the daemon's own view of the cursor rather than on a sleep, and returns whether
    /// it got there — so a test can tell "the terminal never saw the keystroke" apart from "the
    /// terminal saw it and the client was never told", which is the distinction the bug lives in.
    fn type_into(eng: &mut Engine, pane: PaneId, bytes: &[u8], want_x: u16) -> bool {
        eng.session.panes.get_mut(&pane).unwrap().write_input(bytes).unwrap();
        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&pane).unwrap().pump(&theme);
            if eng.session.panes[&pane].cursor().x == want_x {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// Broadcast, then report the cursor the client was actually told about.
    fn cursor_sent_to_client(
        eng: &mut Engine,
        rx: &mut mpsc::UnboundedReceiver<ServerFrame>,
    ) -> Option<crate::proto::CursorPos> {
        broadcast(eng);
        let mut last = None;
        while let Ok(frame) = rx.try_recv() {
            if let ServerFrame::Rows { cursor: Some(c), .. } = frame {
                last = Some(c);
            }
        }
        last
    }

    /// Typing a space has to move the cursor on screen.
    ///
    /// A space landing on an already-blank cell changes no text, so the rebuilt row is identical
    /// and nothing is marked dirty. `broadcast` skips a pane with no dirty rows entirely — and
    /// the cursor only ever travels *attached to* a row update. So the keystroke is invisible
    /// until some later keystroke happens to change a character, at which point the cursor jumps
    /// two columns at once. Reported as "space doesn't render until I type".
    #[test]
    fn a_keystroke_that_changes_no_text_still_moves_the_cursor() {
        // `cat` rather than a shell: it prints no prompt, so column 0 is column 0.
        let mut eng = engine_with_shell(Some("cat"));
        let pane = *eng.session.panes.keys().next().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new() });
        eng.session.focus_pane(pane);

        // A visible character first: this part already works, and it gets the client a cursor
        // to be wrong about.
        assert!(type_into(&mut eng, pane, b"a", 1), "the pty never echoed the first keystroke");
        let before = cursor_sent_to_client(&mut eng, &mut rx).expect("a printable char updates");
        assert_eq!(before.x, 1);

        // Now the space. The emulator must see it...
        assert!(type_into(&mut eng, pane, b" ", 2), "the pty never echoed the space");
        assert_eq!(eng.session.panes[&pane].cursor().x, 2, "the terminal knows where it is");

        // ...and so must the client.
        let after = cursor_sent_to_client(&mut eng, &mut rx);
        kill_all(&mut eng);
        assert_eq!(
            after.map(|c| c.x),
            Some(2),
            "the terminal moved the cursor to column 2 but the client was never told"
        );
    }

    /// A configured variable has to reach the program, not merely be stored.
    ///
    /// This is how a provider key gets to an agent, and the failure mode if it does not arrive is
    /// silent: the agent starts, cannot authenticate, and says so in its own words somewhere in
    /// its own UI. So the assertion goes all the way through a real PTY to a real child.
    #[test]
    fn configured_env_reaches_the_program_in_the_pane() {
        // `printenv VAR` rather than `env`: one variable, one line. A bare `env` prints more
        // than a pane has rows, and the answer scrolls off the top before anything can read it.
        // Neither can use `sh -c '...'` — `build_command` splits on whitespace and does not
        // honour quotes, so an argument containing spaces cannot be expressed here at all.
        let mut eng = engine_with_shell(Some("printenv HORDE_ENV_TEST"));
        let pane = *eng.session.panes.keys().next().unwrap();
        let theme = eng.cfg.theme.clone();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = String::new();
        while std::time::Instant::now() < deadline && !seen.contains("sk-or-test") {
            eng.session.panes.get_mut(&pane).unwrap().pump(&theme);
            seen = eng.session.panes[&pane].visible_text().join("");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        kill_all(&mut eng);
        // `printenv VAR` prints the value alone, so the value is the whole assertion.
        assert!(
            seen.contains("sk-or-test"),
            "the pane never saw the configured value; screen was {seen:?}"
        );
    }

    /// The silent-death path: no warning, no note, and a successor still appears — briefed with
    /// everything horde could see.
    #[test]
    fn an_agent_that_died_without_handing_over_gets_a_successor() {
        let mut eng = engine_with_shell(Some("echo reached your usage limit"));
        let dead = *eng.session.panes.keys().next().unwrap();
        eng.cfg.handover = crate::config::Handover {
            warning: Vec::new(),
            exhausted: vec!["reached your usage limit".into()],
            profile: Some("free".into()),
            instruct: None,
            max_chain: 3,
        };
        eng.cfg.models.insert(
            "free".into(),
            crate::config::ModelProfile {
                cmd: "cat {model}".into(),
                order: vec!["/dev/stdin".into()],
                exhausted: Vec::new(),
                switch: None,
            },
        );
        give_agent_named(&mut eng.session, dead, "builder");

        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&dead).unwrap().pump(&theme);
            if eng.session.panes[&dead].visible_text().join("").contains("usage limit") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let before = eng.session.panes.len();
        succeed_exhausted(&mut eng);
        assert_eq!(eng.session.panes.len(), before + 1, "a successor should exist");
        assert!(eng.session.panes[&dead].succeeded, "and the dead one is marked");

        let successor = eng
            .session
            .panes
            .values()
            .find(|p| p.name.as_deref() == Some("builder-next"))
            .expect("named after the agent it replaces");
        assert_eq!(successor.succession_depth, 1, "one step along the chain");

        // The brief is waiting for it, and says where the work is.
        let held = eng.bus.recent(20);
        let brief = held
            .iter()
            .find(|m| m.to == "builder-next")
            .expect("a brief was composed");
        assert!(brief.body.contains("taking over from builder"), "{}", brief.body);
        assert!(brief.body.contains("Working directory"), "{}", brief.body);

        // Running again must not start a second one.
        let now = eng.session.panes.len();
        succeed_exhausted(&mut eng);
        assert_eq!(eng.session.panes.len(), now, "one successor, not a queue of them");

        kill_all(&mut eng);
    }

    /// A lineage that keeps running out has to stop. If three agents in a row have run out, the
    /// answer is not a fourth.
    #[test]
    fn a_succession_chain_stops_at_its_limit() {
        let mut eng = engine_with_shell(Some("echo reached your usage limit"));
        let dead = *eng.session.panes.keys().next().unwrap();
        eng.cfg.handover = crate::config::Handover {
            warning: Vec::new(),
            exhausted: vec!["reached your usage limit".into()],
            profile: Some("free".into()),
            instruct: None,
            max_chain: 2,
        };
        eng.cfg.models.insert(
            "free".into(),
            crate::config::ModelProfile {
                cmd: "cat {model}".into(),
                order: vec!["/dev/stdin".into()],
                exhausted: Vec::new(),
                switch: None,
            },
        );
        give_agent_named(&mut eng.session, dead, "builder");
        // Already at the end of a chain.
        eng.session.panes.get_mut(&dead).unwrap().succession_depth = 2;

        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&dead).unwrap().pump(&theme);
            if eng.session.panes[&dead].visible_text().join("").contains("usage limit") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let before = eng.session.panes.len();
        succeed_exhausted(&mut eng);
        assert_eq!(eng.session.panes.len(), before, "the chain has run its length");
        kill_all(&mut eng);
    }

    /// Wrapping must not hide a phrase. This is why the previous opencode manifest, which
    /// looked for "esc to interrupt" as one string, never matched anything.
    #[test]
    fn a_phrase_is_found_however_the_terminal_broke_it() {
        assert!(screen_says("Rate limit exceeded", "Rate limit exceeded"));
        // Wrapped between words, which is what a narrow pane does to a sentence.
        assert!(screen_says("... Approaching\nusage limit ...", "Approaching usage limit"));
        // Wrapped inside words, which is what a very narrow pane does.
        assert!(screen_says("  esc to\n  in\n  te\n  rr\n  up\n  t", "esc to interrupt"));
        // And it still says no to text that is genuinely absent.
        assert!(!screen_says("all is well", "Rate limit exceeded"));
        assert!(!screen_says("anything at all", "   "));
    }

    /// The shipped patterns have to match what Claude Code actually prints.
    ///
    /// The limit line is quoted verbatim in anthropics/claude-code issues #9236 and #5977. This
    /// is the string the whole feature turns on, and it is the one thing that cannot be checked
    /// by running horde — so it is checked here instead, wrapped as a narrow pane would wrap it.
    #[test]
    fn the_shipped_patterns_match_what_claude_code_prints() {
        let real = "Claude usage limit reached. Your limit will reset at 3pm (America/New_York)";
        for pattern in ["usage limit reached", "Your limit will reset at"] {
            assert!(screen_says(real, pattern), "{pattern:?} should match {real:?}");
            // And still match once a narrow pane has broken it up.
            let wrapped = "Claude usage limit\nreached. Your limit\nwill reset at 3pm";
            assert!(screen_says(wrapped, pattern), "{pattern:?} should survive wrapping");
        }

        // The enterprise phrasing, which the help centre describes as "limit reached, resets at".
        assert!(screen_says("5-hour limit reached - resets 4pm", "limit reached - resets"));
        assert!(screen_says("limit reached, resets at 4pm", "limit reached, resets"));

        // And the warning tier.
        assert!(screen_says("Approaching 5-hour limit.", "Approaching 5-hour limit"));

        // What must *not* match: horde's own handover instruction mentions the usage limit, and
        // it lands on the very pane being watched. If that tripped the exhausted patterns, being
        // told to hand over would immediately count as having run out.
        let instruction = crate::config::DEFAULT_INSTRUCT;
        for pattern in ["usage limit reached", "Your limit will reset at"] {
            assert!(
                !screen_says(instruction, pattern),
                "horde's own instruction must not read as an exhausted agent: {pattern:?}"
            );
        }
    }

    /// An agent that is nearly out gets told to hand over — once, and with something usable.
    ///
    /// The turn it spends on this is its last usable one, so the instruction has to be concrete:
    /// what to write, where, and the exact command to start its successor.
    #[test]
    fn an_agent_running_out_is_told_to_hand_over_while_it_still_can() {
        let mut eng = engine_with_shell(Some("echo Approaching usage limit"));
        let pane = *eng.session.panes.keys().next().unwrap();
        eng.cfg.handover = crate::config::Handover {
            warning: vec!["Approaching usage limit".into()],
            exhausted: Vec::new(),
            profile: Some("free".into()),
            instruct: None,
            max_chain: 3,
        };
        give_agent_named(&mut eng.session, pane, "builder");

        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&pane).unwrap().pump(&theme);
            if eng.session.panes[&pane].visible_text().join("").contains("Approaching") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        nudge_handover(&mut eng);
        assert!(eng.session.panes[&pane].handover_told, "it should have been told");

        let sent = eng.bus.recent(5);
        let msg = sent.last().expect("an instruction went out");
        assert!(msg.body.contains("handoff-builder.md"), "names its own note: {}", msg.body);
        assert!(msg.body.contains("--profile free"), "names the successor profile: {}", msg.body);
        assert!(msg.body.contains("horde spawn"), "gives the actual command: {}", msg.body);

        // The warning stays on screen. Repeating the instruction would interrupt the handover
        // it is asking for.
        let before = eng.bus.recent(50).len();
        nudge_handover(&mut eng);
        assert_eq!(eng.bus.recent(50).len(), before, "told exactly once");

        kill_all(&mut eng);
    }

    /// A warning with nothing to hand over to is a half-configured feature, and firing it would
    /// spend an agent's last turn telling it to run a command that cannot work.
    #[test]
    fn a_handover_warning_without_a_profile_does_nothing() {
        let mut eng = engine_with_shell(Some("echo Approaching usage limit"));
        let pane = *eng.session.panes.keys().next().unwrap();
        eng.cfg.handover = crate::config::Handover {
            warning: vec!["Approaching usage limit".into()],
            exhausted: Vec::new(),
            profile: None,
            instruct: None,
            max_chain: 3,
        };
        give_agent_named(&mut eng.session, pane, "builder");
        nudge_handover(&mut eng);
        assert!(!eng.session.panes[&pane].handover_told);
        kill_all(&mut eng);
    }

    /// The whole feature, end to end: a model refuses, the agent is moved to the next one.
    ///
    /// Driven through a real pane whose program prints the provider error, because the claim is
    /// specifically that horde reads this off a screen — asserting on an in-memory string would
    /// test the `contains` call and nothing else.
    #[test]
    fn an_exhausted_model_moves_the_agent_to_the_next_one() {
        // `echo` so the pane's screen carries OpenRouter's real refusal wording.
        let mut eng = engine_with_shell(Some("echo Rate limit exceeded"));
        let pane = *eng.session.panes.keys().next().unwrap();
        eng.cfg.models.insert(
            "free".into(),
            crate::config::ModelProfile {
                cmd: "opencode --model openrouter/{model}".into(),
                order: vec!["first/model".into(), "second/model".into()],
                exhausted: vec!["Rate limit exceeded".into()],
                switch: Some("/models openrouter/{model}".into()),
            },
        );
        eng.session.panes.get_mut(&pane).unwrap().model =
            Some(crate::daemon::pane::ModelRun { profile: "free".into(), index: 0, switched: None });
        give_agent_named(&mut eng.session, pane, "builder");

        // Let the refusal reach the screen.
        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&pane).unwrap().pump(&theme);
            if eng.session.panes[&pane].visible_text().join("").contains("Rate limit") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        advance_spent_models(&mut eng);
        let run = eng.session.panes[&pane].model.clone().expect("still on a profile");
        assert_eq!(run.index, 1, "it should have moved to the second model");
        assert!(run.switched.is_some(), "and recorded when, so it does not fire again");

        // The error is still on screen. A second pass inside the quiet window must not walk it
        // through the rest of the list.
        advance_spent_models(&mut eng);
        assert_eq!(eng.session.panes[&pane].model.as_ref().unwrap().index, 1, "one switch, not two");

        kill_all(&mut eng);
    }

    /// A profile with nowhere left to go stops rather than wrapping.
    #[test]
    fn a_spent_profile_stops_instead_of_starting_over() {
        let mut eng = engine_with_shell(Some("echo Rate limit exceeded"));
        let pane = *eng.session.panes.keys().next().unwrap();
        eng.cfg.models.insert(
            "free".into(),
            crate::config::ModelProfile {
                cmd: "c {model}".into(),
                order: vec!["only/model".into()],
                exhausted: vec!["Rate limit exceeded".into()],
                switch: Some("/models {model}".into()),
            },
        );
        eng.session.panes.get_mut(&pane).unwrap().model =
            Some(crate::daemon::pane::ModelRun { profile: "free".into(), index: 0, switched: None });
        give_agent_named(&mut eng.session, pane, "builder");

        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&pane).unwrap().pump(&theme);
            if eng.session.panes[&pane].visible_text().join("").contains("Rate limit") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        advance_spent_models(&mut eng);
        // Still on the last model, not back at the start.
        assert_eq!(eng.session.panes[&pane].model.as_ref().unwrap().index, 0);
        kill_all(&mut eng);
    }

    /// Put a named, idle agent into a pane, standing in for a detection pass.
    pub(super) fn give_agent_named(session: &mut Session, pane: PaneId, name: &str) {
        session.panes.get_mut(&pane).unwrap().agent = Some(crate::daemon::state::AgentRuntime {
            kind: "claude".into(),
            name: name.to_string(),
            class: Default::default(),
            state: crate::proto::AgentState::Idle,
            since: std::time::Instant::now(),
            authority: "test".into(),
            reason: "t".into(),
            seen: false,
            session_id: None,
            queued: Vec::new(),
            question: None,
            activity: Default::default(),
            touched: Default::default(),
            nudged_since: None,
            alerted_since: None,
        });
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
            .snapshot(&cfg, &eng.repos)
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
            class: Default::default(),
            state: crate::proto::AgentState::Idle,
            since: std::time::Instant::now(),
            authority: "screen".into(),
            reason: "t".into(),
            seen: false,
            session_id: None,
            queued: Vec::new(),
            question: None,
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
            // Enlisted, because the nudge only ever speaks to volunteers now.
            pane.board = true;
            pane.agent = Some(state::AgentRuntime {
                kind: "claude".into(),
                name: format!("worker{i}"),
                class: Default::default(),
                state: crate::proto::AgentState::Idle,
                // Staggered, so "idle longest" is well defined.
                since: std::time::Instant::now() - Duration::from_secs(60 - i as u64),
                authority: "hook".into(),
                reason: "t".into(),
                seen: false,
                session_id: None,
                queued: Vec::new(),
                question: None,
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                alerted_since: None,
            });
        }
        eng
    }

    /// The space these fixtures' panes live in. Board work is scoped to a project, so a test
    /// that adds an unscoped task is testing that nothing happens.
    pub(super) fn fixture_space(eng: &Engine) -> String {
        eng.session.spaces[0].name.clone()
    }

    /// Add work to the fixture's own project.
    fn add_task(eng: &mut Engine, text: &str) {
        let space = fixture_space(eng);
        eng.board.add(text, "user", Some(&space)).unwrap();
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
        add_task(&mut eng, "write the tests");
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
            add_task(&mut eng, &format!("job {i}"));
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
        add_task(&mut eng, "single job");
        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "one task, one agent: {sent:?}");
        // The one idle longest is the most available.
        assert_eq!(sent[0].0, "worker0");
        kill_all(&mut eng);
    }

    /// The failure that made the board unusable, pinned.
    ///
    /// Work added in one project used to be offered to any idle agent anywhere, because the
    /// board had no scope and the nudge had none to respect. With two projects open the
    /// symptom is an agent in the wrong repository suddenly working on something you asked
    /// for somewhere else.
    #[test]
    fn work_in_one_project_is_never_offered_to_an_agent_in_another() {
        let mut eng = engine_with_idle_agents("scope", 1);
        let cfg = eng.cfg.clone();
        // A second project, with an enlisted idle agent of its own.
        let other = eng.session.create_space(&cfg, Some("elsewhere"), &std::env::temp_dir()).unwrap();
        let other_pane = *eng
            .session
            .panes
            .values()
            .find(|p| p.space == other)
            .map(|p| &p.id)
            .unwrap();
        {
            let p = eng.session.panes.get_mut(&other_pane).unwrap();
            p.board = true;
            p.agent = Some(state::AgentRuntime {
                kind: "claude".into(),
                name: "stranger".into(),
                class: Default::default(),
                state: crate::proto::AgentState::Idle,
                since: std::time::Instant::now() - Duration::from_secs(600),
                authority: "hook".into(),
                reason: "t".into(),
                seen: false,
                session_id: None,
                queued: Vec::new(),
                question: None,
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                alerted_since: None,
            });
        }

        // Work for the *first* project only. `stranger` has been idle ten times longer, so
        // under the old "whoever is idle longest" rule it would have won outright.
        add_task(&mut eng, "port the parser");
        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, "worker0", "the other project's agent must not be touched");
        kill_all(&mut eng);
    }

    /// The other half of it. An agent you opened to think with, sitting idle in the same
    /// project as a fleet, never volunteered for anything.
    #[test]
    fn an_agent_that_never_enlisted_is_left_alone() {
        let mut eng = engine_with_idle_agents("enlist", 2);
        // worker1 resigns; worker0 stays enlisted.
        let ids: Vec<PaneId> = eng.session.panes.keys().copied().collect();
        for id in ids {
            let named_worker1 = eng
                .session
                .panes
                .get(&id)
                .and_then(|p| p.agent.as_ref())
                .is_some_and(|a| a.name == "worker1");
            if named_worker1 {
                eng.session.panes.get_mut(&id).unwrap().board = false;
            }
        }
        add_task(&mut eng, "one");
        add_task(&mut eng, "two");
        add_task(&mut eng, "three");
        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "only the volunteer: {sent:?}");
        assert_eq!(sent[0].0, "worker0");
        kill_all(&mut eng);
    }

    /// A week-old task is not work waiting for an agent, it is something you forgot about.
    /// Offering it on the next restart is how a quiet morning turns into archaeology.
    #[test]
    fn a_task_old_enough_to_be_forgotten_stops_being_offered() {
        let mut eng = engine_with_idle_agents("stale", 1);
        let space = fixture_space(&eng);
        eng.board.add("from last week", "user", Some(&space)).unwrap();
        // Wind it back past the threshold.
        let id = eng.board.all()[0].id;
        eng.board.backdate_for_test(id, tasks::STALE_AFTER.as_millis() as u64 + 60_000);

        assert_eq!(eng.board.offered_to(&space), 0, "stale work is not offered");
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        // Still on the board, still readable, still claimable by name. Stale is not deleted.
        assert_eq!(eng.board.open_count(), 1);
        assert!(eng.board.claim("worker0", Some(id), Some(&space)).unwrap().is_some());
        kill_all(&mut eng);
    }

    /// The bug this pins, found by running it rather than by reasoning about it: "one agent
    /// per pass" is not the same as "one agent". Over successive detection passes every idle
    /// agent got told about a single task — four nudges for one job, three turns wasted.
    #[test]
    fn one_task_wakes_one_agent_even_across_many_passes() {
        let mut eng = engine_with_idle_agents("across-passes", 4);
        add_task(&mut eng, "the only job");
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
            add_task(&mut eng, &format!("job {i}"));
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
        add_task(&mut eng, "job");
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        kill_all(&mut eng);
    }

    /// The bug that broke the loop: an agent finishing a board task while unfocused lands in
    /// `done`, not `idle`. Excluding `done` meant each agent took exactly one task and then
    /// went quiet, with work still on the board. A board worker stays in the loop.
    #[test]
    fn a_board_worker_that_finished_while_unfocused_is_given_more() {
        let mut eng = engine_with_idle_agents("done-worker", 1);
        add_task(&mut eng, "first");
        add_task(&mut eng, "second");
        eng.board.claim("worker0", Some(1), None).unwrap();
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
        add_task(&mut eng, "job one");
        add_task(&mut eng, "job two");
        eng.board.claim("worker0", Some(1), None).unwrap();
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
        add_task(&mut eng, "job");
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        kill_all(&mut eng);
    }

    /// Having done something, an agent becomes available again — and by then the nudge is
    /// useful rather than noise.
    #[test]
    fn a_new_idle_period_earns_a_fresh_nudge() {
        let mut eng = engine_with_idle_agents("fresh", 1);
        add_task(&mut eng, "job one");
        add_task(&mut eng, "job two");
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
