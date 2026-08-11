//! PTY ownership, in two flavours.
//!
//! A pane's master is either **owned** — we called `openpty` and spawned the child — or
//! **adopted**, handed to us as a bare file descriptor by a previous daemon during a live
//! handoff. The distinction matters in two places: `portable_pty` cannot wrap an existing
//! descriptor, and an adopted pane's child is not our child, so `waitpid` is unavailable.
//!
//! The reader is pausable. That is what makes handoff safe: exactly one process may read a
//! given PTY at a time, so the outgoing daemon has to stop and *acknowledge* that it has
//! stopped before the incoming one starts.

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use portable_pty::{Child, MasterPty, PtySize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// How long the reader waits for data before looping, so it can notice a pause request even
/// when the pane is silent.
const POLL_MS: libc::c_int = 100;

pub enum Master {
    Owned(Box<dyn MasterPty + Send>),
    /// Received over `SCM_RIGHTS`. We hold the master; someone else's child holds the slave.
    Adopted(OwnedFd),
}

impl Master {
    pub fn raw_fd(&self) -> Option<RawFd> {
        match self {
            Master::Owned(m) => m.as_raw_fd(),
            Master::Adopted(fd) => Some(fd.as_raw_fd()),
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        match self {
            Master::Owned(m) => {
                m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
                Ok(())
            }
            Master::Adopted(fd) => {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                // SAFETY: `fd` is a live pty master we own; `ws` outlives the call.
                let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
                if rc == -1 {
                    return Err(std::io::Error::last_os_error()).context("TIOCSWINSZ");
                }
                Ok(())
            }
        }
    }

    /// Foreground process group of the terminal, used to identify what is running.
    pub fn foreground_pgid(&self) -> Option<i32> {
        match self {
            Master::Owned(m) => m.process_group_leader(),
            Master::Adopted(fd) => {
                // SAFETY: `fd` is a live pty master.
                let pgid = unsafe { libc::tcgetpgrp(fd.as_raw_fd()) };
                (pgid >= 0).then_some(pgid)
            }
        }
    }

    /// A writer onto the master. Both flavours go through a plain `File` so there is one
    /// code path, and so an adopted master needs no support from `portable_pty`.
    pub fn writer(&self) -> Result<std::fs::File> {
        let fd = self.raw_fd().ok_or_else(|| anyhow!("pty master has no file descriptor"))?;
        dup_file(fd)
    }

    /// Duplicate the descriptor for sending to a successor daemon.
    pub fn dup_for_handoff(&self) -> Result<OwnedFd> {
        let fd = self.raw_fd().ok_or_else(|| anyhow!("pty master has no file descriptor"))?;
        // SAFETY: `fd` is live; dup returns a new descriptor we take ownership of.
        let new = unsafe { libc::dup(fd) };
        if new == -1 {
            return Err(std::io::Error::last_os_error()).context("dup of pty master");
        }
        // SAFETY: `new` is a fresh descriptor owned by us.
        Ok(unsafe { OwnedFd::from_raw_fd(new) })
    }
}

fn dup_file(fd: RawFd) -> Result<std::fs::File> {
    // SAFETY: `fd` is live; dup yields a new descriptor which the File then owns.
    let new = unsafe { libc::dup(fd) };
    if new == -1 {
        return Err(std::io::Error::last_os_error()).context("dup of pty master");
    }
    // SAFETY: `new` is a fresh descriptor and nothing else holds it.
    Ok(unsafe { std::fs::File::from_raw_fd(new) })
}

/// The process on the other end of the PTY.
pub enum ChildHandle {
    Owned(Box<dyn Child + Send + Sync>),
    /// Adopted across a handoff. We are not this process's parent, so `waitpid` would fail
    /// with ECHILD; liveness is checked with a null signal instead.
    Adopted(i32),
}

impl ChildHandle {
    pub fn pid(&self) -> Option<i32> {
        match self {
            ChildHandle::Owned(c) => c.process_id().map(|p| p as i32),
            ChildHandle::Adopted(pid) => Some(*pid),
        }
    }

    /// Exit status if the process has finished, else None.
    pub fn try_wait(&mut self) -> Option<i32> {
        match self {
            ChildHandle::Owned(c) => match c.try_wait() {
                Ok(Some(s)) => Some(s.exit_code() as i32),
                _ => None,
            },
            ChildHandle::Adopted(pid) => {
                // Signal 0 checks for existence without delivering anything. Once the
                // original parent exited these were reparented to init, which reaps them,
                // so a gone process reports ESRCH rather than lingering as a zombie.
                // SAFETY: kill with signal 0 has no effect beyond the existence check.
                let alive = unsafe { libc::kill(*pid, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
                // Exit code is unknowable for a process we did not wait on.
                (!alive).then_some(0)
            }
        }
    }

    pub fn kill(&mut self) {
        match self {
            ChildHandle::Owned(c) => {
                let _ = c.kill();
            }
            ChildHandle::Adopted(pid) => {
                // SAFETY: sending a signal to a pid we recorded; failure is ignored.
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
        }
    }
}

/// Handle onto the reader thread, so it can be paused and its stopping acknowledged.
pub struct Reader {
    pub rx: UnboundedReceiver<Vec<u8>>,
    paused: Arc<AtomicBool>,
    /// Set by the thread whenever it is not inside a read.
    idle: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Reader {
    /// Stop reading, and wait for the thread to confirm it has stopped.
    ///
    /// Returns false on timeout, which the caller must treat as "do not hand this PTY over":
    /// two processes reading one master would tear the output stream in half.
    pub fn pause(&self, timeout: Duration) -> bool {
        self.paused.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.idle.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Spawn the thread that drains a PTY master into a channel.
///
/// It polls with a timeout rather than blocking forever in `read`, so a pause request is
/// noticed within `POLL_MS` even when the pane is producing nothing.
pub fn spawn_reader(master: &Master, name: String) -> Result<Reader> {
    let fd = master.raw_fd().ok_or_else(|| anyhow!("pty master has no file descriptor"))?;
    let mut file = dup_file(fd)?;
    let read_fd = file.as_raw_fd();

    let (tx, rx): (UnboundedSender<Vec<u8>>, _) = unbounded_channel();
    let paused = Arc::new(AtomicBool::new(false));
    let idle = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let (p, i, s) = (paused.clone(), idle.clone(), stop.clone());
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                if p.load(Ordering::SeqCst) {
                    i.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }

                let mut pfd =
                    libc::pollfd { fd: read_fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: single valid pollfd, count matches.
                let rc = unsafe { libc::poll(&mut pfd, 1, POLL_MS) };
                if rc < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if rc == 0 {
                    // Nothing to read; loop so a pause request is seen promptly.
                    i.store(true, Ordering::SeqCst);
                    continue;
                }

                // Re-check the pause between poll and read: the pause may have arrived while
                // we were waiting, and reading now would steal bytes from a successor.
                if p.load(Ordering::SeqCst) {
                    i.store(true, Ordering::SeqCst);
                    continue;
                }

                i.store(false, Ordering::SeqCst);
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            i.store(true, Ordering::SeqCst);
        })
        .context("spawn pty reader thread")?;

    Ok(Reader { rx, paused, idle, stop })
}

/// Turn a descriptor received over `SCM_RIGHTS` into an adopted master.
pub fn adopt(fd: OwnedFd) -> Master {
    Master::Adopted(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{native_pty_system, CommandBuilder};

    fn open_pty() -> (Master, ChildHandle) {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows: 10, cols: 40, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let child = pair.slave.spawn_command(CommandBuilder::new("cat")).unwrap();
        drop(pair.slave);
        (Master::Owned(pair.master), ChildHandle::Owned(child))
    }

    #[test]
    fn an_owned_master_reads_writes_and_resizes() {
        let (master, mut child) = open_pty();
        let mut reader = spawn_reader(&master, "t".into()).unwrap();
        let mut writer = master.writer().unwrap();
        std::io::Write::write_all(&mut writer, b"hello\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got = Vec::new();
        while Instant::now() < deadline && !String::from_utf8_lossy(&got).contains("hello") {
            if let Ok(b) = reader.rx.try_recv() {
                got.extend(b);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(String::from_utf8_lossy(&got).contains("hello"), "{got:?}");

        master.resize(80, 24).unwrap();
        assert!(master.foreground_pgid().is_some());
        child.kill();
    }

    #[test]
    fn pausing_the_reader_is_acknowledged_and_stops_consuming() {
        let (master, mut child) = open_pty();
        let mut reader = spawn_reader(&master, "t".into()).unwrap();
        let mut writer = master.writer().unwrap();

        assert!(reader.pause(Duration::from_secs(2)), "pause should be acknowledged");
        // The pause was acknowledged above; what matters next is that it consumes nothing.

        // Anything written while paused must stay in the pty, not be drained away.
        std::io::Write::write_all(&mut writer, b"while-paused\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let mut drained = Vec::new();
        while let Ok(b) = reader.rx.try_recv() {
            drained.extend(b);
        }
        assert!(
            !String::from_utf8_lossy(&drained).contains("while-paused"),
            "a paused reader must not consume: {:?}",
            String::from_utf8_lossy(&drained)
        );

        // Resuming picks it up, proving the bytes were still in the pty.
        reader.resume();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got = Vec::new();
        while Instant::now() < deadline && !String::from_utf8_lossy(&got).contains("while-paused") {
            if let Ok(b) = reader.rx.try_recv() {
                got.extend(b);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            String::from_utf8_lossy(&got).contains("while-paused"),
            "resuming should deliver what was buffered"
        );
        child.kill();
    }

    #[test]
    fn a_duplicated_master_can_be_adopted_and_still_works() {
        // This is the handoff in miniature: dup the master, adopt the copy, and drive the
        // same child through it.
        let (master, mut child) = open_pty();
        let dup = master.dup_for_handoff().unwrap();
        let adopted = adopt(dup);

        let mut reader = spawn_reader(&adopted, "t".into()).unwrap();
        let mut writer = adopted.writer().unwrap();
        std::io::Write::write_all(&mut writer, b"through-adopted\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got = Vec::new();
        while Instant::now() < deadline
            && !String::from_utf8_lossy(&got).contains("through-adopted")
        {
            if let Ok(b) = reader.rx.try_recv() {
                got.extend(b);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(String::from_utf8_lossy(&got).contains("through-adopted"), "{got:?}");

        // Resize and pgid work on an adopted master too, without portable_pty's help.
        adopted.resize(100, 30).unwrap();
        assert!(adopted.foreground_pgid().is_some());
        child.kill();
    }

    #[test]
    fn adopted_child_liveness_uses_a_null_signal() {
        let (master, mut owned) = open_pty();
        let pid = owned.pid().unwrap();
        let mut adopted = ChildHandle::Adopted(pid);
        assert_eq!(adopted.try_wait(), None, "a live process must report as running");

        owned.kill();
        // Reap it so the pid is really gone rather than a zombie we still own.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && owned.try_wait().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && adopted.try_wait().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(adopted.try_wait().is_some(), "a dead process must report as exited");
        drop(master);
    }
}
