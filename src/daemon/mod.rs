//! The daemon: owns every PTY, every emulator, and the session shape.
//!
//! One task owns the `Session` outright and everything reaches it through a channel. That
//! avoids holding a mutex across awaits, and makes the tick loop the only place panes are
//! pumped — so pane damage is consumed exactly once per frame.

pub mod approvals;
pub mod agents;
pub mod bus;
pub mod digest;
pub mod files;
pub mod handoff;
pub mod journal;
pub mod kanban;
pub mod layout;
pub mod logfile;
pub mod lsp;

/// The most a pasted attachment may be.
///
/// Eight megabytes is a very large screenshot and a very small photograph. Anything past it
/// is a mistake — a video frame, a raw scan — and a vault is a directory on somebody's disk.
const MAX_ATTACHMENT: usize = 8 * 1024 * 1024;
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
pub mod vault;

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
    /// The window size this client last reported.
    ///
    /// Held per client because the session has one size and there can be several windows.
    /// Without it the last window to speak wins for good: attach a small one beside a large
    /// one, close the small one, and the large one is left drawing full-size rects around
    /// panes whose ttys are still the small window's shape.
    cols: u16,
    rows: u16,
}

pub struct Engine {
    pub cfg: Config,
    pub session: Session,
    pub bus: bus::Bus,
    pub board: tasks::Board,
    /// The personal board. Separate from `board` in every way that matters — see
    /// [`kanban`] — and touching it only through the one seam that hands a card over.
    pub kanban: kanban::Kanban,
    pub triggers: triggers::Store,
    /// What each blocked pane was last seen asking, so a prompt can be required to hold still
    /// before it is answered. See [`approvals`].
    pub approvals: approvals::Seen,
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
    /// Projects opened before, most recent first. What the dashboard offers to reopen.
    pub recents: Vec<persist::SavedRecent>,
    /// Branch and dirty state per directory, refreshed on its own slow cadence because each
    /// answer costs a fork. Read by every snapshot, written by nothing else.
    pub repos: repo::Cache,
    /// One note index per vault root, not per space: two spaces opened on the same project
    /// are looking at the same notes, and indexing them twice would be work done to produce
    /// two answers that must agree.
    pub vaults: HashMap<PathBuf, vault::Index>,
    /// Language servers, one per project and language, started only when a file that needs
    /// one is opened. Nothing in here exists unless `config.toml` asked for it.
    pub lsp: lsp::Registry,
    /// Absolute path to the name the client used for it, for documents the editor has open.
    lsp_paths: HashMap<PathBuf, String>,
    /// Which client is waiting on a completion. One at a time, because a second request means
    /// the cursor moved and the first answer is already wrong.
    lsp_asked: Option<ClientId>,
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

    /// How many projects the dashboard remembers.
    ///
    /// Small on purpose: this is a list you pick from by eye, and a screen of forgotten
    /// directories is a worse answer than the six you actually use.
    pub const MAX_RECENTS: usize = 12;

    /// Record that a project was opened or returned to, most recent first.
    ///
    /// Keyed on the directory, not the name: a space can be renamed, and renaming it should
    /// not make horde think you have two projects.
    pub fn remember_project(&mut self, name: &str, cwd: &std::path::Path) {
        let cwd = cwd.to_string_lossy().to_string();
        self.recents.retain(|r| r.cwd != cwd);
        self.recents.insert(
            0,
            persist::SavedRecent { name: name.to_string(), cwd, last_used: now_millis() },
        );
        self.recents.truncate(Self::MAX_RECENTS);
    }

    /// Move the focused project to the head of the recents list, if it is not already there.
    ///
    /// Cheap enough to call every tick because the common case is a string comparison that
    /// says "already first". That is also what keeps `last_used` from being rewritten sixty
    /// times a second — the list only changes when the project you are looking at does.
    pub fn note_focused_project(&mut self) {
        let Some(space) = self.session.focused_space.and_then(|id| self.session.space(id)) else {
            return;
        };
        let cwd = space.cwd.to_string_lossy().to_string();
        if self.recents.first().is_some_and(|r| r.cwd == cwd) {
            return;
        }
        let (name, cwd) = (space.name.clone(), space.cwd.clone());
        self.remember_project(&name, &cwd);
    }

    /// The whole snapshot, session shape plus everything only the daemon knows.
    ///
    /// One assembly point for both channels. The render path and the JSON `session.snapshot`
    /// used to build this separately, and the JSON one quietly reported zero open tasks and
    /// no armed triggers however many there were — the kind of drift that only shows up when
    /// someone believes the answer.
    pub fn snapshot(&self) -> crate::proto::Snapshot {
        let cfg = self.cfg.clone();
        let mut s = self.session.snapshot(&cfg, &self.repos);
        s.tasks_open = self.board.open_count();
        s.tasks_claimed = self.board.claimed_count();
        // Twelve hours ahead, which is "today" without any calendar arithmetic: a card due
        // today is stored at today's local noon, so anything at or before now-plus-twelve is
        // due today or already late, and tomorrow's noon is still out of reach.
        s.cards_due = self.kanban.due_count(now_millis() + 12 * 3_600_000);
        s.triggers_armed = if self.cfg.unattended { self.triggers.armed_count() } else { 0 };
        s.recents = self.recent_projects();
        for space in s.spaces.iter_mut() {
            space.notes = self.vault_for(space.id).map(|v| v.len());
            space.lsp = self.lsp_info(space.id);
        }
        s
    }

    /// Put a pasted picture in the vault's attachment folder.
    ///
    /// Jailed like everything else that arrives over a socket, and capped: bytes from a
    /// client are bytes from somewhere, and a vault is a directory on somebody's disk.
    pub fn vault_attach(&mut self, space: SpaceId, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        if bytes.is_empty() {
            return Err(anyhow!("nothing to attach"));
        }
        if bytes.len() > MAX_ATTACHMENT {
            return Err(anyhow!(
                "{} bytes; the limit is {MAX_ATTACHMENT}",
                bytes.len()
            ));
        }
        // The name is a filename, never a path. `safe_join` would catch a traversal anyway;
        // this makes one impossible to express rather than merely refused.
        let file = std::path::Path::new(name)
            .file_name()
            .ok_or_else(|| anyhow!("not a filename"))?
            .to_string_lossy()
            .to_string();
        let root = self.vault_root_for_write(space)?;
        let dir = root.join(crate::client::image::ATTACHMENTS);
        std::fs::create_dir_all(&dir)?;
        let path = vault::safe_join(&dir, &file)?;
        std::fs::write(&path, bytes)?;
        self.touch();
        Ok(path)
    }

    /// Where a document the editor has open actually is, and what the client calls it.
    ///
    /// `vault` picks the root: a note is relative to the vault and a file to the project, and
    /// only the client knows which of the two it opened.
    fn doc_path(&self, space: SpaceId, rel: &str, vault: bool) -> Option<PathBuf> {
        let root = if vault {
            self.vault_root(space)?
        } else {
            self.session.space(space)?.cwd.clone()
        };
        vault::safe_join(&root, rel).ok()
    }

    /// Hand a buffer to whichever language server wants it.
    pub fn doc_changed(&mut self, space: SpaceId, rel: &str, body: &str, vault: bool) {
        let Some(path) = self.doc_path(space, rel, vault) else { return };
        let Some(lang) = lsp::language_for(&self.cfg, &path) else { return };
        let Some(root) = self.session.space(space).map(|s| s.cwd.clone()) else { return };
        // Remembered so diagnostics can come back named the way the client named it. The
        // daemon deals in absolute paths and the editor knows a relative one; the translation
        // has to happen somewhere, and here is where both are in hand.
        self.lsp_paths.insert(path.clone(), rel.to_string());
        let cfg = self.cfg.clone();
        self.lsp.did_open(&cfg, &root, &lang, &path, body);
    }

    /// The editor closed. Stop analysing what nobody is looking at.
    pub fn doc_closed(&mut self, space: SpaceId, rel: &str, vault: bool) {
        let Some(path) = self.doc_path(space, rel, vault) else { return };
        let Some(lang) = lsp::language_for(&self.cfg, &path) else { return };
        let Some(root) = self.session.space(space).map(|s| s.cwd.clone()) else { return };
        self.lsp_paths.remove(&path);
        self.lsp.did_close(&root, &lang, &path);
    }

    /// The language servers running for a project, as the chrome shows them.
    fn lsp_info(&self, space: SpaceId) -> Vec<crate::proto::LspInfo> {
        let Some(cwd) = self.session.space(space).map(|s| s.cwd.clone()) else { return Vec::new() };
        self.lsp
            .serving()
            .into_iter()
            .filter(|((root, _), _)| *root == cwd)
            .map(|((_, lang), s)| {
                let (errors, warnings) = s.counts();
                let (state, detail) = match &s.state {
                    lsp::State::Starting => (crate::proto::LspState::Starting, None),
                    lsp::State::Ready => (crate::proto::LspState::Ready, None),
                    lsp::State::Waiting(w) => (crate::proto::LspState::Waiting, Some(w.clone())),
                    lsp::State::Failed(w) => (crate::proto::LspState::Failed, Some(w.clone())),
                };
                crate::proto::LspInfo {
                    lang: lang.clone(),
                    state,
                    open: s.open_count(),
                    errors,
                    warnings,
                    // A server that failed without saying anything on stdout said it on
                    // stderr, which is the only place the reason exists. Failing that, name
                    // what horde actually tried to run — which is usually the whole answer.
                    detail: detail
                        .or_else(|| s.last_error())
                        .or_else(|| Some(s.command.clone()))
                        .filter(|_| !matches!(s.state, lsp::State::Ready)),
                }
            })
            .collect()
    }

    /// Where a space's notes live: its own vault if it has one, else the home vault.
    pub fn vault_root(&self, space: SpaceId) -> Option<PathBuf> {
        self.session
            .space(space)
            .and_then(|s| vault::locate(&s.cwd, &self.cfg.vault_dir))
            .or_else(|| self.cfg.vault_home.is_dir().then(|| self.cfg.vault_home.clone()))
    }

    /// Where a note is about to be written, making the vault if there is not one yet.
    ///
    /// Creating a directory on somebody's disk unasked would be rude, so nothing does it at
    /// startup — but asking for a note *is* the ask. Without this the home vault is a
    /// promise that only holds once you have already made the directory by hand, which is
    /// exactly the wrong way round.
    fn vault_root_for_write(&mut self, space: SpaceId) -> Result<PathBuf> {
        if let Some(root) = self.vault_root(space) {
            return Ok(root);
        }
        let home = self.cfg.vault_home.clone();
        vault::init(&home).with_context(|| format!("could not make a vault at {}", home.display()))?;
        self.notice(NoticeLevel::Info, format!("made a vault at {}", home.display()));
        refresh_vaults(self);
        Ok(home)
    }

    /// The index a space is reading. Falls back to the home vault, so a project with no
    /// notes of its own is still somewhere you can write one.
    pub fn vault_for(&self, space: SpaceId) -> Option<&vault::Index> {
        self.vaults.get(&self.vault_root(space)?)
    }

