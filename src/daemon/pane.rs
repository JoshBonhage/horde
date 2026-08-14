//! A pane: one PTY, one VT emulator, and a mirror of its visible grid.
//!
//! The daemon owns the emulator rather than the client. That is load-bearing: agent status
//! detection has to keep working while no client is attached, so something server-side must
//! always be able to see the screen.
//!
//! The `mirror` is the single place the rest of the daemon reads pane contents from —
//! clients diff against it, and agent detection matches against it. Reading the emulator
//! directly from two places would double-consume damage.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::time::Instant;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::pty::{self, ChildHandle, Master, Reader};

use crate::proto::{attrs, CursorPos, PaneId, Row, Run, SpaceId, TabId};
use crate::theme::Theme;

/// Minimum PTY geometry. A zero-size PTY makes programs misbehave, so panes clamp here
/// even when the layout squeezes them further.
/// Most unwritten input to hold for one pane.
///
/// Reached only if a pane stops reading entirely — the bus gate keeps normal traffic far below
/// this. A cap rather than unbounded growth: a wedged agent should fail its writes, not consume
/// memory until the daemon does.
const MAX_OUTBOUND: usize = 256 * 1024;

const MIN_COLS: u16 = 2;
const MIN_ROWS: u16 = 1;

/// Size handed to the emulator. `total_lines == screen_lines` because scrollback depth is
/// governed by `TermConfig::scrolling_history`, not by this trait.
#[derive(Clone, Copy)]
struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Things the emulator wants the daemon to act on.
#[derive(Debug)]
pub enum PaneSignal {
    /// The emulator's reply to a device query. **Must** reach the PTY or programs that
    /// probe the terminal will hang waiting.
    Write(Vec<u8>),
    Title(String),
    Bell,
    /// OSC 52 clipboard write. horde does not forward these to the client yet, so the
    /// payload is deliberately dropped rather than carried and ignored.
    ClipboardStore,
    Wakeup,
}

#[derive(Clone)]
struct EventProxy {
    tx: UnboundedSender<PaneSignal>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let sig = match event {
            TermEvent::PtyWrite(s) => PaneSignal::Write(s.into_bytes()),
            TermEvent::Title(t) => PaneSignal::Title(t),
            TermEvent::ResetTitle => PaneSignal::Title(String::new()),
            TermEvent::Bell => PaneSignal::Bell,
            TermEvent::ClipboardStore(..) => PaneSignal::ClipboardStore,
            // These carry a formatter that turns our answer into the right escape
            // sequence; run it and send the bytes straight back to the PTY.
            TermEvent::ColorRequest(index, fmt) => {
                let theme = Theme::horde();
                let rgb = theme.indexed(index.min(255) as u8);
                let reply = fmt(alacritty_terminal::vte::ansi::Rgb {
                    r: rgb.r,
                    g: rgb.g,
                    b: rgb.b,
                });
                PaneSignal::Write(reply.into_bytes())
            }
            TermEvent::TextAreaSizeRequest(fmt) => {
                // Pixel dimensions are a guess; nothing in a TUI depends on them being
                // exact, but a missing reply would hang the caller.
                let reply = fmt(alacritty_terminal::event::WindowSize {
                    num_lines: 24,
                    num_cols: 80,
                    cell_width: 8,
                    cell_height: 16,
                });
                PaneSignal::Write(reply.into_bytes())
            }
            TermEvent::Wakeup => PaneSignal::Wakeup,
            _ => return,
        };
        let _ = self.tx.send(sig);
    }
}

