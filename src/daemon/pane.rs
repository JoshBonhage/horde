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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::proto::{attrs, CursorPos, PaneId, Row, Run, SpaceId, TabId};
use crate::theme::Theme;

/// Minimum PTY geometry. A zero-size PTY makes programs misbehave, so panes clamp here
/// even when the layout squeezes them further.
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

    term: Term<EventProxy>,
    parser: Processor,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    bytes_rx: UnboundedReceiver<Vec<u8>>,
    signal_rx: UnboundedReceiver<PaneSignal>,

    /// Visible grid as of the last `pump`. Authoritative for both rendering and detection.
    mirror: Vec<Row>,
    /// Rows changed since the last `take_dirty`.
    dirty: HashSet<u16>,
    /// Set when a client attaches or the pane resizes, forcing a full repaint.
    full_repaint: bool,
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

        let child = pair.slave.spawn_command(builder).context("failed to spawn command")?;
        // The slave fd must be dropped or the PTY never reports EOF when the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;

        // PTY reads are blocking, so they live on their own thread and hand bytes to the
        // async side through a channel.
        let (bytes_tx, bytes_rx) = unbounded_channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("horde-pty-{id}"))
            .spawn(move || {
                let mut buf = [0u8; 65536];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if bytes_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .context("spawn pty reader thread")?;

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
            term,
            parser: Processor::new(),
            master: pair.master,
            writer,
            child,
            bytes_rx,
            signal_rx,
            mirror: vec![Row::default(); rows as usize],
            dirty: HashSet::new(),
            full_repaint: true,
        })
    }

    /// Drain the PTY, advance the emulator, refresh the mirror. Returns true if anything
    /// changed. Called once per daemon tick.
    pub fn pump(&mut self, theme: &Theme) -> bool {
        let mut got_bytes = false;
        // Bounded per tick so one firehosing pane cannot starve the others.
        for _ in 0..256 {
            match self.bytes_rx.try_recv() {
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

        if self.exited.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(status.exit_code() as i32);
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

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.exited.is_some() {
            return Ok(());
        }
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Typing into a pane implies you want to see the live view again.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        if self.scroll_offset() != 0 {
            self.scroll_bottom();
        }
        self.write(bytes)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        self.cols = cols;
        self.rows = rows;
        self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        self.term.resize(TermSize { cols: cols as usize, rows: rows as usize });
        self.mirror.clear();
        self.mirror.resize(rows as usize, Row::default());
        self.full_repaint = true;
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
        self.master.process_group_leader()
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

pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}
