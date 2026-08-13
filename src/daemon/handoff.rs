//! Live handoff: replacing the daemon binary without disturbing the processes it owns.
//!
//! The trick is that panes are attached to the *slave* side of their PTYs while the daemon
//! holds the master. Transfer the master descriptors to a successor process and the children
//! never learn anything changed — no signals, no restarts, no lost conversations.
//!
//! Descriptors move over a Unix socket using `SCM_RIGHTS`. Everything else — layout, names,
//! agent state, and the visible screen of each pane — is ordinary serialisable data.
//!
//! The invariant that makes it safe: **exactly one process may read a given PTY at a time.**
//! The outgoing daemon pauses its readers and waits for them to acknowledge before any
//! descriptor is sent, and the incoming daemon does not start reading until it has
//! everything. Two readers on one master would tear the output stream in half.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::proto::{AgentState, Message, Row, ViewState};

/// Bumped when the manifest shape changes *incompatibly*. A successor that disagrees refuses
/// the handoff, and the outgoing daemon simply carries on.
///
/// Adding a `#[serde(default)]` field is not such a change, and deliberately does not bump
/// this — the manifest is JSON, so a missing key genuinely defaults, unlike `PROTOCOL_VERSION`
/// where postcard's positional encoding leaves no such room. Bumping would refuse every
/// handoff *out of* an already-running older daemon, which is the exact operation this version
/// exists to protect: you would have to `horde stop` and lose every live agent conversation
/// just to get onto the new build. The cost of a defaulted field is that a downgrade drops it,
/// and a downgrade drops it regardless.
pub const HANDOFF_VERSION: u32 = 1;

