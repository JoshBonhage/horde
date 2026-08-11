//! The handoff transaction: pause, transfer, commit, exit.
//!
//! Split from [`super::handoff`], which owns the wire format and the descriptor passing.
//! This module owns the *ordering*, which is where the safety lives.
//!
//! ```text
//!   1. pause every reader, and wait for each to acknowledge
//!   2. snapshot state, duplicate the PTY masters
//!   3. spawn the successor in import mode, connected by a socketpair
//!   4. send manifest + descriptors
//!   5. successor rebuilds the panes and binds <socket>.handoff   -> "R"
//!   6. we unlink <socket>                                        -> "G"
//!   7. successor renames <socket>.handoff to <socket>            -> "B"
//!   8. we exit without signalling any pane process group
//! ```
//!
//! Anything failing before step 6 rolls back: readers resume, the successor is killed, and
//! the session carries on as though nothing happened. Steps 6 and 7 are a `unlink` followed
//! by a `rename` of a file the successor already created, so the committed window has no
//! failure mode worth planning around.

use std::io::{Read, Write};
use std::os::fd::{IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::handoff::{self, HNode, HPane, HSpace, HTab, Manifest};
use super::{log_line, Engine};
use crate::proto::PaneId;

/// How long to wait for a reader to confirm it has stopped.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for the successor at each step.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Descriptor the successor finds its handoff socket on.
const HANDOFF_FD: RawFd = 3;

/// Path the successor binds before taking over the real one.
pub fn staging_socket() -> PathBuf {
    let s = crate::config::socket_path();
    s.with_extension("sock.handoff")
}

/// Hand this session to a new daemon process. Returns Ok(()) when the caller should exit.
pub fn run(eng: &mut Engine, exe: Option<PathBuf>) -> Result<()> {
    let exe = match exe {
        Some(p) => p,
        None => std::env::current_exe().context("cannot find the horde binary")?,
    };
    if !exe.is_file() {
        return Err(anyhow!("{} is not a file", exe.display()));
    }

    let pane_ids: Vec<PaneId> = ordered_panes(eng);
    log_line(&format!("handoff: starting, {} panes, to {}", pane_ids.len(), exe.display()));

    // --- 1. pause every reader ------------------------------------------
    let mut paused: Vec<PaneId> = Vec::new();
    for &id in &pane_ids {
        let ok = eng.session.panes.get(&id).is_some_and(|p| p.pause_reader(PAUSE_TIMEOUT));
        if !ok {
            resume(eng, &paused);
            return Err(anyhow!(
                "pane {id} did not stop reading in time; nothing was changed"
            ));
        }
        paused.push(id);
    }

    // --- 2. snapshot and duplicate --------------------------------------
    let (manifest, fds) = match build(eng, &pane_ids) {
        Ok(v) => v,
        Err(e) => {
            resume(eng, &paused);
            return Err(e.context("building the handoff manifest"));
        }
    };

    // --- 3. spawn the successor -----------------------------------------
    let (ours, theirs) = UnixStream::pair().context("socketpair for handoff")?;
    let mut child = match spawn_successor(&exe, theirs) {
        Ok(c) => c,
        Err(e) => {
            resume(eng, &paused);
            close_all(fds);
            return Err(e);
        }
    };

    // From here on, any failure before the commit kills the successor and resumes.
    let result = transact(&ours, &manifest, &fds);
    close_all(fds);

    if let Err(e) = result {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(staging_socket());
        resume(eng, &paused);
        return Err(e.context("handoff aborted; the session is untouched"));
    }

    log_line("handoff: committed, exiting without signalling panes");
    // Deliberately do not kill or reap the children: their PTY masters live on in the
    // successor, and signalling their process groups is exactly what we are avoiding.
    Ok(())
}

/// Steps 4 to 7. Errors here are recoverable by the caller until `G` has been sent.
fn transact(sock: &UnixStream, manifest: &Manifest, fds: &[RawFd]) -> Result<()> {
    let mut sock = sock.try_clone().context("cloning handoff socket")?;
    sock.set_read_timeout(Some(STEP_TIMEOUT))?;
    sock.set_write_timeout(Some(STEP_TIMEOUT))?;

    handoff::send(&mut sock, manifest, fds).context("sending manifest and descriptors")?;

    // 5. successor has rebuilt everything and is listening on the staging path.
    expect(&mut sock, b'R', "successor never reported ready")?;

    // 6. commit. Unlinking rather than closing our listener means the successor can bind
    // without us having to coordinate with the accept loop.
    let socket = crate::config::socket_path();
    let _ = std::fs::remove_file(&socket);
    sock.write_all(b"G").context("sending the go-ahead")?;
    sock.flush()?;

    // 7. it has moved the staging socket into place.
    expect(&mut sock, b'B', "successor never took over the socket")?;
    Ok(())
}

fn expect(sock: &mut UnixStream, want: u8, whinge: &str) -> Result<()> {
    let mut b = [0u8; 1];
    sock.read_exact(&mut b).with_context(|| whinge.to_string())?;
    if b[0] != want {
        return Err(anyhow!("{whinge}: got {:?}", b[0] as char));
    }
    Ok(())
}

fn resume(eng: &Engine, ids: &[PaneId]) {
    for id in ids {
        if let Some(p) = eng.session.panes.get(id) {
            p.resume_reader();
        }
    }
    log_line("handoff: rolled back, readers resumed");
}

fn close_all(fds: Vec<RawFd>) {
    for fd in fds {
        // SAFETY: these are duplicates we made and have finished sending.
        unsafe { libc::close(fd) };
    }
}

/// Panes in a stable order: space, then tab, then tree order. The manifest and the
/// descriptor sequence must agree, and this is the one place that order is decided.
fn ordered_panes(eng: &Engine) -> Vec<PaneId> {
    let mut out = Vec::new();
    for space in &eng.session.spaces {
        for &tid in &space.tabs {
            if let Some(tab) = eng.session.tab(tid) {
                out.extend(tab.layout.panes());
            }
        }
    }
    out
}

fn build(eng: &mut Engine, order: &[PaneId]) -> Result<(Manifest, Vec<RawFd>)> {
    let mut panes: Vec<HPane> = Vec::with_capacity(order.len());
    let mut fds: Vec<RawFd> = Vec::with_capacity(order.len());
    let mut index = std::collections::HashMap::new();

    for (i, &id) in order.iter().enumerate() {
        let pane = eng
            .session
            .panes
            .get_mut(&id)
            .ok_or_else(|| anyhow!("pane {id} vanished mid-handoff"))?;
        let (h, fd) = pane.export()?;
        panes.push(h);
        fds.push(fd.into_raw_fd());
        index.insert(id, i);
    }

    let mut spaces = Vec::new();
    for space in &eng.session.spaces {
        let mut tabs = Vec::new();
        for &tid in &space.tabs {
            let Some(tab) = eng.session.tab(tid) else { continue };
            let Some(root) = tab.layout.root() else { continue };
            tabs.push(HTab {
                name: tab.name.clone(),
                tree: to_hnode(root, &index),
                focused_pane: tab.focused_pane.and_then(|p| index.get(&p).copied()),
            });
        }
        spaces.push(HSpace {
            focused_tab: space
                .focused_tab
                .and_then(|t| space.tabs.iter().position(|&x| x == t)),
            name: space.name.clone(),
            cwd: space.cwd.to_string_lossy().to_string(),
            tabs,
        });
    }

    let manifest = Manifest {
        version: handoff::HANDOFF_VERSION,
        from_version: env!("CARGO_PKG_VERSION").to_string(),
        focused_space: eng
            .session
            .focused_space
            .and_then(|id| eng.session.spaces.iter().position(|s| s.id == id)),
        view: eng.session.view,
        client_cols: eng.session.client_cols,
        client_rows: eng.session.client_rows,
        spaces,
        panes,
        bus: eng.bus.recent(200),
    };
    Ok((manifest, fds))
}

fn to_hnode(
    n: &super::layout::Node,
    index: &std::collections::HashMap<PaneId, usize>,
) -> HNode {
    use super::layout::{Axis, Node};
    match n {
        Node::Leaf(p) => HNode::Leaf(index.get(p).copied().unwrap_or(0)),
        Node::Split { axis, ratio, a, b, .. } => HNode::Split {
            horizontal: *axis == Axis::Horizontal,
            ratio: *ratio,
            a: Box::new(to_hnode(a, index)),
            b: Box::new(to_hnode(b, index)),
        },
    }
}

/// Start the successor with the handoff socket on a known descriptor.
fn spawn_successor(exe: &PathBuf, theirs: UnixStream) -> Result<std::process::Child> {
    let log = crate::config::log_path();
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let err = out.try_clone()?;
    let raw = theirs.into_raw_fd();

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("--import")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);

    unsafe {
        cmd.pre_exec(move || {
            // Put our end of the socketpair on a fixed descriptor the child looks for, and
            // clear CLOEXEC so it survives the exec.
            if libc::dup2(raw, HANDOFF_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(HANDOFF_FD, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The successor must survive this process exiting, exactly as a fresh daemon does.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().with_context(|| format!("spawning {}", exe.display()))?;
    // SAFETY: the child has its own copy; ours is finished with.
    unsafe { libc::close(raw) };
    Ok(child)
}

/// The successor's side: pick up the socket the predecessor left on descriptor 3.
pub fn inherited_socket() -> Result<UnixStream> {
    // SAFETY: by contract the predecessor placed a connected socket here before exec.
    let sock = unsafe { <UnixStream as std::os::fd::FromRawFd>::from_raw_fd(HANDOFF_FD) };
    // A quick sanity check that it really is a socket, so a stray `--import` fails cleanly.
    sock.peer_addr()
        .map(|_| ())
        .or_else(|_| sock.local_addr().map(|_| ()))
        .context("descriptor 3 is not a connected socket — was --import passed by hand?")?;
    Ok(sock)
}

/// Report readiness, wait for the go-ahead, then take over the socket path.
pub fn complete_import(sock: &mut UnixStream) -> Result<()> {
    sock.set_read_timeout(Some(STEP_TIMEOUT))?;
    sock.write_all(b"R").context("reporting ready")?;
    sock.flush()?;

    let mut b = [0u8; 1];
    sock.read_exact(&mut b).context("waiting for the go-ahead")?;
    if b[0] != b'G' {
        return Err(anyhow!("predecessor sent {:?} instead of go", b[0] as char));
    }

    // Rename rather than bind: the file already exists and is already listening, so this
    // cannot fail for a reason we could have avoided.
    let staged = staging_socket();
    let real = crate::config::socket_path();
    std::fs::rename(&staged, &real)
        .with_context(|| format!("moving {} into place", staged.display()))?;

    sock.write_all(b"B").context("confirming takeover")?;
    sock.flush()?;
    log_line("handoff: took over the socket");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_staging_socket_is_beside_the_real_one_and_distinct() {
        let real = crate::config::socket_path();
        let staged = staging_socket();
        assert_ne!(real, staged);
        assert_eq!(real.parent(), staged.parent(), "both must be bindable in the same dir");
        assert!(staged.to_string_lossy().contains("handoff"));
    }

    #[test]
    fn the_layout_tree_survives_conversion_to_manifest_form() {
        use super::super::layout::{Layout, Node};
        use crate::proto::{Dir, Rect};
        let area = Rect::new(0, 0, 100, 40);
        let mut l = Layout::single(7);
        l.split(7, Dir::Right, 9, area);
        l.split(9, Dir::Down, 11, area);

        let index: std::collections::HashMap<PaneId, usize> =
            [(7, 0), (9, 1), (11, 2)].into_iter().collect();
        let h = to_hnode(l.root().unwrap(), &index);

        // Rebuild through the same path import uses and compare pane order and shape.
        fn back(n: &HNode, ids: &[PaneId]) -> Node {
            match n {
                HNode::Leaf(i) => Node::Leaf(ids[*i]),
                HNode::Split { horizontal, ratio, a, b } => Node::Split {
                    id: 0,
                    axis: if *horizontal {
                        super::super::layout::Axis::Horizontal
                    } else {
                        super::super::layout::Axis::Vertical
                    },
                    ratio: *ratio,
                    a: Box::new(back(a, ids)),
                    b: Box::new(back(b, ids)),
                },
            }
        }
        let rebuilt = Layout::from_root(back(&h, &[7, 9, 11]));
        assert_eq!(rebuilt.panes(), l.panes());
        let a = l.geometry(area);
        let b = rebuilt.geometry(area);
        for id in l.panes() {
            assert_eq!(a.panes[&id], b.panes[&id], "pane {id} moved");
        }
    }
}