pub struct Pane {
    pub id: PaneId,
    pub tab: TabId,
    pub space: SpaceId,
    /// Explicit name from `horde pane rename`, which wins over any detected agent name.
    pub name: Option<String>,
    /// Title reported by the program via OSC 0/2.
    pub osc_title: String,
    pub cwd: PathBuf,
    pub cmd: String,
    pub cols: u16,
    pub rows: u16,
    pub exited: Option<i32>,
    /// Set once detection identifies an agent running here.
    pub agent: Option<super::state::AgentRuntime>,
    /// The trigger that started this pane, when horde started it rather than you.
    ///
    /// Two things read it, and both stop working without it: the cap on how many agents horde
    /// may run unattended, and the refusal to let a machine-started agent create more triggers.
    /// Distinguishing the fleet you built from the fleet the machine built is not cosmetic.
    pub spawned_by: Option<u64>,
    /// What this pane is *for*, as you decided: `reviewer`, `builder`, `docs`.
    ///
    /// Three names meet on an agent pane and none substitutes for another. `name` is how you
    /// address it, `agent.kind` is which program was detected, and this is the job. Only the
    /// job recurs across projects, which is what makes it the one worth grouping by.
    ///
    /// Here rather than on `AgentRuntime` for the same reason `name` is: detection creates
    /// and destroys the agent record, and a label you gave should not evaporate because the
    /// process it described exited, or came back as a shell under `restore = false`.
    pub role: Option<String>,
    /// Held at the top of the sidebar's agent list, whichever space it lives in.
    pub pinned: bool,
    /// The pane whose agent asked for this one, when an agent started it rather than you.
    ///
    /// Distinct from `spawned_by`, which records a *trigger*. Both exist to answer "who is
    /// responsible for this pane", and they have different answers and different caps: one
    /// bounds what runs with nobody present, this one bounds what an agent in a loop can do
    /// while you watch.
    pub spawned_by_pane: Option<PaneId>,
    /// This agent has enlisted for board work, so the nudge may tell it about waiting tasks.
    ///
    /// Opt-in, and on the *pane* rather than the agent, for the same reason `role` is: an
    /// agent that goes unrecognised for one detection pass must not quietly resign. Before
    /// this existed every idle agent counted as a volunteer, which is how work added in one
    /// project reached an agent you had left thinking in another.
    pub board: bool,

    term: Term<EventProxy>,
    parser: Processor,
    master: Master,
    writer: std::fs::File,
    child: ChildHandle,
    reader: Reader,
    signal_rx: UnboundedReceiver<PaneSignal>,

    /// Visible grid as of the last `pump`. Authoritative for both rendering and detection.
    mirror: Vec<Row>,
    /// Rows changed since the last `take_dirty`.
    dirty: HashSet<u16>,
    /// The cursor as the client was last told it.
    ///
    /// Kept because the cursor rides along with row updates, and a keystroke can move it without
    /// changing any row — typing a space onto a blank cell rebuilds an identical row. Without
    /// this the pane looks unchanged, nothing is sent, and the cursor sits a column behind until
    /// some later keystroke happens to alter a character.
    pub last_sent_cursor: Option<crate::proto::CursorPos>,
    /// Set when a client attaches or the pane resizes, forcing a full repaint.
    full_repaint: bool,
    /// Bytes to write once their deadline passes.
    ///
    /// This exists for one reason: Enter has to arrive as its own read. Agents treat a chunk
    /// of text and a carriage return arriving together as a *paste*, and a trailing CR in a
    /// paste inserts a literal newline instead of submitting. Writing the text, then the CR
    /// a beat later, makes the CR read as a keypress.
    deferred: Vec<(Instant, Vec<u8>)>,
    /// Bytes accepted for this pane but not yet taken by the tty.
    ///
    /// The master is non-blocking, so a write takes what fits and reports the rest. Holding the
    /// remainder here and pushing it on later ticks is what keeps a slow or wedged agent from
    /// stalling the engine mid-message.
    outbound: Vec<u8>,
}