    /// Write a note, creating it if it does not exist, and reindex at once.
    ///
    /// Synchronous reindex rather than waiting for the next scan: the daemon did the writing,
    /// so there is nothing to discover — and a note that does not appear in its own vault
    /// until a timer fires is a note you cannot trust the index about.
    pub fn vault_write(&mut self, space: SpaceId, rel: &str, body: &str) -> Result<PathBuf> {
        self.vault_put(space, rel, body, None, false)
    }

    /// Write a note, with the rules that apply when the writer might not be a person.
    ///
    /// `by` is who to credit; `Some` also means "this is not a human's own note", which
    /// decides both the folder it defaults into and whether it gets stamped. `append` adds to
    /// what is there rather than replacing it, which is what makes a dated note a log rather
    /// than the last thing that happened to be written to it.
    pub fn vault_put(
        &mut self,
        space: SpaceId,
        rel: &str,
        body: &str,
        by: Option<&str>,
        append: bool,
    ) -> Result<PathBuf> {
        if body.len() > vault::MAX_NOTE {
            return Err(anyhow!(
                "note is {} bytes; the limit is {}. Something is writing in a loop.",
                body.len(),
                vault::MAX_NOTE
            ));
        }
        let root = self.vault_root_for_write(space)?;
        // An agent's note goes in its own folder unless it named one. Not enforced against a
        // caller that gives a path — an agent asked to write somewhere specific should be
        // able to — but the default is the safe one, which is what defaults are for.
        let rel = match by {
            Some(_) if !rel.contains('/') => format!("{}/{rel}", vault::AGENT_DIR),
            _ => rel.to_string(),
        };
        let path = vault::safe_join(&root, &rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let text = match (append, existing.is_empty()) {
            (true, false) => {
                // A blank line between, so appended sections read as separate entries rather
                // than as one that ran on.
                let joined = format!("{}\n\n{}", existing.trim_end(), body.trim_start());
                if joined.len() > vault::MAX_NOTE {
                    return Err(anyhow!("appending would take the note past {} bytes", vault::MAX_NOTE));
                }
                joined
            }
            _ => match by {
                Some(who) => {
                    vault::attribute(body, who, &triggers::local_date(now_millis()))
                }
                None => body.to_string(),
            },
        };
        std::fs::write(&path, &text)?;
        if let Some(idx) = self.vaults.get_mut(&root) {
            idx.refresh(usize::MAX);
        }
        self.touch();
        Ok(path)
    }

    /// Answer a vault question. `None` when the space has no vault to ask.
    pub fn vault_answer(
        &self,
        space: SpaceId,
        kind: &crate::proto::VaultQuery,
    ) -> Option<crate::proto::VaultReply> {
        use crate::proto::{NoteLine, VaultQuery};
        let idx = self.vault_for(space)?;
        let line = |id: vault::NoteId| -> Option<NoteLine> {
            let n = idx.note(id)?;
            Some(NoteLine {
                path: n.path.to_string_lossy().to_string(),
                title: n.title.clone(),
                tags: n.tags.clone(),
                mtime: n.mtime,
                backlinks: idx.backlinks(id).len(),
            })
        };

        let mut reply = crate::proto::VaultReply {
            space,
            root: idx.root.to_string_lossy().to_string(),
            notes: Vec::new(),
            body: None,
            backlinks: Vec::new(),
            graph: None,
            tasks: Vec::new(),
        };
        match kind {
            VaultQuery::List => reply.notes = idx.search("").into_iter().filter_map(line).collect(),
            VaultQuery::Search { q } => {
                reply.notes = idx.search(q).into_iter().filter_map(line).collect()
            }
            VaultQuery::Note { path } => {
                let id = idx.notes.iter().position(|n| n.path.to_string_lossy() == path.as_str())?;
                reply.notes = line(id).into_iter().collect();
                // Read from disk rather than keeping bodies in memory: the index is a map of
                // the vault, not a copy of it.
                reply.body = std::fs::read_to_string(idx.root.join(path)).ok();
                reply.backlinks = idx.backlinks(id).iter().filter_map(|b| line(*b)).collect();
                // A task may name this note by its filename or by any title it answers to,
                // because that is how a link written by hand names it.
                let n = &idx.notes[id];
                let mut names = vec![n.stem(), n.title.clone()];
                names.extend(n.aliases.iter().cloned());
                reply.tasks = self
                    .board
                    .about(&names)
                    .into_iter()
                    .map(|t| crate::proto::TaskLine {
                        id: t.id,
                        text: t.text.clone(),
                        owner: t.owner.clone(),
                        result: t.result.clone(),
                        dropped: matches!(t.state, tasks::TaskState::Dropped),
                        done: !t.is_open() && !t.is_claimed(),
                    })
                    .collect();
            }
            VaultQuery::Graph => reply.graph = Some(idx.graph()),
        }
        Some(reply)
    }

    /// The personal board, as of now.
    ///
    /// `space` is a *client-side* id, resolved to a project name here because that is what a
    /// card holds — see [`kanban::Card::project`]. `None` means every project, which is a
    /// board you are reading rather than a board you are working in.
    pub fn kanban_answer(&self, space: Option<SpaceId>) -> crate::proto::KanbanReply {
        let project = space.and_then(|s| self.session.space(s)).map(|s| s.name.clone());
        crate::proto::KanbanReply {
            cards: self.kanban.all().to_vec(),
            columns: self.cfg.kanban_columns.clone(),
            project,
        }
    }

    /// Send the board to one client, after it asked or after it changed something.
    fn send_kanban(&self, to: ClientId, space: Option<SpaceId>) {
        let reply = self.kanban_answer(space);
        if let Some(c) = self.clients.get(&to) {
            let _ = c.out.send(ServerFrame::Kanban(Box::new(reply)));
        }
    }

    /// The name that goes on a comment you wrote.
    ///
    /// `user@host`, because a board that will also carry agent-written comments needs the
    /// human ones to be recognisably a person rather than another process called `user`.
    /// Overridable, since a machine's real hostname is often uglier than what you would
    /// choose to sign your own notes with.
    pub fn local_user(&self) -> String {
        if let Some(name) = &self.cfg.kanban_author {
            return name.clone();
        }
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "user".into());
        match hostname() {
            Some(h) => format!("{user}@{h}"),
            None => user,
        }
    }

    /// How many files the browser will list before saying there are too many.
    const FILE_LIMIT: usize = 4_000;

    /// A project's files.
    pub fn file_list(&self, space: SpaceId) -> Option<crate::proto::FileList> {
        let root = self.session.space(space)?.cwd.clone();
        let (files, truncated) = files::list(&root, Self::FILE_LIMIT);
        Some(crate::proto::FileList {
            space,
            root: root.to_string_lossy().to_string(),
            files: files.iter().map(|f| f.path.to_string_lossy().to_string()).collect(),
            truncated,
            body: None,
            path: None,
        })
    }

    /// One project file's text.
    pub fn file_read(&self, space: SpaceId, rel: &str) -> Result<crate::proto::FileList> {
        let root = self.session.space(space).ok_or_else(|| anyhow!("no such space"))?.cwd.clone();
        let path = vault::safe_join(&root, rel)?;
        let body = std::fs::read_to_string(&path)?;
        Ok(crate::proto::FileList {
            space,
            root: root.to_string_lossy().to_string(),
            files: Vec::new(),
            truncated: false,
            body: Some(body),
            path: Some(rel.to_string()),
        })
    }

    /// Write a project file, jailed to the project — the same rule notes get, for the same
    /// reason: a path arriving from a text field must not be able to name anything else.
    pub fn file_write(&mut self, space: SpaceId, rel: &str, body: &str) -> Result<()> {
        let root = self.session.space(space).ok_or_else(|| anyhow!("no such space"))?.cwd.clone();
        let path = vault::safe_join(&root, rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, body)?;
        Ok(())
    }

    /// Tell a language server about a file, starting one if this is the first of its
    /// language in this project.
    ///
    /// Does nothing at all unless `config.toml` declares a server for the language — which is
    /// the promise that opening horde spawns nothing you did not ask for.
    pub fn lsp_sync(&mut self, space: SpaceId, rel: &str, text: &str) {
        let Some(root) = self.session.space(space).map(|s| s.cwd.clone()) else { return };
        let Ok(path) = vault::safe_join(&root, rel) else { return };
        let Some(lang) = lsp::language_for(&self.cfg, &path) else { return };
        let cfg = self.cfg.clone();
        // `did_open` on a file already open is a change, so this one call is both.
        self.lsp.did_open(&cfg, &root, &lang, &path, text);
    }

