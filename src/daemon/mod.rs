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
pub mod manifest;
pub mod pane;
pub mod persist;
pub mod pty;
pub mod rpc;
pub mod state;
pub mod tasks;
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
    pub journal: journal::Journal,
    /// Pane names as of the start of this tick. An exit event is emitted after the pane has
    /// already been removed, so the name has to have been captured before that.
    pane_names: HashMap<PaneId, String>,
    /// When this daemon started, unix millis. The fallback window for a first digest.
    pub started: u64,
    /// When you last read a digest. The window a digest covers, in other words — it advances
    /// only on a read, so ignoring digests widens the window instead of losing the history.
    pub last_seen: u64,
    pub agents: agents::Detector,
    clients: HashMap<ClientId, Client>,
    /// Set when the shape changed and clients need a fresh snapshot.
    dirty_shape: bool,
    /// Set when a pane appeared, so detection runs on the next tick instead of waiting for
    /// the slow cadence.
    detect_soon: bool,
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

    fn flush_bus(&mut self) -> Vec<Event> {
        let Engine { bus, session, cfg, .. } = self;
        bus.flush_queued(session, cfg)
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
        journal: journal::Journal::new(crate::config::journal_path()),
        pane_names: HashMap::new(),
        started: now_millis(),
        last_seen: 0,
        agents,
        cfg,
        clients: HashMap::new(),
        dirty_shape: true,
        detect_soon: true,
        pending_events: Vec::new(),
    };

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
        let now_attached = !eng.clients.is_empty();
        if now_attached != attached {
            attached = now_attached;
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
            eng.session.set_client_size(&cfg, cols, rows);
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
        Cmd::ApplyLayout { preset } => {
            if let Err(e) = eng.session.apply_preset(&cfg, &preset) {
                problems.push((NoticeLevel::Warn, e.to_string()));
            }
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
    if !exited.is_empty() && eng.session.spaces.is_empty() {
        // The last pane closed; recreate a space so horde is never left unusable.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = eng.session.create_space(&cfg, None, &cwd);
        eng.dirty_shape = true;
    }

    broadcast(eng);
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
    fn engine() -> Engine {
        let cfg = Config::default();
        let session = Session::new(&cfg);
        let agents = agents::Detector::new(&cfg);
        let mut eng = Engine {
            session,
            bus: bus::Bus::new(std::env::temp_dir().join("horde-test-bus.jsonl")),
            board: tasks::Board::new(std::env::temp_dir().join("horde-test-tasks.jsonl")),
            journal: journal::Journal::new(std::env::temp_dir().join("horde-test-journal.jsonl")),
            pane_names: HashMap::new(),
            started: now_millis(),
            last_seen: 0,
            agents,
            cfg,
            clients: HashMap::new(),
            dirty_shape: true,
            detect_soon: true,
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
}