impl Pane {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: PaneId,
        tab: TabId,
        space: SpaceId,
        cmd: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
        scrollback: usize,
        socket: &Path,
        env: &std::collections::HashMap<String, String>,
    ) -> Result<Pane> {
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty failed")?;

        let mut builder = build_command(cmd);
        builder.cwd(cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        // Panes learn their own identity from the environment, so an agent can run
        // `horde send reviewer "..."` without having to say who it is.
        builder.env("HORDE_SOCKET", socket);
        builder.env("HORDE_PANE", id.to_string());
        builder.env("HORDE_TAB", tab.to_string());
        builder.env("HORDE_SPACE", space.to_string());
        // Anything that shells out to a pager would block on input nobody can give it.
        builder.env("PAGER", "cat");
        // Discovery: an agent has no way to know horde exists, let alone that other agents
        // are reachable. This is the breadcrumb.
        builder.env("HORDE_DOCS", "horde docs orchestration");

        // The user's own environment, last so it can override anything above — including `PAGER`
        // for someone who means it. This is how a provider key reaches an agent.
        //
        // Values are secrets and are never logged. Note what that costs: a mistyped key produces
        // an agent that fails to authenticate with nothing in horde's log to explain it, and that
        // is the right trade. A key in a log file outlives every session that could have used it.
        for (k, v) in env {
            builder.env(k, v);
        }

        let child = pair.slave.spawn_command(builder).with_context(|| {
            // `failed to spawn command` alone sent someone hunting for a config problem when
            // the binary simply was not installed. Name the program and the likely cause.
            let prog = cmd.split_whitespace().next().unwrap_or(cmd);
            format!("could not start {prog:?} — is it installed and on PATH?")
        })?;
        // The slave fd must be dropped or the PTY never reports EOF when the child exits.
        drop(pair.slave);

        let master = Master::Owned(pair.master);
        // Before any write can happen: the engine must never block on a pty.
        master.set_nonblocking()?;
        let writer = master.writer()?;
        let reader = pty::spawn_reader(&master, format!("horde-pty-{id}"))?;
        let child = ChildHandle::Owned(child);

        let (signal_tx, signal_rx) = unbounded_channel::<PaneSignal>();
        let config = TermConfig { scrolling_history: scrollback, ..Default::default() };
        let size = TermSize { cols: cols as usize, rows: rows as usize };
        let term = Term::new(config, &size, EventProxy { tx: signal_tx });

        Ok(Pane {
            id,
            tab,
            space,
            name: None,
            osc_title: String::new(),
            cwd: cwd.to_path_buf(),
            cmd: cmd.to_string(),
            cols,
            rows,
            exited: None,
            agent: None,
            spawned_by: None,
            role: None,
            pinned: false,
            board: false,
            spawned_by_pane: None,
            term,
            parser: Processor::new(),
            master,
            writer,
            child,
            reader,
            signal_rx,
            mirror: vec![Row::default(); rows as usize],
            dirty: HashSet::new(),
            last_sent_cursor: None,
            full_repaint: true,
            deferred: Vec::new(),
            outbound: Vec::new(),
        })
    }

    /// Drain the PTY, advance the emulator, refresh the mirror. Returns true if anything
    /// changed. Called once per daemon tick.
    pub fn pump(&mut self, theme: &Theme) -> bool {
        let mut got_bytes = false;
        // Bounded per tick so one firehosing pane cannot starve the others.
        for _ in 0..256 {
            match self.reader.rx.try_recv() {
                Ok(chunk) => {
                    self.parser.advance(&mut self.term, &chunk);
                    got_bytes = true;
                }
                Err(_) => break,
            }
        }

        let mut signals = Vec::new();
        while let Ok(sig) = self.signal_rx.try_recv() {
            signals.push(sig);
        }
        for sig in signals {
            match sig {
                PaneSignal::Write(bytes) => {
                    let _ = self.write(&bytes);
                }
                PaneSignal::Title(t) => self.osc_title = t,
                PaneSignal::Bell | PaneSignal::ClipboardStore | PaneSignal::Wakeup => {}
            }
        }

        // Flush any deferred writes whose moment has come.
        let now = Instant::now();
        let due: Vec<Vec<u8>> = {
            let (due, pending): (Vec<_>, Vec<_>) =
                std::mem::take(&mut self.deferred).into_iter().partition(|(at, _)| *at <= now);
            self.deferred = pending;
            due.into_iter().map(|(_, b)| b).collect()
        };
        for bytes in due {
            let _ = self.write(&bytes);
        }
        // Whatever the tty could not take last tick goes out now.
        if !self.outbound.is_empty() {
            self.push_outbound();
        }

        if self.exited.is_none() {
            if let Some(code) = self.child.try_wait() {
                self.exited = Some(code);
            }
        }

        let changed = self.refresh_mirror(theme);
        got_bytes || changed
    }

    /// Rebuild changed mirror rows from the emulator grid.
    fn refresh_mirror(&mut self, theme: &Theme) -> bool {
        let rows = self.rows as usize;
        if self.mirror.len() != rows {
            self.mirror.resize(rows, Row::default());
            self.full_repaint = true;
        }

        // Which viewport rows to rebuild. Damage is reported relative to the live view, so
        // while scrolled back it does not describe what is on screen — rebuild everything.
        let scrolled = self.term.grid().display_offset() != 0;
        let targets: Vec<usize> = match self.term.damage() {
            TermDamage::Full => (0..rows).collect(),
            TermDamage::Partial(iter) => {
                let lines: Vec<usize> = iter.map(|d| d.line).collect();
                if scrolled || self.full_repaint {
                    (0..rows).collect()
                } else {
                    lines.into_iter().filter(|&l| l < rows).collect()
                }
            }
        };
        self.term.reset_damage();

        let targets = if self.full_repaint { (0..rows).collect() } else { targets };
        self.full_repaint = false;

        let mut changed = false;
        for y in targets {
            let row = self.build_row(y, theme);
            if self.mirror[y] != row {
                self.mirror[y] = row;
                self.dirty.insert(y as u16);
                changed = true;
            }
        }
        changed
    }

    /// Read one viewport row out of the grid and run-length encode it by style.
    fn build_row(&self, y: usize, theme: &Theme) -> Row {
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;
        // Viewport row 0 is `Line(-offset)`; scrolling back shows history above it.
        let line = Line(y as i32 - offset);
        let cols = self.cols as usize;

        let mut runs: Vec<Run> = Vec::new();
        let grid_row = &grid[line];

        for x in 0..cols {
            let cell = &grid_row[Column(x)];
            let flags = cell.flags;

            // The second half of a wide character is a spacer holding no content of its
            // own; emitting it would push the row one column too wide.
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }

            let mut fg = theme.resolve(cell.fg);
            let mut bg = theme.resolve(cell.bg);
            let mut a = 0u8;
            if flags.contains(Flags::BOLD) {
                a |= attrs::BOLD;
            }
            if flags.contains(Flags::DIM) {
                a |= attrs::DIM;
            }
            if flags.contains(Flags::ITALIC) {
                a |= attrs::ITALIC;
            }
            if flags.intersects(Flags::ALL_UNDERLINES) {
                a |= attrs::UNDERLINE;
            }
            if flags.contains(Flags::STRIKEOUT) {
                a |= attrs::STRIKEOUT;
            }
            // Resolve inverse here so the client never has to think about it.
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }

            let ch = if flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };

            match runs.last_mut() {
                Some(run) if run.fg == fg && run.bg == bg && run.attrs == a => {
                    run.text.push(ch);
                    if let Some(zw) = cell.zerowidth() {
                        run.text.extend(zw.iter().copied());
                    }
                }
                _ => {
                    let mut text = String::with_capacity(8);
                    text.push(ch);
                    if let Some(zw) = cell.zerowidth() {
                        text.extend(zw.iter().copied());
                    }
                    runs.push(Run { text, fg, bg, attrs: a });
                }
            }
        }

        Row { runs }
    }

    /// Rows changed since the last call, clearing the dirty set.
    pub fn take_dirty(&mut self) -> Vec<u16> {
        let mut v: Vec<u16> = self.dirty.drain().collect();
        v.sort_unstable();
        v
    }

    pub fn mirror(&self) -> &[Row] {
        &self.mirror
    }

    pub fn row(&self, y: u16) -> Option<&Row> {
        self.mirror.get(y as usize)
    }

    /// Visible text, one string per row, trailing blanks trimmed. This is what agent
    /// detection matches against and what `horde pane read` returns.
    pub fn visible_text(&self) -> Vec<String> {
        self.mirror
            .iter()
            .map(|r| {
                let s: String = r.runs.iter().map(|run| run.text.as_str()).collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    /// The last `n` non-blank rows from the bottom of the live view.
    ///
    /// Detection reads from here rather than the scrolled viewport, so scrolling back
    /// never changes what horde thinks an agent is doing.
    pub fn detection_snapshot(&self, n: usize) -> Vec<String> {
        let all = self.visible_text();
        let end = all.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(0);
        let start = end.saturating_sub(n);
        all[start..end].to_vec()
    }

    pub fn cursor(&self) -> CursorPos {
        let grid = self.term.grid();
        let Point { line, column } = grid.cursor.point;
        let offset = grid.display_offset() as i32;
        let y = line.0 + offset;
        CursorPos {
            x: column.0 as u16,
            y: y.max(0) as u16,
            // Hidden by the program, or scrolled out of view.
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR)
                && y >= 0
                && y < self.rows as i32,
        }
    }

    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// True when the program asked to receive mouse events, in which case the client should
    /// forward them instead of using the mouse for horde's own UI.
    pub fn wants_mouse(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    pub fn scroll(&mut self, lines: i32) {
        use alacritty_terminal::grid::Scroll;
        // Scrolling changes which grid lines are visible without generating damage, so
        // force a repaint explicitly.
        self.term.scroll_display(Scroll::Delta(lines));
        self.full_repaint = true;
    }

    pub fn scroll_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        self.term.scroll_display(Scroll::Bottom);
        self.full_repaint = true;
    }

    /// Accept bytes for the pane, pushing as much as the tty will take right now.
    ///
    /// Never blocks. Whatever the terminal cannot take yet stays buffered and goes out on
    /// following ticks, so a long message to a slow agent costs no engine time.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.exited.is_some() {
            return Ok(());
        }
        // A pane that has stopped reading entirely must not grow this without limit. The bus
        // gate normally prevents it from getting close; this is the backstop.
        if self.outbound.len() + bytes.len() > MAX_OUTBOUND {
            return Err(anyhow!("pane {} is not reading its input", self.id));
        }
        self.outbound.extend_from_slice(bytes);
        self.push_outbound();
        Ok(())
    }

    /// Hand the buffer to the tty until it stops accepting.
    fn push_outbound(&mut self) {
        while !self.outbound.is_empty() {
            match self.writer.write(&self.outbound) {
                Ok(0) => break,
                Ok(n) => {
                    self.outbound.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // The tty is full. The rest waits for a later tick.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // The pane is gone; holding its bytes forever would leak.
                Err(_) => {
                    self.outbound.clear();
                    break;
                }
            }
        }
        let _ = self.writer.flush();
    }

    /// Typing into a pane implies you want to see the live view again.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        if self.scroll_offset() != 0 {
            self.scroll_bottom();
        }
        self.write(bytes)
    }

    // -- live handoff -----------------------------------------------------

    /// Stop reading this pane's PTY and wait for the reader to confirm it has stopped.
    ///
    /// Returns false on timeout. A caller that cannot get confirmation must abandon the
    /// handoff: two processes reading one master would tear the output stream apart.
    pub fn pause_reader(&self, timeout: std::time::Duration) -> bool {
        self.reader.pause(timeout)
    }

    pub fn resume_reader(&self) {
        self.reader.resume();
    }

    /// Everything a successor daemon needs to take this pane over, plus a duplicate of the
    /// PTY master to go with it.
    ///
    /// Call only while the reader is paused, so `pending` really is everything outstanding.
    pub fn export(&mut self) -> Result<(super::handoff::HPane, std::os::fd::OwnedFd)> {
        // Anything already read but not yet fed to the emulator travels with the manifest;
        // dropping it would lose whatever the pane printed in the last instant.
        let mut pending = Vec::new();
        while let Ok(chunk) = self.reader.rx.try_recv() {
            pending.extend(chunk);
        }
        let cursor = self.cursor();
        let fd = self.master.dup_for_handoff()?;
        Ok((
            super::handoff::HPane {
                pid: self.child.pid().unwrap_or(0),
                cmd: self.cmd.clone(),
                cwd: self.cwd.to_string_lossy().to_string(),
                name: self.name.clone(),
                osc_title: self.osc_title.clone(),
                cols: self.cols,
                rows: self.rows,
                agent: self.agent.as_ref().map(|a| super::handoff::HAgent {
                    kind: a.kind.clone(),
                    name: a.name.clone(),
                    state: a.state,
                    authority: a.authority.clone(),
                    reason: a.reason.clone(),
                    seen: a.seen,
                    session_id: a.session_id.clone(),
                    queued: a.queued.clone(),
                }),
                spawned_by: self.spawned_by,
                role: self.role.clone(),
                pinned: self.pinned,
                board: self.board,
                spawned_by_pane: self.spawned_by_pane,
                screen: self.mirror.clone(),
                pending,
                cursor_x: cursor.x,
                cursor_y: cursor.y,
            },
            fd,
        ))
    }

    /// Rebuild a pane around a PTY master received from a predecessor.
    ///
    /// The child process is untouched and unaware: it is still attached to the slave side.
    /// We are not its parent, so liveness moves to a null-signal check.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        id: PaneId,
        tab: TabId,
        space: SpaceId,
        saved: &super::handoff::HPane,
        fd: std::os::fd::OwnedFd,
        scrollback: usize,
        theme: &Theme,
    ) -> Result<Pane> {
        let master = pty::adopt(fd);
        // Before any write can happen: the engine must never block on a pty.
        master.set_nonblocking()?;
        let writer = master.writer()?;
        let reader = pty::spawn_reader(&master, format!("horde-pty-{id}"))?;

        let (signal_tx, signal_rx) = unbounded_channel::<PaneSignal>();
        let config = TermConfig { scrolling_history: scrollback, ..Default::default() };
        let size = TermSize { cols: saved.cols as usize, rows: saved.rows as usize };
        let term = Term::new(config, &size, EventProxy { tx: signal_tx });

        let mut pane = Pane {
            id,
            tab,
            space,
            name: saved.name.clone(),
            osc_title: saved.osc_title.clone(),
            cwd: PathBuf::from(&saved.cwd),
            cmd: saved.cmd.clone(),
            cols: saved.cols,
            rows: saved.rows,
            exited: None,
            spawned_by: saved.spawned_by,
            role: saved.role.clone(),
            pinned: saved.pinned,
            board: saved.board,
            spawned_by_pane: saved.spawned_by_pane,
            agent: saved.agent.as_ref().map(|a| super::state::AgentRuntime {
                kind: a.kind.clone(),
                name: a.name.clone(),
                // Not persisted: the next scan reads it back off the manifest, and guessing
                // here would outlive a manifest the user has since changed.
                class: Default::default(),
                state: a.state,
                // The clock restarts; the alternative is serialising a monotonic instant,
                // which is meaningless in another process.
                since: Instant::now(),
                authority: a.authority.clone(),
                reason: a.reason.clone(),
                seen: a.seen,
                session_id: a.session_id.clone(),
                queued: a.queued.clone(),
                question: None,
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                alerted_since: None,
            }),
            term,
            parser: Processor::new(),
            master,
            writer,
            child: ChildHandle::Adopted(saved.pid),
            reader,
            signal_rx,
            mirror: vec![Row::default(); saved.rows as usize],
            dirty: HashSet::new(),
            last_sent_cursor: None,
            full_repaint: true,
            deferred: Vec::new(),
            outbound: Vec::new(),
        };

        // Repaint the emulator to what was on screen, then apply anything the predecessor
        // had read but not yet processed. Without this the pane would come back blank until
        // whatever is running happened to redraw.
        let replay =
            super::handoff::screen_to_ansi(&saved.screen, saved.cursor_x, saved.cursor_y);
        pane.parser.advance(&mut pane.term, &replay);
        if !saved.pending.is_empty() {
            pane.parser.advance(&mut pane.term, &saved.pending);
        }
        pane.refresh_mirror(theme);
        pane.request_full_repaint();
        Ok(pane)
    }

    /// Queue bytes to be written after `delay`.
    pub fn write_later(&mut self, bytes: Vec<u8>, delay: std::time::Duration) {
        self.deferred.push((Instant::now() + delay, bytes));
    }

    /// Whether the pty will take a write now. See [`Master::writable`].
    ///
    /// The timeout is deliberately tiny: this runs on the engine thread once per delivery, and
    /// a target that cannot take input within a few milliseconds is better queued than waited
    /// on. It flushes on a later pass at no cost.
    pub fn accepts_input(&self) -> bool {
        self.master.writable(5)
    }

    /// Longest text this pane can accept in one line without the tty discarding the tail.
    ///
    /// `None` means unlimited: a raw-mode terminal has no line limit, and `write_all` loops
    /// over the buffer-sized pieces the master accepts. Canonical mode caps a line at
    /// `MAX_CANON` and drops the rest silently, so a long message has to be refused rather
    /// than half-delivered.
    pub fn max_input_line(&self) -> Option<usize> {
        match self.master.input_is_canonical() {
            // A little headroom under MAX_CANON (1024): the limit counts the whole line, and
            // anything already typed at that prompt counts against it too.
            Some(true) => Some(900),
            _ => None,
        }
    }

    /// True while anything is still on its way into this pane — bytes the tty has not taken
    /// yet, or a timed write such as the submitting Enter.
    ///
    /// Anything about to type into this pane must wait for both, or its text would land inside
    /// the previous message or in front of a submit that has not fired.
    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty() || !self.outbound.is_empty()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        self.cols = cols;
        self.rows = rows;
        self.master.resize(cols, rows)?;
        self.term.resize(TermSize { cols: cols as usize, rows: rows as usize });
        self.mirror.clear();
        self.mirror.resize(rows as usize, Row::default());
        self.full_repaint = true;
        Ok(())
    }

    /// Make the program redraw, without its size having changed.
    ///
    /// `request_full_repaint` only re-sends the mirror horde already holds; it cannot conjure
    /// content the program never drew. After a resize the emulator keeps whatever was last
    /// painted, so a program that misses its `SIGWINCH` — or repaints while a drag is still
    /// delivering sizes — leaves its output sitting in a corner of a pane that has since grown.
    /// Nothing horde can ask the emulator will fix that. Only the program can.
    ///
    /// So the size is wobbled: one cell shorter, then back. Two `SIGWINCH`es the program cannot
    /// mistake for the size it already believes it has, which is the one thing every terminal
    /// program is guaranteed to repaint for. A same-size `TIOCSWINSZ` would be tidier, but
    /// whether it signals at all is up to the platform.
    pub fn force_redraw(&mut self) -> Result<()> {
        let (cols, rows) = (self.cols, self.rows);
        if rows > MIN_ROWS {
            self.resize(cols, rows - 1)?;
        } else {
            self.resize(cols + 1, rows)?;
        }
        self.resize(cols, rows)?;
        Ok(())
    }

    pub fn request_full_repaint(&mut self) {
        self.full_repaint = true;
        for y in 0..self.rows {
            self.dirty.insert(y);
        }
    }

    /// Best-effort display name: explicit rename, then OSC title, then the command.
    pub fn display_name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        if !self.osc_title.is_empty() {
            return self.osc_title.clone();
        }
        self.cmd
            .split_whitespace()
            .next()
            .and_then(|s| Path::new(s).file_name())
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "shell".to_string())
    }

    /// Foreground process group of the PTY. Detection uses this to work out which program
    /// is actually in charge, which is not necessarily what we spawned.
    pub fn foreground_pgid(&self) -> Option<i32> {
        self.master.foreground_pgid()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// Split a command string into a program plus arguments.
///
/// Quoting is deliberately not handled: commands come from config, keybindings, or
/// `--cmd`, and anything needing quotes can be written as `sh -c '...'`.
fn build_command(cmd: &str) -> CommandBuilder {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some(prog) => {
            let mut b = CommandBuilder::new(prog);
            for a in parts {
                b.arg(a);
            }
            b
        }
        None => CommandBuilder::new(default_shell()),
    }
}