    /// The recents list as the wire carries it, with the ones already on screen marked.
    ///
    /// `live` is computed here rather than stored, because whether a project is open is a
    /// fact about right now and the list outlives every session it describes.
    fn recent_projects(&self) -> Vec<crate::proto::RecentProject> {
        self.recents
            .iter()
            .map(|r| crate::proto::RecentProject {
                name: r.name.clone(),
                cwd: r.cwd.clone(),
                last_used: r.last_used,
                live: self.session.spaces.iter().any(|s| s.cwd.to_string_lossy() == r.cwd),
            })
            .collect()
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

    /// Hand over every card whose armed window has arrived.
    ///
    /// Run from the same slow sweep as [`Self::nudge_for_tasks`], because both are the same
    /// question asked on a clock — is there something an agent should know about — and a
    /// second timer would be a second thing to get wrong.
    ///
    /// Gated on `agents.board`, which is the promise that switch already makes: turning the
    /// agents' board off has to turn off everything that puts work on it, or the setting is a
    /// lie told in one place and honoured in another.
    fn hand_over_due_cards(&mut self) {
        if !self.cfg.board {
            return;
        }
        for card in self.kanban.ready_to_hand_over(now_millis()) {
            match hand_over(self, card) {
                Ok(task) => log_line(&format!("kanban: card #{card} handed over as task #{task}")),
                // Logged rather than surfaced: this fires on a timer with nobody necessarily
                // watching, and a toast for something that will be retried on the next sweep
                // is noise. The card's own thread is where the record belongs.
                Err(e) => log_line(&format!("kanban: card #{card} could not be handed over: {e}")),
            }
        }
    }

    /// The roles of the agents enlisted for board work in one project.
    ///
    /// What decides whether role-tagged work can be taken by anybody here. Enlistment is part of
    /// the question, not decoration: a reviewer sitting in the project who never ran
    /// `horde task work` is not going to claim anything, so counting it would report a task as
    /// covered when nothing will ever pick it up.
    pub fn roles_enlisted_in(&self, space_name: &str) -> Vec<String> {
        let Some(space) = self.session.spaces.iter().find(|s| s.name == space_name) else {
            return Vec::new();
        };
        self.session
            .panes
            .values()
            .filter(|p| p.space == space.id && p.board && p.exited.is_none())
            .filter(|p| p.agent.is_some())
            .filter_map(|p| p.role.clone())
            .collect()
    }

    /// Tell one enlisted agent in `space` that its project has work waiting *for it*.
    ///
    /// This is horde's dispatcher, and it is deliberately not an agent. It knows which project a
    /// task belongs to, which role may take it, who is enlisted, who is already holding
    /// something, and who has been idle longest — and it decides in the daemon, where the answer
    /// is the same every time and cannot be blocked at a permission prompt.
    fn nudge_one(&mut self, space: SpaceId, space_name: &str) -> Option<Event> {
        if self.board.offered_to(space_name) == 0 {
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
        let candidates: Vec<Candidate> = self
            .session
            .panes
            .values()
            .filter(|p| p.space == space && p.board && p.exited.is_none())
            .filter_map(|p| p.agent.as_ref().map(|a| (p, a)))
            .filter(|(_, a)| eligible_state(a, &board_workers))
            .filter(|(_, a)| a.queued.is_empty())
            .filter(|(_, a)| !holding.contains(&a.name))
            .map(|(p, a)| Candidate {
                pane: p.id,
                name: a.name.clone(),
                role: p.role.clone(),
                since: a.since,
            })
            .collect();

        // Whether an agent has been woken already and not yet acted. Such an agent is about to
        // consume a task, so it counts against the work available — otherwise "one per pass"
        // simply wakes every idle agent over successive passes, which is the waste this is meant
        // to avoid. Observed: one task, four idle agents, four nudges.
        let told = |p: &crate::daemon::pane::Pane| {
            p.agent.as_ref().is_some_and(|a| a.nudged_since == Some(a.since))
        };

        // Accounted per role, because the work is. Five reviewer tasks are not five tasks for a
        // builder, and one pool of numbers would either wake a builder for work it cannot claim
        // or hold back a reviewer because somebody else is busy.
        let engaged_like = |role: Option<&str>| -> usize {
            self.session
                .panes
                .values()
                .filter(|p| p.space == space && p.board && p.exited.is_none())
                .filter(|p| p.role.as_deref() == role)
                .filter(|p| {
                    p.agent.as_ref().is_some_and(|a| holding.contains(&a.name))
                        || (told(p) && p.agent.as_ref().is_some_and(|a| eligible_state(a, &board_workers)))
                })
                .count()
        };

        // General hands first. An agent kept for anything is the cheapest one to spend on work
        // that named nobody, and spending a specialist on it is how the one task only they could
        // have taken ends up waiting for them.
        let mut order = candidates;
        order.sort_by_key(|c| (c.role.is_some(), c.since));

        let chosen = order.into_iter().find(|c| {
            // Not already woken and still waiting to act on it.
            let fresh = self
                .session
                .panes
                .get(&c.pane)
                .and_then(|p| p.agent.as_ref())
                .is_some_and(|a| a.nudged_since != Some(c.since));
            let mine = self.board.offered_to_agent(space_name, c.role.as_deref());
            fresh && mine > engaged_like(c.role.as_deref())
        })?;
        let Candidate { pane, name, role, since } = chosen;
        let waiting = self.board.offered_to_agent(space_name, role.as_deref());

        // Marked before sending, so a failure cannot produce a nudge loop.
        if let Some(a) = self.session.panes.get_mut(&pane).and_then(|p| p.agent.as_mut()) {
            a.nudged_since = Some(since);
        }

        // Says what it is being offered, not what the board holds. An agent told "9 tasks
        // waiting" that then claims twice and gets nothing has been lied to, and the honest
        // number is the one it can act on.
        let body = format!(
            "{waiting} task{} on the {space_name} board {}. Run `horde task claim` to take \
             the next one, do it, then `horde task done --result \"<what happened>\"`. Repeat \
             while it keeps returning work.",
            if waiting == 1 { "" } else { "s" },
            match &role {
                Some(r) => format!("for {r}"),
                None => "waiting".to_string(),
            }
        );
        let Engine { bus, session, cfg, .. } = self;
        match bus.send(session, cfg, bus::Outgoing::plain(None, &name, &body)) {
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
        kanban: kanban::Kanban::new(crate::config::kanban_path()),
        triggers: triggers::Store::new(crate::config::triggers_path()),
        approvals: approvals::Seen::default(),
        journal: journal::Journal::new(crate::config::journal_path()),
        pane_names: HashMap::new(),
        started: now_millis(),
        last_seen: 0,
        last_alert: 0,
        agents,
        recents: Vec::new(),
        repos: repo::Cache::default(),
        vaults: HashMap::new(),
        lsp: lsp::Registry::new(),
        lsp_paths: HashMap::new(),
        lsp_asked: None,
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

    // Children do not outlive the daemon that started them. `kill_on_drop` would mostly
    // handle it, but "mostly" is how a stopped horde leaves a rust-analyzer holding a
    // gigabyte until the machine reboots.
    eng.lsp.shutdown();
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
            eng.clients.insert(id, Client { out, needs_full: panes, cols, rows });
            resync_client_size(eng);
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
            resync_client_size(eng);
        }
        DaemonMsg::Frame { id, frame } => handle_client_frame(eng, id, frame),
    }
    false
}

/// Re-derive the session size from the windows still attached, and make them all repaint.
///
/// The size is the smallest attached window rather than the most recent one to speak. Two
/// windows share one set of ttys, so anything wider or taller than the narrowest of them is
/// content that window can only see part of. Recomputed on attach and detach as well as on a
/// resize, because closing the small window is the moment the large one is allowed to grow
/// back and nothing else would ever tell the daemon so.
fn resync_client_size(eng: &mut Engine) {
    let Some((cols, rows)) = eng
        .clients
        .values()
        .map(|c| (c.cols, c.rows))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
    else {
        // Nothing is watching. Keep the last size so a reattach comes back to its layout.
        return;
    };
    // Applied immediately so the layout tracks the drag, but remembered as pending so the tick
    // can settle it afterwards. Dragging a window edge delivers dozens of sizes a second, and a
    // program that repaints for each of them is repainting for a size already out of date.
    let cfg = eng.cfg.clone();
    if !eng.session.set_client_size(&cfg, cols, rows) {
        return;
    }
    eng.resize_settling = Some(std::time::Instant::now());
    // Every pane moved, so nothing any client has cached is still valid.
    let panes: Vec<PaneId> = eng.session.panes.keys().copied().collect();
    for p in &panes {
        if let Some(pane) = eng.session.panes.get_mut(p) {
            pane.request_full_repaint();
        }
    }
    for c in eng.clients.values_mut() {
        c.needs_full = panes.clone();
    }
    eng.dirty_shape = true;
}

fn handle_client_frame(eng: &mut Engine, id: ClientId, frame: ClientFrame) {
    match frame {
        ClientFrame::Ping => {}
        ClientFrame::Detach => {
            eng.clients.remove(&id);
            resync_client_size(eng);
        }
        ClientFrame::Resize { cols, rows } => {
            if let Some(c) = eng.clients.get_mut(&id) {
                c.cols = cols;
                c.rows = rows;
            }
            resync_client_size(eng);
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
        // The second command with a result rather than an effect, and answered the same
        // way: to the client that asked, never broadcast.
        ClientFrame::Command(Cmd::VaultQuery { space, kind }) => {
            if let Some(reply) = eng.vault_answer(space, &kind) {
                if let Some(c) = eng.clients.get(&id) {
                    let _ = c.out.send(ServerFrame::Vault(Box::new(reply)));
                }
            }
        }
        // Writes answer the caller with what is now on disk, so a view never shows a note
        // it merely hoped had been saved.
        ClientFrame::Command(Cmd::VaultSave { space, path, body }) => {
            match eng.vault_write(space, &path, &body) {
                Ok(_) => {
                    if let Some(reply) =
                        eng.vault_answer(space, &crate::proto::VaultQuery::Note { path })
                    {
                        if let Some(c) = eng.clients.get(&id) {
                            let _ = c.out.send(ServerFrame::Vault(Box::new(reply)));
                        }
                    }
                }
                Err(e) => eng.notice(NoticeLevel::Error, format!("could not save the note: {e}")),
            }
        }
        ClientFrame::Command(Cmd::VaultInit { space }) => {
            // Where a vault *would* go: the project's configured notes directory, or the
            // home vault when the project has no opinion.
            let root = eng
                .session
                .space(space)
                .map(|s| s.cwd.join(&eng.cfg.vault_dir))
                .unwrap_or_else(|| eng.cfg.vault_home.clone());
            match vault::init(&root) {
                Ok(fresh) => {
                    let msg = if fresh {
                        format!("made a vault at {}", root.display())
                    } else {
                        format!("{} is already a vault", root.display())
                    };
                    eng.notice(NoticeLevel::Info, msg);
                    refresh_vaults(eng);
                }
                Err(e) => eng.notice(NoticeLevel::Error, format!("could not make a vault: {e}")),
            }
        }
        // -- the kanban ------------------------------------------------------
        //
        // Every one of these answers with the whole board rather than with what it changed.
        // The client holds one authoritative picture and replaces it, exactly as it does with
        // `Snapshot`, so a card cannot end up drawn in a state it was never in. A board is a
        // few hundred cards; a diff protocol would be a second source of truth to keep honest
        // for no measurable gain.
        //
        // A failed write says so and re-sends the board unchanged, because the alternative —
        // staying quiet — leaves the view showing the move it optimistically drew.
        ClientFrame::Command(Cmd::KanbanQuery { space }) => eng.send_kanban(id, space),
        ClientFrame::Command(Cmd::CardNew { space, column, title }) => {
            let project = space.and_then(|s| eng.session.space(s)).map(|s| s.name.clone());
            match eng.kanban.add(&title, &column, project.as_deref()) {
                Ok(_) => eng.touch(),
                Err(e) => eng.notice(NoticeLevel::Warn, format!("could not add the card: {e}")),
            }
            eng.send_kanban(id, space);
        }
        ClientFrame::Command(Cmd::CardEdit { id: card, patch }) => {
            if let Err(e) = eng.kanban.edit(card, &patch) {
                eng.notice(NoticeLevel::Warn, format!("could not change the card: {e}"));
            }
            eng.touch();
            eng.send_kanban(id, eng.session.focused_space);
        }
        ClientFrame::Command(Cmd::CardMove { id: card, column, after }) => {
            if let Err(e) = eng.kanban.place(card, &column, after) {
                eng.notice(NoticeLevel::Warn, format!("could not move the card: {e}"));
            }
            eng.touch();
            eng.send_kanban(id, eng.session.focused_space);
        }
        ClientFrame::Command(Cmd::CardComment { id: card, body }) => {
            let by = eng.local_user();
            if let Err(e) = eng.kanban.comment(card, &by, &body) {
                eng.notice(NoticeLevel::Warn, format!("could not add the comment: {e}"));
            }
            eng.touch();
            eng.send_kanban(id, eng.session.focused_space);
        }
        ClientFrame::Command(Cmd::CardArchive { id: card, archived }) => {
            if let Err(e) = eng.kanban.archive(card, archived) {
                eng.notice(NoticeLevel::Warn, format!("could not archive the card: {e}"));
            }
            eng.touch();
            eng.send_kanban(id, eng.session.focused_space);
        }
        ClientFrame::Command(Cmd::CardHandOff { id: card }) => {
            match hand_over(eng, card) {
                Ok(task) => eng.notice(
                    NoticeLevel::Info,
                    format!("card #{card} is on the agents' board as task #{task}"),
                ),
                Err(e) => eng.notice(NoticeLevel::Warn, format!("{e}")),
            }
            eng.send_kanban(id, eng.session.focused_space);
        }
        ClientFrame::Command(Cmd::ColumnRename { from, to }) => {
            eng.kanban.rename_column(&from, &to);
            eng.touch();
            eng.send_kanban(id, eng.session.focused_space);
        }

        // The project's own files. Answered to the asking client, like every other query.
        ClientFrame::Command(Cmd::FileQuery { space }) => {
            if let Some(reply) = eng.file_list(space) {
                if let Some(c) = eng.clients.get(&id) {
                    let _ = c.out.send(ServerFrame::Files(Box::new(reply)));
                }
            }
        }
        ClientFrame::Command(Cmd::FileRead { space, path }) => {
            match eng.file_read(space, &path) {
                Ok(reply) => {
                    // Opening a file is the moment a language server becomes worth having, and
                    // the last moment before you would notice it was not there.
                    if let Some(body) = reply.body.as_deref() {
                        eng.lsp_sync(space, &path, body);
                    }
                    if let Some(c) = eng.clients.get(&id) {
                        let _ = c.out.send(ServerFrame::Files(Box::new(reply)));
                    }
                }
                Err(e) => eng.notice(NoticeLevel::Warn, format!("could not open {path}: {e}")),
            }
        }
        // What the editor's buffer says right now, which is not what is on disk and is not
        // meant to be. The point of this arriving separately from a save is that a language
        // server should be objecting to the line you are on, not to the last thing you wrote
        // out.
        ClientFrame::Command(Cmd::DocChanged { space, path, body, vault }) => {
            eng.doc_changed(space, &path, &body, vault);
            // Whatever is already known about it, straight back to the client that asked. A
            // server answers a document it has already seen with silence, so a file opened a
            // second time would otherwise look clean until the next edit.
            let known = eng
                .doc_path(space, &path, vault)
                .and_then(|p| eng.lsp.diags_for(&p).cloned());
            if let (Some(diags), Some(c)) = (known, eng.clients.get(&id)) {
                let _ = c.out.send(ServerFrame::Diagnostics { path, diags });
            }
        }
        ClientFrame::Command(Cmd::DocClosed { space, path, vault }) => {
            eng.doc_closed(space, &path, vault);
        }
        ClientFrame::Command(Cmd::Attach { space, name, bytes }) => {
            match eng.vault_attach(space, &name, &bytes) {
                Ok(path) => log_line(&format!("attached {}", path.display())),
                Err(e) => eng.notice(NoticeLevel::Warn, format!("could not attach {name}: {e}")),
            }
        }
        ClientFrame::Command(Cmd::Complete { space, path, body, line, col, vault }) => {
            // The buffer goes with the request rather than waiting for the debounce: an
            // answer about text the server has not been shown yet is an answer about a
            // different file.
            eng.doc_changed(space, &path, &body, vault);
            let Some(full) = eng.doc_path(space, &path, vault) else { return };
            let Some(lang) = lsp::language_for(&eng.cfg, &full) else { return };
            let Some(root) = eng.session.space(space).map(|s| s.cwd.clone()) else { return };
            eng.lsp_asked = Some(id);
            eng.lsp.complete(&root, &lang, &full, line, col);
        }
        ClientFrame::Command(Cmd::FileSave { space, path, body }) => {
            match eng.file_write(space, &path, &body) {
                Ok(()) => eng.lsp_sync(space, &path, &body),
                Err(e) => eng.notice(NoticeLevel::Error, format!("could not save {path}: {e}")),
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
                // A file pane closing is a document closing, and a server that is never told
                // goes on analysing something nobody is looking at.
                let doc = eng.session.panes.get(&p).and_then(|p| p.doc_path().map(|d| d.to_path_buf()));
                if let Some(path) = doc {
                    if let Some(lang) = lsp::language_for(&cfg, &path) {
                        // The pane being closed is the focused one, so its project is the
                        // focused project.
                        let root = eng
                            .session
                            .focused_space
                            .and_then(|s| eng.session.space(s))
                            .map(|s| s.cwd.clone());
                        if let Some(root) = root {
                            eng.lsp.did_close(&root, &lang, &path);
                        }
                    }
                }
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
            eng.session.relayout(&cfg);
        }
        Cmd::PrevTab => {
            eng.session.cycle_tab(-1);
            eng.session.relayout(&cfg);
        }
        Cmd::GotoTab(i) => {
            eng.session.goto_tab(i);
            eng.session.relayout(&cfg);
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
            eng.session.relayout(&cfg);
        }
        Cmd::OpenDocPane { space, path } => {
            // Relative to the project, and jailed to it: this opens a pane on a path that
            // arrived over a socket, so it gets the same treatment as writing one.
            let root = eng.session.space(space).map(|s| s.cwd.clone());
            let full = root.and_then(|r| vault::safe_join(&r, &path).ok()).or_else(|| {
                // A note lives in the vault rather than the project, so try there too.
                eng.vault_root(space).and_then(|r| vault::safe_join(&r, &path).ok())
            });
            match full.filter(|p| p.is_file()) {
                Some(p) => {
                    if let Err(e) = eng.session.split_doc(&cfg, None, Dir::Right, &p) {
                        problems.push((NoticeLevel::Warn, e.to_string()));
                    } else if let Some(lang) = lsp::language_for(&cfg, &p) {
                        if let Ok(text) = std::fs::read_to_string(&p) {
                            let root = eng.session.space(space).map(|s| s.cwd.clone());
                            if let Some(root) = root {
                                eng.lsp.did_open(&cfg, &root, &lang, &p, &text);
                            }
                        }
                    }
                }
                None => problems.push((NoticeLevel::Warn, format!("no file at {path}"))),
            }
        }
        Cmd::OpenProject { cwd } => {
            let path = PathBuf::from(&cwd);
            // A remembered directory can be gone by the time you pick it. Say so instead of
            // opening a space whose cwd silently became wherever the daemon was started.
            if !path.is_dir() {
                problems.push((NoticeLevel::Warn, format!("{cwd} is no longer a directory")));
            } else if let Some(existing) =
                eng.session.spaces.iter().find(|s| s.cwd == path).map(|s| s.id)
            {
                // Already open: go there rather than starting a second copy of the project.
                eng.session.focus_space(existing);
            } else {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.clone());
                if let Err(e) = eng.session.create_space(&cfg, Some(&name), &path) {
                    problems.push((NoticeLevel::Error, e.to_string()));
                }
            }
        }
        Cmd::NextSpace => {
            eng.session.cycle_space(1);
            eng.session.relayout(&cfg);
        }
        Cmd::PrevSpace => {
            eng.session.cycle_space(-1);
            eng.session.relayout(&cfg);
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
            // Which space is focused decides whether a tab bar takes a row, and zoom only
            // applies to the tab on screen — so what changed here is geometry, not just focus.
            eng.session.relayout(&cfg);
            seen = eng.session.focused_pane();
        }
        Cmd::NewTabIn(space) => {
            if let Err(e) = eng.session.create_tab(&cfg, space, None) {
                problems.push((NoticeLevel::Error, e.to_string()));
            }
        }
        // Answered in `handle_client_frame`, which knows which client asked. Reaching here
        // means it came from the control API, where `digest` is the method to use.
        // Both handled per-client in `handle_client_frame`, where the asking client is
        // known. Reaching them here means someone routed a question through a broadcast.
        Cmd::RequestDigest
        | Cmd::VaultQuery { .. }
        | Cmd::VaultSave { .. }
        | Cmd::VaultInit { .. }
        | Cmd::FileQuery { .. }
        | Cmd::FileRead { .. }
        | Cmd::FileSave { .. }
        | Cmd::DocChanged { .. }
        | Cmd::DocClosed { .. }
        | Cmd::Complete { .. }
        | Cmd::Attach { .. }
        // Every kanban command answers with the board, so all of them need to know which
        // client asked. Reaching here means one was routed through the broadcast path.
        | Cmd::KanbanQuery { .. }
        | Cmd::CardNew { .. }
        | Cmd::CardEdit { .. }
        | Cmd::CardMove { .. }
        | Cmd::CardComment { .. }
        | Cmd::CardArchive { .. }
        | Cmd::CardHandOff { .. }
        | Cmd::ColumnRename { .. } => {}
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

    // Whatever project you are looking at is the most recent one. Done here rather than in
    // the handful of commands that change spaces, because spaces are also created over the
    // socket and by restore, and one funnel cannot forget a path the way five can.
    eng.note_focused_project();

    // Git state, on detection's cadence but with its own much longer staleness window inside
    // the cache. Piggybacking on `detect_due` rather than taking a timer of its own keeps the
    // fork-per-directory work on one clock.
    // Every tick, because a diagnostic is worth showing the moment it lands and draining a
    // channel that is almost always empty costs nothing.
    drain_lsp(eng);

    if detect_due {
        refresh_repos(eng);
        refresh_vaults(eng);
        // The half of a language server's lifecycle that is easy to leave for later: stop the
        // ones nobody is using and the ones whose project has closed, and give the ones that
        // died their next attempt.
        let roots: Vec<PathBuf> = eng.session.spaces.iter().map(|s| s.cwd.clone()).collect();
        eng.lsp.sweep(|root| roots.iter().any(|cwd| cwd.starts_with(root)));
        let cfg = eng.cfg.clone();
        eng.lsp.retry(&cfg);
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
        // And hand over any card whose armed window has arrived. The same clock, because it
        // is the same question — is there something an agent should be told about.
        eng.hand_over_due_cards();
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

    // Only on a detection pass: the state and the screen it reads were both refreshed by one,
    // and answering off a stale snapshot is answering a question that may already be gone.
    if detect_due {
        let answered = approvals::consider(eng);
        if !answered.is_empty() {
            eng.pending_events.extend(answered);
            eng.dirty_shape = true;
        }
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

/// One agent the nudge could wake, and everything the choice turns on.
///
/// Named rather than a tuple: the role made it a fourth field, and `(pane, name, role, since)`
/// sorted on `.3` is a line that stays correct only until somebody inserts a field.
struct Candidate {
    pane: PaneId,
    /// How the bus addresses it.
    name: String,
    /// What it is labelled as, which decides what it may be offered.
    role: Option<String>,
    /// When it entered its current state. Longest-idle is most available.
    since: std::time::Instant,
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

    // Filed before the successor is told anything, so the note it is pointed at exists by
    // the time it goes looking.
    let filed = file_handoff(eng, dead, &name, &successor);
    let brief = match &filed {
        Some(path) => format!("{brief}\n\nThis handover is written up at {}.", path.display()),
        None => brief,
    };

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

/// Put a handover in the vault, where it outlives the pane it came from.
///
/// The agent writes its own notes into `.horde/`, which is derived state horde is free to
/// delete — fine for a brief read minutes later, wrong for the record of how a piece of work
/// changed hands. So the note is copied into the vault, attributed to the agent that wrote it
/// and linked to both ends of the succession, which is what makes it findable later by the
/// only route anyone will actually use: the graph, or a search for the agent's name.
///
/// Nothing here is load-bearing for succession itself. A vault that cannot be written to is a
/// reason to log and carry on, not a reason to leave an exhausted agent unsucceeded.
fn file_handoff(eng: &mut Engine, pane: PaneId, name: &str, successor: &str) -> Option<PathBuf> {
    let cwd = eng.session.panes.get(&pane)?.cwd.clone();
    let space = eng.session.focused_space?;
    let left = cwd.join(format!(".horde/handoff-{name}.md"));
    let body = std::fs::read_to_string(&left).ok()?;
    if body.trim().is_empty() {
        return None;
    }
    let project = eng.session.space(space).map(|s| s.name.clone()).unwrap_or_default();
    let note = format!(
        "# Handoff — {name}\n\nProject: [[{project}]]\nTaken over by: [[{successor}]]\n\n{}",
        body.trim()
    );
    let day = triggers::local_date(now_millis());
    match eng.vault_put(space, &format!("Handoff — {name} {day}.md"), &note, Some(name), false) {
        Ok(path) => Some(path),
        Err(e) => {
            log_line(&format!("could not file {name}'s handover in the vault: {e}"));
            None
        }
    }
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
        if let Err(e) = bus.send(session, &cfg, bus::Outgoing::plain(None, &name, &body)) {
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
        if let Err(e) = bus.send(session, &cfg, bus::Outgoing::plain(None, &name, &command)) {
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

/// How many notes one pass may parse, across every vault.
///
/// This shares a tick with pane pumping, so a cold vault of five thousand notes has to
/// arrive over several seconds rather than stalling the terminal to answer a question
/// nobody has asked yet. Small enough to be invisible, large enough that a normal vault is
/// fully indexed within a pass or two.
const VAULT_BUDGET: usize = 40;

/// Fold what the language servers have said into the session.
///
/// A server that started, died, or changed its mind about a file is something the person at
/// the keyboard should be able to find out about without reading a log — especially the
/// death, since the alternative symptom is diagnostics that simply never appear.
fn drain_lsp(eng: &mut Engine) {
    let events = eng.lsp.drain();
    if events.is_empty() {
        return;
    }
    let mut said = Vec::new();
    let mut changed = false;
    for ev in events {
        match ev {
            lsp::Event::Ready((root, lang)) => {
                let _ = root;
                said.push((NoticeLevel::Info, format!("{lang} language server ready")));
            }
            lsp::Event::Exited { key, why } => {
                let (_, lang) = &key;
                // Which of the two it is has already been decided by the registry, and the
                // difference matters: one is "wait a moment", the other is "fix your config".
                let text = match eng.lsp.get(&key).map(|s| s.state.clone()) {
                    Some(lsp::State::Failed(_)) => {
                        format!("{lang} language server keeps dying, giving up: {why}")
                    }
                    _ => format!("{lang} language server died, restarting: {why}"),
                };
                said.push((NoticeLevel::Warn, text));
            }
            // Straight back to whoever asked, and only to them: an unsolicited completion
            // popup in somebody else's editor would be a haunting.
            lsp::Event::Completions { path, items } => {
                let named = eng.lsp_paths.get(&path).cloned();
                if let (Some(rel), Some(c)) =
                    (named, eng.lsp_asked.take().and_then(|id| eng.clients.get(&id)))
                {
                    let _ = c.out.send(ServerFrame::Completions { path: rel, items });
                }
            }
            lsp::Event::Reply { .. } => {}
            lsp::Event::Diagnostics { path, diags, .. } => {
                changed = true;
                // Only for files an editor actually has open, and named the way that editor
                // named them. A server publishes diagnostics for whatever it feels like
                // looking at — headers, generated code, the whole dependency tree — and none
                // of that is on anybody's screen.
                if let Some(rel) = eng.lsp_paths.get(&path).cloned() {
                    for c in eng.clients.values() {
                        let _ = c
                            .out
                            .send(ServerFrame::Diagnostics { path: rel.clone(), diags: diags.clone() });
                    }
                }
            }
        }
    }
    for (level, text) in said {
        eng.notice(level, text);
    }
    // Diagnostic counts ride the snapshot, so a fresh set is a reason to send one.
    if changed {
        eng.dirty_shape = true;
    }
}

/// Reindex each project's notes, and forget vaults no space is looking at any more.
pub(super) fn refresh_vaults(eng: &mut Engine) {
    if !eng.cfg.vault {
        eng.vaults.clear();
        return;
    }
    let mut roots: Vec<PathBuf> = eng
        .session
        .spaces
        .iter()
        .filter_map(|s| vault::locate(&s.cwd, &eng.cfg.vault_dir))
        .collect();
    // The home vault is indexed whether or not any project points at it, because notes are
    // not a feature of whichever directory happens to be open.
    if eng.cfg.vault_home.is_dir() {
        roots.push(eng.cfg.vault_home.clone());
    }
    roots.sort();
    roots.dedup();

    let mut changed = false;
    for root in &roots {
        let idx = eng.vaults.entry(root.clone()).or_insert_with(|| vault::Index::new(root.clone()));
        changed |= idx.refresh(VAULT_BUDGET);
    }
    eng.vaults.retain(|k, _| roots.iter().any(|r| r == k));
    if changed {
        // The note count rides the snapshot, so a changed index is a changed shape.
        eng.touch();
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

    let snapshot = if eng.dirty_shape {
        eng.dirty_shape = false;
        Some(Box::new(eng.snapshot()))
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
                //
                // Name the socket when it is not the default one. A daemon on its own
                // socket is one somebody started deliberately — a sandbox, a second session,
                // a build under test — and plain `horde stop` would walk past it and stop
                // whichever daemon the environment points at instead, which is at best
                // confusing and at worst somebody else's running work.
                let fix = if std::env::var_os("HORDE_SOCKET").is_some()
                    || std::env::var_os("HORDE_CONFIG_DIR").is_some()
                {
                    format!(
                        "Stop this one with `HORDE_SOCKET={} horde stop`, then start it again.",
                        crate::config::socket_path().display()
                    )
                } else {
                    "Run `horde stop`, then `horde`.".to_string()
                };
                let bye = ServerFrame::Bye {
                    reason: format!(
                        "protocol mismatch: client speaks v{protocol}, daemon speaks \
                         v{PROTOCOL_VERSION}. {fix}"
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

/// This machine's short name, lowercased, with a trailing `.local` taken off.
///
/// mDNS hands out `Joshs-MacBook-Pro.local`, and the suffix is a protocol detail rather than
/// part of what anyone calls the machine. Falls back to `$HOSTNAME` because a login shell has
/// usually set it even where the syscall is unhelpful.
fn hostname() -> Option<String> {
    let mut buf = [0i8; 256];
    // SAFETY: `gethostname` writes at most `len` bytes into the buffer we hand it, and the
    // result is only read up to the first NUL.
    let raw = unsafe {
        if libc::gethostname(buf.as_mut_ptr(), buf.len()) != 0 {
            None
        } else {
            let bytes: Vec<u8> =
                buf.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
            String::from_utf8(bytes).ok()
        }
    };
    let name = raw.or_else(|| std::env::var("HOSTNAME").ok())?;
    let name = name.trim().trim_end_matches(".local").to_lowercase();
    (!name.is_empty()).then_some(name)
}

/// Put a card's work on the agents' board, and record on the card that it went.
///
/// The one seam between the two boards, and deliberately the only one. It runs in this
/// direction only: a card becomes a task, and the task's result comes back as a comment. The
/// agents never see a card, never move one between columns, and never learn that columns
/// exist — which is what stops the personal board's rules from having to make sense to them.
///
/// A free function rather than a method because it writes to two stores that are otherwise
/// strangers, and a method on either one would make it look like that store's business.
///
/// The order matters: the task goes on the board first, and only a task that really landed
/// gets marked on the card. The other order would leave a card claiming it had been handed
/// over to a task that does not exist, which is worse than doing it twice.
fn hand_over(eng: &mut Engine, card: u64) -> Result<u64> {
    if !eng.cfg.board {
        return Err(anyhow!(
            "the agents' board is off — set agents.board = true in config.toml to hand a card over"
        ));
    }
    let c = eng.kanban.get(card).ok_or_else(|| anyhow!("no card #{card}"))?.clone();
    if let Some(task) = c.handed {
        return Err(anyhow!("card #{card} is already task #{task}"));
    }
    let project = c
        .project
        .clone()
        .ok_or_else(|| anyhow!("card #{card} names no project, so no agent could be told where to work"))?;
    // Title and description together: the title alone is a label written for a column two
    // dozen cells wide, and an agent handed only that has to guess at everything the card
    // actually says.
    let text = match c.body.trim() {
        "" => c.title.clone(),
        body => format!("{}\n\n{body}", c.title),
    };
    // A card carries no role of its own — it is written in a column on your board, not addressed
    // to anybody. So it inherits the one role the config already names: where `task_authors` is
    // set, whoever may write work is also who receives it, and an armed card lands on their plate
    // to be read and broken up rather than in a pool where the nearest idle agent starts editing
    // from a title and a due date. With no such role configured it is general work, which is what
    // every card has always been.
    let lead = eng.cfg.task_authors.first().cloned();
    let task = eng.board.add(tasks::NewTask {
        role: lead.as_deref(),
        ..tasks::NewTask::new(&text, "kanban", Some(&project))
    })?;
    eng.kanban.mark_handed(card, task.id)?;
    eng.touch();
    Ok(task.id)
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

    /// The home vault is a promise that horde makes notes possible anywhere. It has to make
    /// the directory too, or the promise only holds for people who already made it by hand —
    /// which is every new user, told they have no vault the first time they write one.
    #[test]
    fn writing_the_first_note_makes_the_vault_that_holds_it() {
        let mut eng = engine();
        let home = std::env::temp_dir().join(format!("horde-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        eng.cfg.vault_home = home.clone();
        assert!(!home.exists(), "nothing is created before it is asked for");

        let space = eng.session.focused_space.expect("a space");
        let written = eng.vault_write(space, "First.md", "# First\n").expect("the write works");

        assert!(written.starts_with(&home), "it landed in the home vault");
        assert!(vault::is_vault(&home), "which is now a vault horde will find again");
        assert!(eng.vault_for(space).is_some_and(|v| v.len() >= 1), "and it is indexed");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// An agent's notes go somewhere of their own, always. Mixed in with your own writing
    /// there is no undoing it — nothing recorded which was which.
    #[test]
    fn a_note_written_by_an_agent_is_kept_apart_and_signed() {
        let mut eng = engine();
        let home = std::env::temp_dir().join(format!("horde-agentnote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        eng.cfg.vault_home = home.clone();
        let space = eng.session.focused_space.expect("a space");

        let written = eng
            .vault_put(space, "Findings.md", "the thing I found", Some("reviewer"), false)
            .expect("written");
        assert!(
            written.parent().is_some_and(|d| d.ends_with(vault::AGENT_DIR)),
            "in its own folder: {}",
            written.display()
        );
        let text = std::fs::read_to_string(&written).unwrap();
        assert!(text.contains("by: reviewer"), "and signed: {text}");

        // A path with a folder in it is the caller being specific, and is honoured.
        let elsewhere = eng
            .vault_put(space, "reviews/Deep.md", "body", Some("reviewer"), false)
            .expect("written");
        assert!(elsewhere.parent().is_some_and(|d| d.ends_with("reviews")));

        // A person's own note is neither moved nor stamped.
        let mine = eng.vault_write(space, "Mine.md", "just mine").expect("written");
        assert_eq!(mine.parent(), Some(home.as_path()));
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "just mine");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Appending is what makes a dated note a log rather than the last thing that happened to
    /// be written to it — and it must not re-stamp, or the file fills with frontmatter.
    #[test]
    fn appending_adds_to_a_note_without_stamping_it_again() {
        let mut eng = engine();
        let home = std::env::temp_dir().join(format!("horde-append-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        eng.cfg.vault_home = home.clone();
        let space = eng.session.focused_space.expect("a space");

        eng.vault_put(space, "Log.md", "first", Some("builder"), false).unwrap();
        let p = eng.vault_put(space, "Log.md", "second", Some("builder"), true).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("source: horde").count(), 1, "stamped once: {text}");
        assert!(text.contains("first") && text.contains("second"), "{text}");
        assert!(text.find("first") < text.find("second"), "in order");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The thing that writes a megabyte of note is an agent that has gone wrong, and the
    /// first symptom should be a refusal with a reason rather than a vault nobody can open.
    #[test]
    fn a_note_too_large_to_be_meant_is_refused_with_a_reason() {
        let mut eng = engine();
        let home = std::env::temp_dir().join(format!("horde-huge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        eng.cfg.vault_home = home.clone();
        let space = eng.session.focused_space.expect("a space");

        let huge = "x".repeat(vault::MAX_NOTE + 1);
        let err = eng.vault_put(space, "Huge.md", &huge, Some("runaway"), false).unwrap_err();
        assert!(err.to_string().contains("loop"), "it says what it thinks happened: {err}");
        assert!(!home.join(vault::AGENT_DIR).join("Huge.md").exists(), "and wrote nothing");

        // And it cannot be got past by appending a little at a time.
        eng.vault_put(space, "Slow.md", "start", Some("runaway"), false).unwrap();
        let big = "y".repeat(vault::MAX_NOTE);
        assert!(eng.vault_put(space, "Slow.md", &big, Some("runaway"), true).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }


    /// A note written by horde has to be in horde's index immediately. Waiting for the next
    /// scan would mean the daemon not knowing about a file it just wrote itself.
    #[test]
    fn a_note_written_is_indexed_at_once() {
        let mut eng = engine();
        let home = std::env::temp_dir().join(format!("horde-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        vault::init(&home).unwrap();
        eng.cfg.vault_home = home.clone();
        refresh_vaults(&mut eng);

        let space = eng.session.focused_space.expect("a space");
        eng.vault_write(space, "Ideas/One.md", "# One\n\nsee [[Welcome]]\n").unwrap();

        let idx = eng.vault_for(space).expect("the home vault answers");
        let welcome = idx.resolve("Welcome").expect("the starter note");
        assert_eq!(idx.backlinks(welcome).len(), 1, "the new note's link is already known");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The knowledge layer does not depend on which directory happens to be open. A project
    /// with no vault of its own still has somewhere to put a thought.
    #[test]
    fn a_project_without_a_vault_falls_back_to_the_home_one() {
        let mut eng = engine();
        let home = std::env::temp_dir().join(format!("horde-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        vault::init(&home).unwrap();
        eng.cfg.vault_home = home.clone();
        refresh_vaults(&mut eng);

        let space = eng.session.focused_space.expect("a space");
        assert_eq!(eng.vault_root(space).as_ref(), Some(&home), "the fallback is the home vault");
        assert!(eng.vault_for(space).is_some(), "and it is indexed");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The recents list is keyed on the directory, not the name. A renamed space is the
    /// same project, and remembering it twice would offer you a choice between two rows
    /// that open the same thing.
    #[test]
    fn reopening_a_project_moves_it_up_rather_than_listing_it_twice() {
        let mut eng = engine();
        eng.remember_project("alpha", std::path::Path::new("/tmp/alpha"));
        eng.remember_project("beta", std::path::Path::new("/tmp/beta"));
        eng.remember_project("renamed-alpha", std::path::Path::new("/tmp/alpha"));

        let cwds: Vec<&str> = eng.recents.iter().map(|r| r.cwd.as_str()).collect();
        assert_eq!(cwds, vec!["/tmp/alpha", "/tmp/beta"], "one entry each, newest first");
        assert_eq!(eng.recents[0].name, "renamed-alpha", "and it carries the current name");
    }

    /// A list you pick from by eye stops being useful long before it stops being possible.
    #[test]
    fn the_recents_list_is_capped() {
        let mut eng = engine();
        for i in 0..(Engine::MAX_RECENTS + 5) {
            eng.remember_project(&format!("p{i}"), &std::path::PathBuf::from(format!("/tmp/p{i}")));
        }
        assert_eq!(eng.recents.len(), Engine::MAX_RECENTS);
        assert_eq!(eng.recents[0].name, format!("p{}", Engine::MAX_RECENTS + 4), "newest first");
    }

    /// Opening a project you already have on screen goes *there*. Starting a second space on
    /// the same directory would split one project's agents across two rows of the sidebar.
    #[test]
    fn opening_a_project_that_is_already_open_focuses_it_instead_of_duplicating_it() {
        let mut eng = engine();
        // A directory of its own: the test engine's first space already sits on the temp
        // root, and matching that one would prove nothing about the one we opened.
        let dir = std::env::temp_dir().join(format!("horde-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = eng.cfg.clone();
        let first = eng.session.create_space(&cfg, Some("already-here"), &dir).unwrap();
        let before = eng.session.spaces.len();

        apply_cmd(&mut eng, Cmd::OpenProject { cwd: dir.to_string_lossy().to_string() });

        assert_eq!(eng.session.spaces.len(), before, "no second space");
        assert_eq!(eng.session.focused_space, Some(first), "and it went there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A remembered directory can be deleted between sessions. Say so, rather than opening a
    /// space whose cwd silently became wherever the daemon happened to start.
    #[test]
    fn opening_a_project_that_no_longer_exists_warns_instead_of_guessing() {
        let mut eng = engine();
        let before = eng.session.spaces.len();
        apply_cmd(&mut eng, Cmd::OpenProject { cwd: "/tmp/horde-definitely-not-here".into() });
        assert_eq!(eng.session.spaces.len(), before, "nothing opened");
        assert!(
            eng.pending_events.iter().any(|e| matches!(e, Event::Notice { .. })),
            "and it said why"
        );
    }

    pub(super) fn engine() -> Engine {
        engine_with_shell(None)
    }

    /// An engine whose one pane prints `words` and then stays open.
    ///
    /// The way to give a pane a line for horde to detect. `echo` looks like the obvious choice
    /// and is a race: it exits immediately, and macOS discards a tty's pending output when the
    /// last descriptor on the slave side closes, so under suite-level load the reader thread can
    /// find a closed pty with nothing in it. The test then reports that the feature did not fire,
    /// when the truth is the line never arrived to fire it — a failure that blames the code for
    /// the test's own timing. See `tests/support/say.py`.
    pub(super) fn engine_saying(words: &str) -> Engine {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/say.py");
        engine_with_shell(Some(&format!("python3 {script} {words}")))
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
        // Point the home vault somewhere that does not exist, so tests never read the notes
        // of whoever is running them. Without this the suite passes or fails depending on
        // whether the developer happens to keep a `~/notes` — which is exactly the kind of
        // difference between a laptop and CI that costs an afternoon to find.
        cfg.vault_home = std::env::temp_dir()
            .join(format!("horde-test-novault-{}", std::process::id()));
        let session = Session::new(&cfg);
        let agents = agents::Detector::new(&cfg);
        let mut eng = Engine {
            session,
            bus: bus::Bus::new(test_path("bus.jsonl")),
            board: tasks::Board::new(test_path("tasks.jsonl")),
            kanban: kanban::Kanban::new(test_path("kanban.jsonl")),
            triggers: triggers::Store::new(
                test_path("triggers.jsonl"),
            ),
            approvals: approvals::Seen::default(),
            journal: journal::Journal::new(test_path("journal.jsonl")),
            pane_names: HashMap::new(),
            started: now_millis(),
            last_seen: 0,
            last_alert: 0,
            agents,
            recents: Vec::new(),
            repos: repo::Cache::default(),
            vaults: HashMap::new(),
            lsp: lsp::Registry::new(),
            lsp_paths: HashMap::new(),
            lsp_asked: None,
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

    /// A theme change has to reach what is already on screen.
    ///
    /// Colours are resolved into the mirror when a row is built, so every row a pane is already
    /// holding is painted in the old palette. Nothing about the emulator grid changed, so no row
    /// is rebuilt, nothing is marked dirty, and the client is told nothing — the chrome recolours
    /// around a terminal that keeps the previous theme until whatever is running happens to
    /// redraw. Reported as "the terminal only picks up the new theme after I `/clear`".
    #[test]
    fn switching_theme_recolours_what_is_already_on_screen() {
        let mut eng = engine_with_shell(Some("cat"));
        let pane = *eng.session.panes.keys().next().unwrap();
        assert!(type_into(&mut eng, pane, b"a", 1), "the pty never echoed the keystroke");
        let before = eng.session.panes[&pane].row(0).cloned().expect("row 0 exists");

        // Swap to a palette that resolves the default foreground differently.
        let other = crate::theme::Theme::by_name("gruvbox").expect("a second palette to switch to");
        assert_ne!(other.fg, eng.cfg.theme.fg, "the two themes have to actually differ");
        eng.cfg.theme = other.clone();

        // Asking for the repaint is what carries the new palette onto rows the program has
        // already drawn and has no reason to draw again.
        eng.session.panes.get_mut(&pane).unwrap().request_full_repaint();
        eng.session.panes.get_mut(&pane).unwrap().pump(&other);
        let after = eng.session.panes[&pane].row(0).cloned().expect("row 0 exists");
        assert_eq!(after.runs[0].text, before.runs[0].text, "the text should be untouched");
        assert_ne!(after.runs[0].fg, before.runs[0].fg, "row 0 is still in the old palette");
        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

    /// Two windows share one set of ttys, so the size has to be the smallest of them — and the
    /// moment the small one closes is the moment the large one is allowed to grow back. Nothing
    /// else tells the daemon: a client only reports a size when its own size changes, and the
    /// window that stayed open never changed. Left uncorrected, the large window draws full-size
    /// pane rects around ttys still shaped for the window that went away, which on screen is an
    /// agent shrunk into the top-left corner of its pane.
    #[test]
    fn closing_the_smaller_window_hands_the_larger_one_its_size_back() {
        let mut eng = engine_with_shell(Some("cat"));
        let pane = *eng.session.panes.keys().next().unwrap();

        let (big, _big_rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: big, needs_full: Vec::new(), cols: 200, rows: 60 });
        resync_client_size(&mut eng);
        let (wide, tall) = (eng.session.panes[&pane].cols, eng.session.panes[&pane].rows);

        // A second, smaller window opens. It wins while it is there: anything wider than it is
        // content it can only see part of.
        let (small, _small_rx) = mpsc::unbounded_channel();
        eng.clients.insert(2, Client { out: small, needs_full: Vec::new(), cols: 80, rows: 24 });
        resync_client_size(&mut eng);
        assert!(
            eng.session.panes[&pane].cols < wide,
            "the smaller window has to win while it is open"
        );

        // And it gives the size back on the way out.
        eng.clients.remove(&2);
        resync_client_size(&mut eng);
        assert_eq!(
            (eng.session.panes[&pane].cols, eng.session.panes[&pane].rows),
            (wide, tall),
            "the pty is still shaped for the window that closed"
        );
        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

    /// The whole feature, end to end through the engine: a blocked agent showing a prompt a
    /// rule permits gets the key the agent itself highlighted, and only on the second pass.
    ///
    /// Driven through `approvals::consider` with a real pane and a real screen rather than
    /// against the matcher, because the parts that can be wrong here are the ones between the
    /// two — reading the pane, requiring it to hold still, and writing to the tty.
    #[test]
    fn an_armed_rule_presses_what_the_agent_recommended_once_the_prompt_holds_still() {
        // `cat` echoes what is written, so the pty's own echo is the proof the key was sent.
        let mut eng = engine_with_shell(Some("cat"));
        // Its own journal. The hourly ceiling is counted off `Fired` entries, and the shared
        // test journal already holds the trigger suite's — which would spend this budget before
        // the test starts and make it pass alone but fail in the suite.
        let jp = test_path("approvals-journal.jsonl");
        let _ = std::fs::remove_file(&jp);
        eng.journal = journal::Journal::new(jp);
        eng.cfg.unattended = true;
        eng.cfg.approvals = vec![crate::config::Approval {
            space: None,
            role: None,
            matches: "do you want to make this edit".into(),
            allow: vec!["yes".into()],
        }];
        let pane = *eng.session.panes.keys().next().unwrap();

        // A blocked agent, showing a menu with the first option highlighted.
        let screen = "Do you want to make this edit to src/mux.rs?\n\
                      ❯ 1. Yes\n\
                      2. Yes, and don't ask again\n\
                      3. No, and tell Claude what to do differently\n";
        // Waiting on the text rather than on the cursor: this screen ends in a newline, so
        // `type_into`'s column-zero condition is already true before the pty has echoed a byte,
        // and under a loaded test run it returns before there is anything to read.
        eng.session.panes.get_mut(&pane).unwrap().write_input(screen.as_bytes()).unwrap();
        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut showing = false;
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&pane).unwrap().pump(&theme);
            let seen = eng.session.panes[&pane].detection_snapshot(40).join("\n");
            if seen.contains("3. No, and tell Claude") {
                showing = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(showing, "the pty never echoed the prompt");
        let p = eng.session.panes.get_mut(&pane).unwrap();
        p.agent = Some(state::AgentRuntime {
            kind: "claude".into(),
            name: "builder".into(),
            class: Default::default(),
            state: crate::proto::AgentState::Blocked,
            since: std::time::Instant::now(),
            authority: "screen".into(),
            reason: "test".into(),
            seen: true,
            session_id: None,
            queued: Vec::new(),
            question: None,
            activity: Default::default(),
            touched: Default::default(),
            nudged_since: None,
            alerted_since: None,
        });

        // First pass only remembers: a screen still being drawn is how the wrong menu gets read.
        assert!(approvals::consider(&mut eng).is_empty(), "nothing on the first sighting");
        // Second pass, same prompt: now it answers.
        let events = approvals::consider(&mut eng);
        assert_eq!(events.len(), 1, "the steady prompt should have been answered");

        // The key pressed was the recommended one, not merely the first that was allowed.
        assert!(
            eng.journal
                .since(0)
                .any(|e| e.kind == journal::Kind::Fired && e.subject.contains("answered builder: 1")),
            "the answer is journalled as something horde decided"
        );
        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

    /// Disarmed, the same setup answers nothing — and does not bank the sighting either, so
    /// arming horde does not immediately answer a prompt that has been sitting there.
    #[test]
    fn nothing_is_answered_while_horde_is_not_armed() {
        let mut eng = engine_with_shell(Some("cat"));
        eng.cfg.unattended = false;
        eng.cfg.approvals = vec![crate::config::Approval {
            space: None,
            role: None,
            matches: "proceed".into(),
            allow: vec!["yes".into()],
        }];
        assert!(approvals::consider(&mut eng).is_empty());
        assert!(approvals::consider(&mut eng).is_empty(), "and still nothing on a second pass");
        for p in eng.session.panes.values_mut() {
            p.kill();
        }
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
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });
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

    // -- the kanban over the wire ------------------------------------------
    //
    // The store has its own tests and so does the view. These are the bit in between: that a
    // command arriving from a client reaches the store and that the board comes back — to the
    // client that asked, and to nobody else.

    /// An engine whose two boards are its own.
    ///
    /// `test_path` is unique per *process*, and tests run in parallel threads inside one — so
    /// two tests that each replay `kanban.jsonl` would be reading each other's cards. Both
    /// logs get a name of their own here, which is the same fix commit `bf2287a` applied to
    /// the task board's own tests and for the same reason.
    fn engine_with_boards(tag: &str) -> Engine {
        let mut eng = engine();
        let path = |what: &str| {
            std::env::temp_dir()
                .join(format!("horde-kb-{tag}-{}-{what}.jsonl", std::process::id()))
        };
        for what in ["kanban", "tasks"] {
            let _ = std::fs::remove_file(path(what));
        }
        eng.kanban = kanban::Kanban::new(path("kanban"));
        eng.board = tasks::Board::new(path("tasks"));
        eng
    }

    /// Every reply the daemon sent this client, drained.
    fn kanban_replies(
        rx: &mut mpsc::UnboundedReceiver<ServerFrame>,
    ) -> Vec<crate::proto::KanbanReply> {
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ServerFrame::Kanban(r) = f {
                out.push(*r);
            }
        }
        out
    }

    #[test]
    fn a_client_asking_for_the_board_gets_it_back() {
        let mut eng = engine_with_boards("board-query");
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });

        handle_client_frame(&mut eng, 1, ClientFrame::Command(Cmd::KanbanQuery { space: None }));
        let reply = kanban_replies(&mut rx).pop().expect("the board came back");
        assert!(reply.cards.is_empty());
        // The columns travel with it rather than being read from the client's own config, so
        // a client attached from another machine still agrees about what the columns are.
        assert_eq!(reply.columns, eng.cfg.kanban_columns);
        kill_all(&mut eng);
    }

    /// Every command that changes a card answers with the whole board, so the client never
    /// has to patch its own copy and cannot end up showing a state that was never true.
    #[test]
    fn every_card_command_answers_with_the_whole_board() {
        let mut eng = engine_with_boards("card-cmds");
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });
        let send = |eng: &mut Engine, cmd: Cmd| handle_client_frame(eng, 1, ClientFrame::Command(cmd));

        send(&mut eng, Cmd::CardNew { space: None, column: "Todo".into(), title: "write it".into() });
        let after_new = kanban_replies(&mut rx).pop().expect("a reply");
        assert_eq!(after_new.cards.len(), 1);
        let id = after_new.cards[0].id;

        send(
            &mut eng,
            Cmd::CardEdit {
                id,
                patch: crate::proto::CardPatch {
                    body: Some("the long version".into()),
                    ..Default::default()
                },
            },
        );
        assert_eq!(kanban_replies(&mut rx).pop().unwrap().cards[0].body, "the long version");

        send(&mut eng, Cmd::CardComment { id, body: "parked".into() });
        let commented = kanban_replies(&mut rx).pop().unwrap();
        let last = commented.cards[0].comments.last().expect("the comment landed");
        assert_eq!(last.body, "parked");
        // The daemon stamps who said it. The client only ever sends the words, which is what
        // stops a client claiming to be somebody else.
        assert_eq!(last.by, eng.local_user());
        assert!(last.by.contains('@') || last.by == "user", "a person, not a process: {}", last.by);

        send(&mut eng, Cmd::CardMove { id, column: "Doing".into(), after: None });
        assert_eq!(kanban_replies(&mut rx).pop().unwrap().cards[0].column, "Doing");

        send(&mut eng, Cmd::CardArchive { id, archived: true });
        assert!(kanban_replies(&mut rx).pop().unwrap().cards[0].archived);
        kill_all(&mut eng);
    }

    /// A refused command still answers, or the view keeps showing the move it optimistically
    /// drew. Staying quiet on failure is the one thing a board must not do.
    #[test]
    fn a_refused_card_command_still_sends_the_board_back() {
        let mut eng = engine_with_boards("refused");
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });

        handle_client_frame(
            &mut eng,
            1,
            ClientFrame::Command(Cmd::CardNew {
                space: None,
                column: "Todo".into(),
                title: "   ".into(),
            }),
        );
        let reply = kanban_replies(&mut rx).pop().expect("it answered anyway");
        assert!(reply.cards.is_empty(), "and refused the card");
        kill_all(&mut eng);
    }

    /// The one seam between the two boards, driven from the client rather than the clock.
    #[test]
    fn handing_a_card_over_puts_a_real_task_on_the_agents_board() {
        let mut eng = engine_with_boards("handoff");
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });
        let space = eng.session.spaces[0].id;
        let name = eng.session.spaces[0].name.clone();

        handle_client_frame(
            &mut eng,
            1,
            ClientFrame::Command(Cmd::CardNew {
                space: Some(space),
                column: "Todo".into(),
                title: "wire up the importer".into(),
            }),
        );
        let id = kanban_replies(&mut rx).pop().unwrap().cards[0].id;
        handle_client_frame(&mut eng, 1, ClientFrame::Command(Cmd::CardHandOff { id }));

        let task = eng.board.all().last().expect("a task on the agents' board").clone();
        assert_eq!(task.text, "wire up the importer");
        assert_eq!(task.space.as_deref(), Some(name.as_str()), "scoped to the card's project");
        assert_eq!(task.by, "kanban", "and says where it came from");

        let card = kanban_replies(&mut rx).pop().unwrap().cards[0].clone();
        assert_eq!(card.handed, Some(task.id));
        assert!(card.comments.iter().any(|c| c.by == "horde"), "the card records that it went");

        // Handing it over twice would put the same work on the board again.
        handle_client_frame(&mut eng, 1, ClientFrame::Command(Cmd::CardHandOff { id }));
        assert_eq!(eng.board.all().len(), 1, "once only");
        kill_all(&mut eng);
    }

    /// An agent has to be told which tree to work in, which is the same failure `tasks.rs`
    /// scoping exists to prevent — in the other direction.
    #[test]
    fn a_card_with_no_project_is_never_handed_over() {
        let mut eng = engine_with_boards("no-project");
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });
        handle_client_frame(
            &mut eng,
            1,
            ClientFrame::Command(Cmd::CardNew {
                space: None,
                column: "Todo".into(),
                title: "not about a repo".into(),
            }),
        );
        let id = kanban_replies(&mut rx).pop().unwrap().cards[0].id;
        assert!(hand_over(&mut eng, id).is_err());
        assert!(eng.board.all().is_empty());
        kill_all(&mut eng);
    }

    /// The switch that closes the agents' board has to close everything that puts work on it,
    /// or it is a promise honoured in one place and broken in another.
    #[test]
    fn closing_the_agents_board_closes_the_bridge_too() {
        let mut eng = engine_with_boards("board-off");
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });
        let space = eng.session.spaces[0].id;
        handle_client_frame(
            &mut eng,
            1,
            ClientFrame::Command(Cmd::CardNew {
                space: Some(space),
                column: "Todo".into(),
                title: "work".into(),
            }),
        );
        let id = kanban_replies(&mut rx).pop().unwrap().cards[0].id;

        eng.cfg.board = false;
        assert!(hand_over(&mut eng, id).is_err(), "the client-driven half refuses");
        eng.hand_over_due_cards();
        assert!(eng.board.all().is_empty(), "and so does the one on the clock");
        kill_all(&mut eng);
    }

    /// Renaming a column carries its cards, which is what keeps an edit to the configured
    /// list from orphaning work.
    #[test]
    fn renaming_a_column_over_the_wire_carries_its_cards() {
        let mut eng = engine_with_boards("rename");
        let (tx, mut rx) = mpsc::unbounded_channel();
        eng.clients.insert(1, Client { out: tx, needs_full: Vec::new(), cols: 120, rows: 40 });
        handle_client_frame(
            &mut eng,
            1,
            ClientFrame::Command(Cmd::CardNew {
                space: None,
                column: "Todo".into(),
                title: "work".into(),
            }),
        );
        let _ = kanban_replies(&mut rx);
        handle_client_frame(
            &mut eng,
            1,
            ClientFrame::Command(Cmd::ColumnRename { from: "Todo".into(), to: "Next".into() }),
        );
        assert_eq!(kanban_replies(&mut rx).pop().unwrap().cards[0].column, "Next");
        kill_all(&mut eng);
    }

    /// A configured variable has to reach the program, not merely be stored.
    ///
    /// This is how a provider key gets to an agent, and the failure mode if it does not arrive is
    /// silent: the agent starts, cannot authenticate, and says so in its own words somewhere in
    /// its own UI. So the assertion goes all the way through a real PTY to a real child.
    #[test]
    fn configured_env_reaches_the_program_in_the_pane() {
        // One variable, one line. A bare `env` prints more than a pane has rows and the answer
        // scrolls off the top before anything can read it.
        let mut eng = engine_saying("--env HORDE_ENV_TEST");
        let pane = *eng.session.panes.keys().next().unwrap();
        // The value is printed alone, so the value is the whole assertion.
        wait_for_text(&mut eng, pane, "sk-or-test");
        kill_all(&mut eng);
    }


    /// A handover written into `.horde/` is derived state horde may delete. The record of how
    /// a piece of work changed hands should outlive the pane, which means the vault — and it
    /// has to link both ends, because the only route anyone will actually use to find it later
    /// is the graph or a search for the agent's name.
    ///
    /// Tests the filing rather than the succession around it: succession is covered above, and
    /// driving it twice in one suite means two engines writing the same shared bus log.
    #[test]
    fn a_handover_is_filed_in_the_vault_and_links_both_ends() {
        let mut eng = engine();
        let home = std::env::temp_dir().join(format!("horde-handoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        eng.cfg.vault_home = home.clone();
        let pane = *eng.session.panes.keys().next().unwrap();
        give_agent_named(&mut eng.session, pane, "scribe");

        // Named after an agent no other test uses: `.horde/handoff-{name}.md` lives in a cwd
        // every test pane shares, so a common name is a failure two tests away from its cause.
        let cwd = eng.session.panes[&pane].cwd.clone();
        let left = cwd.join(".horde/handoff-scribe.md");
        std::fs::create_dir_all(cwd.join(".horde")).unwrap();
        std::fs::write(&left, "Half-done: the parser. Do not touch the lexer.").unwrap();

        let filed = file_handoff(&mut eng, pane, "scribe", "scribe-next").expect("filed");
        let text = std::fs::read_to_string(&filed).unwrap();

        assert!(
            filed.parent().is_some_and(|d| d.ends_with(vault::AGENT_DIR)),
            "with the rest of what horde wrote: {}",
            filed.display()
        );
        assert!(text.contains("by: scribe"), "credited to whoever wrote it: {text}");
        assert!(text.contains("Do not touch the lexer"), "its own words survive: {text}");
        assert!(text.contains("[[scribe-next]]"), "linked to who took over: {text}");
        assert!(text.contains("Project: [["), "and to the project: {text}");

        // An agent that left nothing has nothing to file, and that is not a failure.
        std::fs::remove_file(&left).unwrap();
        assert!(file_handoff(&mut eng, pane, "scribe", "scribe-next").is_none());
        kill_all(&mut eng);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The silent-death path: no warning, no note, and a successor still appears — briefed with
    /// everything horde could see.
    #[test]
    fn an_agent_that_died_without_handing_over_gets_a_successor() {
        let mut eng = engine_saying("reached your usage limit");
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

        wait_for_text(&mut eng, dead, "usage limit");

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
        let mut eng = engine_saying("reached your usage limit");
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

        wait_for_text(&mut eng, dead, "usage limit");

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
        let mut eng = engine_saying("Approaching usage limit");
        let pane = *eng.session.panes.keys().next().unwrap();
        eng.cfg.handover = crate::config::Handover {
            warning: vec!["Approaching usage limit".into()],
            exhausted: Vec::new(),
            profile: Some("free".into()),
            instruct: None,
            max_chain: 3,
        };
        give_agent_named(&mut eng.session, pane, "builder");

        wait_for_text(&mut eng, pane, "Approaching");

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
        let mut eng = engine_saying("Approaching usage limit");
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
        let mut eng = engine_saying("Rate limit exceeded");
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

        wait_for_text(&mut eng, pane, "Rate limit");

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
        let mut eng = engine_saying("Rate limit exceeded");
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

        wait_for_text(&mut eng, pane, "Rate limit");

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

    /// Pump `pane` until its screen contains `needle`, and fail saying so if it never does.
    ///
    /// Every handover and model test needs the same thing first: a real process has to get far
    /// enough to print the line the feature triggers on. Five of them spelled that out inline as
    /// "loop until the deadline, then carry on regardless", which made two of them flaky and one
    /// of them worse than flaky — the test that asserts *no* successor appears passed for the
    /// wrong reason whenever the process was slow, because a screen that never showed the
    /// message also produces no successor.
    ///
    /// So the wait asserts. The deadline is generous because the cost is asymmetric: too short
    /// fails a good build, and too long only matters on a build that is already failing.
    fn wait_for_text(eng: &mut Engine, pane: PaneId, needle: &str) {
        let theme = eng.cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            eng.session.panes.get_mut(&pane).unwrap().pump(&theme);
            if eng.session.panes[&pane].visible_text().join("").contains(needle) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let screen = eng.session.panes[&pane].visible_text().join("");
        kill_all(eng);
        panic!("the pane never printed {needle:?} — nothing was triggered. Screen was {screen:?}");
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
        eng.board.add(tasks::NewTask::new(text, "user", Some(&space))).unwrap();
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

    /// Give an enlisted agent a role, the way `spawn --role` does.
    fn label(eng: &mut Engine, worker: &str, role: &str) {
        let pane = *eng
            .session
            .panes
            .iter()
            .find(|(_, p)| p.agent.as_ref().is_some_and(|a| a.name == worker))
            .map(|(id, _)| id)
            .expect("that worker exists");
        eng.session.set_pane_role(pane, role);
    }

    /// Add role-tagged work to the fixture's own project.
    fn add_task_for(eng: &mut Engine, text: &str, role: &str) {
        let space = fixture_space(eng);
        eng.board
            .add(tasks::NewTask {
                role: Some(role),
                ..tasks::NewTask::new(text, "pm", Some(&space))
            })
            .unwrap();
    }

    /// The dispatcher wakes the agent the work is *for*.
    ///
    /// Not the one that has been idle longest, which is the rule for general work and would here
    /// mean a builder woken for a task it cannot claim: a turn spent finding out, and a reviewer
    /// still sitting idle beside it.
    #[test]
    fn role_tagged_work_wakes_the_role_it_names() {
        let mut eng = engine_with_idle_agents("role-nudge", 2);
        label(&mut eng, "worker0", "builder");
        label(&mut eng, "worker1", "reviewer");
        add_task_for(&mut eng, "review the diff", "reviewer");

        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, "worker1", "the reviewer, not whoever was idle longest");
        assert!(sent[0].1.contains("for reviewer"), "and it says whose work it is: {sent:?}");
        kill_all(&mut eng);
    }

    /// Nobody is woken for work they could not claim if they tried.
    ///
    /// The failure this prevents is the expensive kind: a nudge costs the recipient a whole turn,
    /// and a turn that ends in `horde task claim` printing nothing is a turn spent on nothing.
    #[test]
    fn an_agent_is_never_woken_for_work_its_role_cannot_take() {
        let mut eng = engine_with_idle_agents("role-wrong", 1);
        label(&mut eng, "worker0", "builder");
        add_task_for(&mut eng, "review the diff", "reviewer");

        for _ in 0..3 {
            assert!(
                nudge_bodies(&eng.nudge_for_tasks()).is_empty(),
                "a builder must not be woken for a reviewer's task"
            );
        }
        // And the work is visibly stuck rather than quietly waiting.
        let space = fixture_space(&eng);
        let present = eng.roles_enlisted_in(&space);
        assert_eq!(present, ["builder"], "{present:?}");
        assert_eq!(eng.board.stranded(&space, &present).len(), 1);
        kill_all(&mut eng);
    }

    /// General work goes to a general hand before it goes to a specialist.
    ///
    /// Spending the reviewer on work that named nobody is how the one task only the reviewer
    /// could have taken ends up waiting for the reviewer.
    #[test]
    fn general_work_prefers_an_unlabelled_agent_over_a_specialist() {
        let mut eng = engine_with_idle_agents("role-general", 2);
        // worker0 has been idle longest and is the specialist, so "longest idle" alone would
        // pick it. The role is what makes worker1 the right answer.
        label(&mut eng, "worker0", "reviewer");
        add_task(&mut eng, "anything");

        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, "worker1", "the unlabelled one: {sent:?}");
        kill_all(&mut eng);
    }

    /// Work is accounted per role, not in one pool.
    ///
    /// Two reviewer tasks and one reviewer means one nudge; the builder beside it is not woken to
    /// make up the number, and is not held back either when its own work arrives.
    #[test]
    fn the_wake_cap_counts_each_roles_work_separately() {
        let mut eng = engine_with_idle_agents("role-cap", 2);
        label(&mut eng, "worker0", "reviewer");
        label(&mut eng, "worker1", "builder");
        add_task_for(&mut eng, "review one", "reviewer");
        add_task_for(&mut eng, "review two", "reviewer");

        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "two reviewer tasks, one reviewer: {sent:?}");
        assert_eq!(sent[0].0, "worker0");
        // The builder stays asleep however many reviewer tasks pile up.
        add_task_for(&mut eng, "review three", "reviewer");
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty(), "not the builder's work");

        // Its own work still reaches it, unblocked by the reviewer's backlog.
        add_task_for(&mut eng, "build one", "builder");
        let sent = nudge_bodies(&eng.nudge_for_tasks());
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, "worker1");
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
        eng.board.add(tasks::NewTask::new("from last week", "user", Some(&space))).unwrap();
        // Wind it back past the threshold.
        let id = eng.board.all()[0].id;
        eng.board.backdate_for_test(id, tasks::STALE_AFTER.as_millis() as u64 + 60_000);

        assert_eq!(eng.board.offered_to(&space), 0, "stale work is not offered");
        assert!(nudge_bodies(&eng.nudge_for_tasks()).is_empty());
        // Still on the board, still readable, still claimable by name. Stale is not deleted.
        assert_eq!(eng.board.open_count(), 1);
        assert!(eng.board.claim("worker0", Some(id), tasks::Claimant { space: Some(&space), role: None }).unwrap().is_some());
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
        eng.board.claim("worker0", Some(1), Default::default()).unwrap();
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
        eng.board.claim("worker0", Some(1), Default::default()).unwrap();
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