/// Descriptors per `sendmsg`. Well under the kernel's per-message limit, so a session with
/// many panes just takes several messages.
const FDS_PER_MSG: usize = 32;

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Version of the daemon handing over, for the log.
    pub from_version: String,
    pub spaces: Vec<HSpace>,
    pub focused_space: Option<usize>,
    pub view: ViewState,
    pub client_cols: u16,
    pub client_rows: u16,
    /// Flat list of panes, in the same order as the transferred descriptors.
    pub panes: Vec<HPane>,
    /// Recent bus traffic, so the drawer is not empty after an upgrade.
    pub bus: Vec<Message>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HSpace {
    pub name: String,
    pub cwd: String,
    pub tabs: Vec<HTab>,
    pub focused_tab: Option<usize>,
    /// Project accent slot. `None` from a daemon that predates accents, which the successor
    /// answers by picking one — inheriting slot 0 for every space would look like a bug
    /// rather than like the absence it is.
    #[serde(default)]
    pub accent: Option<u8>,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HTab {
    pub name: String,
    pub tree: HNode,
    /// Index into `Manifest::panes`.
    pub focused_pane: Option<usize>,
}

/// The layout tree, with panes referenced by their index in `Manifest::panes`.
#[derive(Debug, Serialize, Deserialize)]
pub enum HNode {
    Leaf(usize),
    Split { horizontal: bool, ratio: f32, a: Box<HNode>, b: Box<HNode> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HPane {
    pub pid: i32,
    pub cmd: String,
    pub cwd: String,
    pub name: Option<String>,
    pub osc_title: String,
    pub cols: u16,
    pub rows: u16,
    pub agent: Option<HAgent>,
    /// Carried across the swap, or `horde upgrade` would launder a machine-started agent into
    /// one horde thinks you started — quietly freeing a slot under the unattended cap.
    #[serde(default)]
    pub spawned_by: Option<u64>,
    /// Carried across the swap for the same reason `spawned_by` is: metadata left out of the
    /// manifest is not degraded, it is gone — on every upgrade, silently, and you would not
    /// find out until you next looked for it.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    /// The pane's visible grid, replayed into the successor's emulator so the screen does
    /// not go blank. Programs that own the alternate screen (`nvim`, `htop`) come back
    /// looking approximate until they next redraw — their processes are untouched either way.
    pub screen: Vec<Row>,
    /// Bytes already read off the PTY but not yet fed to the emulator. Handing these over
    /// rather than dropping them is the difference between a seamless swap and a lost line.
    pub pending: Vec<u8>,
    /// Where the cursor was, so a shell's prompt does not come back in the wrong place.
    pub cursor_x: u16,
    pub cursor_y: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HAgent {
    pub kind: String,
    pub name: String,
    pub state: AgentState,
    pub authority: String,
    pub reason: String,
    pub seen: bool,
    pub session_id: Option<String>,
    pub queued: Vec<Message>,
}

// ---------------------------------------------------------------------------
// Descriptor passing
// ---------------------------------------------------------------------------

/// Send the manifest, then every descriptor.
pub fn send(sock: &mut UnixStream, manifest: &Manifest, fds: &[RawFd]) -> Result<()> {
    let body = serde_json::to_vec(manifest)?;
    let len = u32::try_from(body.len()).map_err(|_| anyhow!("manifest too large"))?;
    sock.write_all(&len.to_le_bytes())?;
    sock.write_all(&body)?;
    sock.flush()?;

    for chunk in fds.chunks(FDS_PER_MSG) {
        send_fds(sock.as_raw_fd(), chunk)?;
    }
    Ok(())
}

/// Receive a manifest and its descriptors, in the order they were sent.
pub fn recv(sock: &mut UnixStream) -> Result<(Manifest, Vec<OwnedFd>)> {
    let mut len = [0u8; 4];
    sock.read_exact(&mut len).context("reading manifest length")?;
    let len = u32::from_le_bytes(len) as usize;
    if len > 64 * 1024 * 1024 {
        return Err(anyhow!("manifest of {len} bytes is implausible"));
    }
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).context("reading manifest")?;
    let manifest: Manifest = serde_json::from_slice(&body).context("parsing manifest")?;

    if manifest.version != HANDOFF_VERSION {
        return Err(anyhow!(
            "handoff manifest is version {} but this binary speaks {HANDOFF_VERSION}",
            manifest.version
        ));
    }

    let want = manifest.panes.len();
    let mut fds = Vec::with_capacity(want);
    while fds.len() < want {
        let batch = recv_fds(sock.as_raw_fd(), FDS_PER_MSG)?;
        if batch.is_empty() {
            return Err(anyhow!(
                "handoff ended after {} of {want} descriptors",
                fds.len()
            ));
        }
        fds.extend(batch);
    }
    fds.truncate(want);
    Ok((manifest, fds))
}

/// One `sendmsg` carrying `fds` as ancillary data.
///
/// `SCM_RIGHTS` requires at least one byte of ordinary payload, so a single marker byte
/// carries the count for the receiver to sanity-check against.
fn send_fds(sock: RawFd, fds: &[RawFd]) -> Result<()> {
    if fds.is_empty() {
        return Ok(());
    }
    let n = fds.len();
    let payload = [n as u8];
    let space = unsafe { libc::CMSG_SPACE((std::mem::size_of::<RawFd>() * n) as u32) } as usize;
    let mut cmsg_buf = vec![0u8; space];

    // SAFETY: msghdr is zeroed then filled with pointers to live locals that outlive the
    // sendmsg call; the control buffer is sized by CMSG_SPACE for exactly `n` descriptors.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(anyhow!("no room for control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN((std::mem::size_of::<RawFd>() * n) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg) as *mut RawFd, n);

        let sent = libc::sendmsg(sock, &msg, 0);
        if sent < 0 {
            return Err(std::io::Error::last_os_error()).context("sendmsg with SCM_RIGHTS");
        }
    }
    Ok(())
}

/// One `recvmsg`, returning whatever descriptors arrived with it.
fn recv_fds(sock: RawFd, max: usize) -> Result<Vec<OwnedFd>> {
    let mut payload = [0u8; 1];
    let space = unsafe { libc::CMSG_SPACE((std::mem::size_of::<RawFd>() * max) as u32) } as usize;
    let mut cmsg_buf = vec![0u8; space];

    // SAFETY: as in send_fds — everything the msghdr points at is a live local, and the
    // control buffer is large enough for `max` descriptors.
    let (received, fds) = unsafe {
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;

        let got = libc::recvmsg(sock, &mut msg, 0);
        if got < 0 {
            return Err(std::io::Error::last_os_error()).context("recvmsg for SCM_RIGHTS");
        }
        if got == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                // The payload length tells us how many descriptors actually came through,
                // which can be fewer than we made room for.
                let data_len = (*cmsg).cmsg_len as usize
                    - (libc::CMSG_DATA(cmsg) as usize - cmsg as usize);
                let count = data_len / std::mem::size_of::<RawFd>();
                let base = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..count {
                    out.push(OwnedFd::from_raw_fd(*base.add(i)));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        (got, out)
    };
    let _ = received;
    Ok(fds)
}

/// Rebuild a byte stream that redraws `rows`, for replay into a fresh emulator.
///
/// This is what keeps the screen from going blank across a handoff. It reconstructs the
/// visible grid rather than replaying history, which is enough for shells and agent TUIs;
/// alternate-screen programs redraw themselves on their own schedule.
pub fn screen_to_ansi(rows: &[Row], cursor_x: u16, cursor_y: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows.len() * 80);
    // Reset attributes, clear, and home the cursor before painting.
    out.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
    for (y, row) in rows.iter().enumerate() {
        out.extend_from_slice(format!("\x1b[{};1H", y + 1).as_bytes());
        for run in &row.runs {
            out.extend_from_slice(
                format!(
                    "\x1b[0m\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m",
                    run.fg.r, run.fg.g, run.fg.b, run.bg.r, run.bg.g, run.bg.b
                )
                .as_bytes(),
            );
            for (bit, code) in [
                (crate::proto::attrs::BOLD, "1"),
                (crate::proto::attrs::DIM, "2"),
                (crate::proto::attrs::ITALIC, "3"),
                (crate::proto::attrs::UNDERLINE, "4"),
                (crate::proto::attrs::STRIKEOUT, "9"),
            ] {
                if run.attrs & bit != 0 {
                    out.extend_from_slice(format!("\x1b[{code}m").as_bytes());
                }
            }
            out.extend_from_slice(run.text.as_bytes());
        }
    }
    out.extend_from_slice(b"\x1b[0m");
    out.extend_from_slice(format!("\x1b[{};{}H", cursor_y + 1, cursor_x + 1).as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Rgb, Run};

    fn manifest(panes: usize) -> Manifest {
        Manifest {
            version: HANDOFF_VERSION,
            from_version: "test".into(),
            spaces: vec![],
            focused_space: None,
            view: ViewState::default(),
            client_cols: 120,
            client_rows: 40,
            panes: (0..panes)
                .map(|i| HPane {
                    pid: 1000 + i as i32,
                    cmd: "zsh".into(),
                    cwd: "/tmp".into(),
                    name: None,
                    osc_title: String::new(),
                    cols: 80,
                    rows: 24,
                    agent: None,
                    spawned_by: None,
                    role: None,
                    pinned: false,
                    screen: vec![],
                    pending: vec![],
                    cursor_x: 0,
                    cursor_y: 0,
                })
                .collect(),
            bus: vec![],
        }
    }

    /// The core of the whole feature: descriptors survive the trip and still refer to the
    /// same open file.
    #[test]
    fn descriptors_survive_the_socket() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        // Two pipes stand in for pty masters. Writing through a transferred descriptor must
        // come out of the original's other end.
        let mut fds = Vec::new();
        let mut readers = Vec::new();
        for _ in 0..3 {
            let mut p = [0 as RawFd; 2];
            // SAFETY: pipe fills two descriptors we then own.
            assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
            readers.push(p[0]);
            fds.push(p[1]);
        }

        let m = manifest(3);
        let sender = std::thread::spawn(move || {
            send(&mut a, &m, &fds).unwrap();
        });
        let (got, received) = recv(&mut b).unwrap();
        sender.join().unwrap();

        assert_eq!(got.panes.len(), 3);
        assert_eq!(received.len(), 3, "every descriptor should arrive");

        for (i, fd) in received.iter().enumerate() {
            let msg = format!("through-{i}");
            // SAFETY: `fd` is a live descriptor we received and own.
            let written = unsafe {
                libc::write(fd.as_raw_fd(), msg.as_ptr() as *const libc::c_void, msg.len())
            };
            assert_eq!(written, msg.len() as isize);

            let mut buf = [0u8; 64];
            // SAFETY: reading from the original pipe's read end.
            let n = unsafe {
                libc::read(readers[i], buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            assert!(n > 0);
            assert_eq!(
                &buf[..n as usize],
                msg.as_bytes(),
                "a transferred descriptor must still refer to the same pipe"
            );
        }
    }

    #[test]
    fn more_descriptors_than_fit_in_one_message_still_all_arrive() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let count = FDS_PER_MSG * 2 + 5;
        let mut fds = Vec::new();
        for _ in 0..count {
            let mut p = [0 as RawFd; 2];
            assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
            fds.push(p[1]);
            // SAFETY: closing the read end we do not need.
            unsafe { libc::close(p[0]) };
        }
        let m = manifest(count);
        let sender = std::thread::spawn(move || send(&mut a, &m, &fds).unwrap());
        let (_, received) = recv(&mut b).unwrap();
        sender.join().unwrap();
        assert_eq!(received.len(), count, "chunked descriptors must all arrive");
    }

    #[test]
    fn a_version_mismatch_is_refused() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let mut m = manifest(0);
        m.version = HANDOFF_VERSION + 1;
        let sender = std::thread::spawn(move || {
            let _ = send(&mut a, &m, &[]);
        });
        let err = recv(&mut b).unwrap_err().to_string();
        sender.join().unwrap();
        assert!(err.contains("version"), "{err}");
    }

    #[test]
    fn a_truncated_handoff_is_an_error_not_a_hang() {
        let (a, mut b) = UnixStream::pair().unwrap();
        // Claim two panes but send no descriptors, then hang up.
        let m = manifest(2);
        let sender = std::thread::spawn(move || {
            let mut a = a;
            let body = serde_json::to_vec(&m).unwrap();
            a.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
            a.write_all(&body).unwrap();
            a.flush().unwrap();
            drop(a);
        });
        let err = recv(&mut b).unwrap_err().to_string();
        sender.join().unwrap();
        assert!(err.contains("descriptors") || err.contains("ended"), "{err}");
    }

    #[test]
    fn screen_replay_reproduces_the_grid_through_a_real_emulator() {
        // Feed the generated bytes into the same emulator the daemon uses, and confirm the
        // text and colours come back out.
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;

        struct Size;
        impl Dimensions for Size {
            fn total_lines(&self) -> usize {
                4
            }
            fn screen_lines(&self) -> usize {
                4
            }
            fn columns(&self) -> usize {
                20
            }
        }

        let rows = vec![
            Row {
                runs: vec![Run {
                    text: "hello".into(),
                    fg: Rgb::new(10, 20, 30),
                    bg: Rgb::new(1, 2, 3),
                    attrs: crate::proto::attrs::BOLD,
                }],
            },
            Row {
                runs: vec![Run {
                    text: "world".into(),
                    fg: Rgb::new(200, 100, 50),
                    bg: Rgb::new(0, 0, 0),
                    attrs: 0,
                }],
            },
        ];

        let bytes = screen_to_ansi(&rows, 3, 1);
        let mut term = Term::new(Config::default(), &Size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, &bytes);

        let text = |line: i32, len: usize| -> String {
            (0..len)
                .map(|c| term.grid()[Line(line)][Column(c)].c)
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(text(0, 20), "hello");
        assert_eq!(text(1, 20), "world");

        // Colour and attributes survive, not just the characters.
        let cell = &term.grid()[Line(0)][Column(0)];
        assert!(
            matches!(cell.fg, alacritty_terminal::vte::ansi::Color::Spec(c)
                if (c.r, c.g, c.b) == (10, 20, 30)),
            "foreground colour should be replayed: {:?}",
            cell.fg
        );
        assert!(cell.flags.contains(alacritty_terminal::term::cell::Flags::BOLD));

        // And the cursor lands where it was.
        assert_eq!(term.grid().cursor.point.line, Line(1));
        assert_eq!(term.grid().cursor.point.column, Column(3));
    }

    /// Metadata left out of the manifest is not degraded, it is *gone* — on every
    /// `horde upgrade`, silently. This is the guard for that.
    #[test]
    fn roles_and_accents_survive_the_manifest_round_trip() {
        let mut m = manifest(2);
        m.panes[0].role = Some("reviewer".into());
        m.panes[0].pinned = true;
        m.spaces.push(HSpace {
            name: "api".into(),
            cwd: "/tmp".into(),
            tabs: vec![],
            focused_tab: None,
            accent: Some(6),
            collapsed: true,
        });

        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.panes[0].role.as_deref(), Some("reviewer"));
        assert!(back.panes[0].pinned);
        assert_eq!(back.panes[1].role, None, "an unlabelled pane stays unlabelled");
        assert_eq!(back.spaces[0].accent, Some(6));
        assert!(back.spaces[0].collapsed);
    }

    /// The test that earns `HANDOFF_VERSION` staying at 1. A manifest from a daemon built
    /// before roles existed carries none of these keys, and must still be adopted — bumping
    /// the version instead would refuse every handoff *out of* an already-running older
    /// daemon, forcing a `horde stop` and the loss of every live agent conversation just to
    /// reach the new build.
    #[test]
    fn a_manifest_from_a_daemon_without_roles_is_still_adopted() {
        let doc = serde_json::json!({
            "version": 1,
            "from_version": "old",
            "spaces": [{ "name": "api", "cwd": "/tmp", "tabs": [], "focused_tab": null }],
            "focused_space": null,
            "view": { "sidebar_open": true, "bus_open": false,
                      "sidebar_width": 24, "bus_width": 30, "zoom": null },
            "client_cols": 120,
            "client_rows": 40,
            "panes": [{
                "pid": 1, "cmd": "zsh", "cwd": "/tmp", "name": null, "osc_title": "",
                "cols": 80, "rows": 24, "agent": null, "screen": [], "pending": [],
                "cursor_x": 0, "cursor_y": 0,
            }],
            "bus": [],
        });
        let m: Manifest = serde_json::from_value(doc).expect("an older manifest must parse");
        assert_eq!(m.version, HANDOFF_VERSION, "and not be refused");
        assert_eq!(m.panes[0].role, None);
        assert!(!m.panes[0].pinned);
        // `None` rather than `Some(0)`: the successor answers this by picking a slot, and it
        // can only tell "no accent" from "slot zero" if the absence is modelled.
        assert_eq!(m.spaces[0].accent, None);
    }
}