/// The shell to open a pane with when nothing else says.
///
/// `$SHELL` answers this in any session a human started, so the fallback only fires for a daemon
/// launched with a thin environment — which is precisely where guessing wrong is unrecoverable,
/// because there is no prompt to report the error to. `/bin/zsh` is the right guess on macOS,
/// where it is the login default, and the wrong one everywhere else: a stock Ubuntu — including
/// every WSL distro — does not ship it, and the pane dies with `ENOENT` instead of opening.
/// `/bin/sh` is the only shell POSIX actually promises, so it is what the guess falls back to.
///
/// An empty `$SHELL` is treated as unset. It is not a path, and `CommandBuilder` would fail on
/// it the same way a missing zsh does.
pub fn default_shell() -> String {
    match std::env::var("SHELL") {
        Ok(s) if !s.is_empty() => s,
        _ if cfg!(target_os = "macos") => "/bin/zsh".to_string(),
        _ => "/bin/sh".to_string(),
    }
}

#[cfg(test)]
mod tests {
    /// Whatever this resolves to has to be something the OS can actually exec.
    ///
    /// Asserted against the filesystem rather than against a literal, because the bug this
    /// replaces was not a wrong string — it was a string that named nothing. Deliberately does
    /// not set `$SHELL`: tests run in parallel and the environment is shared, so this checks
    /// whichever branch the host lands on, and CI runs one job with `$SHELL` unset to cover the
    /// fallback that no developer machine ever takes.
    #[test]
    fn the_default_shell_is_a_program_that_exists() {
        let sh = super::default_shell();
        assert!(
            std::path::Path::new(&sh).exists(),
            "default_shell() returned {sh:?}, which is not on this filesystem"
        );
    }
}
