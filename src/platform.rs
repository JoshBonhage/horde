//! The handful of things horde does that only work on one operating system.
//!
//! There are exactly three: putting text on the clipboard, posting a notification, and asking
//! what a process is called. Each had a single macOS implementation inlined at its call site —
//! `pbcopy`, `osascript`, `ps` — and each of those is either missing or wrong everywhere else.
//! Collected here so that "what does horde assume about the machine" is one file rather than a
//! grep, and so the next platform is a match arm instead of an archaeology exercise.
//!
//! The rule every function follows: **resolve a strategy, then degrade**. Nothing here returns
//! an error because the host is unusual. It returns the best thing this host can actually do,
//! and where that is nothing, it says so once rather than failing on every attempt.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Host questions
// ---------------------------------------------------------------------------

/// Whether this Linux is Linux-under-Windows.
///
/// Cached: the answer cannot change while the process lives, and the clipboard path asks on
/// every copy.
pub fn is_wsl() -> bool {
    static WSL: OnceLock<bool> = OnceLock::new();
    *WSL.get_or_init(|| {
        let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        detect_wsl(&osrelease, std::env::var("WSL_DISTRO_NAME").ok().as_deref())
    })
}

/// Split from `is_wsl` so the two signals can be tested without a Windows machine.
///
/// `WSL_DISTRO_NAME` is set by `wsl.exe` in any session a human started, and the kernel's own
/// release string carries the vendor tag in both WSL1 (`Microsoft`) and WSL2 (`microsoft`).
/// Either alone is enough: the environment variable is absent from a daemon started with a thin
/// environment, and a custom-compiled WSL2 kernel can drop the tag.
fn detect_wsl(osrelease: &str, distro_env: Option<&str>) -> bool {
    if distro_env.is_some_and(|d| !d.is_empty()) {
        return true;
    }
    osrelease.to_ascii_lowercase().contains("microsoft")
}

/// Whether `prog` is somewhere on `$PATH`.
///
/// Spawning-and-catching-ENOENT would answer the same question, but the answer is needed
/// *before* deciding a strategy, and some of the candidates here are Windows executables whose
/// startup cost is not something to pay speculatively on every notification.
pub fn which(prog: &str) -> bool {
    // An explicit path is a claim about a specific file, not a name to search for.
    if prog.contains('/') {
        return Path::new(prog).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        let full: PathBuf = dir.join(prog);
        full.is_file()
    })
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// A command that will post a desktop notification, or `None` if this host has nowhere to post
/// one.
///
/// Returned rather than run, because the two callers need different things from it: the client
/// fires and forgets, and the daemon runs it on a thread with a busy flag so a wedged notifier
/// cannot accumulate one stuck child per alert window.
///
/// **There is deliberately no WSL case.** The candidates are all bad — `notify-send` needs a
/// notification daemon that a headless distro does not run, BurntToast needs a PowerShell module
/// installed on the Windows side, and `wsl-notify-send.exe` is a third-party binary. Taking a
/// dependency on any of them to make one config value work would cost more than the value is
/// worth. WSL falls through to `None`, and `notify_command` — which already reaches Pushover,
/// Telegram, ntfy or a PowerShell one-liner — is the documented sink.
pub fn system_notify(summary: &str) -> Option<Command> {
    if cfg!(target_os = "macos") {
        // Interpolated into AppleScript source, so quotes and backslashes have to survive the
        // trip as data rather than terminating the string literal.
        let escaped = summary.replace('\\', "\\\\").replace('"', "\\\"");
        let mut c = Command::new("osascript");
        c.args(["-e", &format!("display notification \"{escaped}\" with title \"horde\"")]);
        return Some(c);
    }
    if which("notify-send") {
        let mut c = Command::new("notify-send");
        // `--` because a summary beginning with a dash is otherwise parsed as a flag, and the
        // summary is a digest headline horde composed, not a constant.
        c.args(["--app-name=horde", "--", "horde", summary]);
        return Some(c);
    }
    None
}

/// Why `notify = "system"` will not do anything here, phrased as advice.
///
/// Worth a sentence rather than silence: the setting is on, the user is expecting a ping, and
/// the failure is otherwise indistinguishable from horde deciding there was nothing to say.
pub fn no_notifier_hint() -> &'static str {
    if is_wsl() {
        "notify = \"system\" has no sink under WSL — set notify_command to a powershell.exe one-liner"
    } else {
        "notify = \"system\" needs notify-send on this platform — install it, or set notify_command"
    }
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// The local program that owns the system clipboard, if there is one.
///
/// Ordered by how specific the evidence is. A Wayland or X11 socket in the environment is proof
/// that the display server it belongs to is the one to talk to; being on WSL is proof that
/// Windows owns the clipboard. Guessing past that point does more harm than good, which is why
/// there is no bare `xclip` case: on a headless box `xclip` is installed as often as not, and it
/// hangs rather than failing when there is no display to reach.
///
/// Every one of these can still be missing, so the caller must be prepared to fall through to
/// [`osc52`], which needs nothing installed at all.
pub fn clipboard_command() -> Option<Command> {
    if cfg!(target_os = "macos") && which("pbcopy") {
        return Some(Command::new("pbcopy"));
    }
    // Before the display-server cases: WSLg sets WAYLAND_DISPLAY, and copying into a Wayland
    // clipboard nothing is looking at is not what the user meant by "copy".
    if is_wsl() && which("clip.exe") {
        return Some(Command::new("clip.exe"));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-copy") {
        return Some(Command::new("wl-copy"));
    }
    if std::env::var_os("DISPLAY").is_some() {
        if which("xclip") {
            let mut c = Command::new("xclip");
            c.args(["-selection", "clipboard"]);
            return Some(c);
        }
        if which("xsel") {
            let mut c = Command::new("xsel");
            c.args(["--clipboard", "--input"]);
            return Some(c);
        }
    }
    None
}

/// Above this many bytes, do not attempt the escape-sequence route.
///
/// OSC 52 travels as one unbroken sequence through every layer between horde and the terminal,
/// and each has its own ceiling — xterm's parser gives up well before a megabyte, and a
/// truncated sequence does not fail cleanly, it leaves the tail of the payload printed across
/// the screen as text. 64 KiB is far more than a selection and far less than anything known to
/// break, and past it saying "too large" beats corrupting the display.
pub const OSC52_LIMIT: usize = 64 * 1024;

/// The escape sequence that asks the terminal to put `text` on the clipboard.
///
/// This is the route that works when nothing is installed and the machine is not even the one
/// you are sitting at: the sequence travels back over SSH, out of WSL, and through Windows
/// Terminal — which has supported the copy direction since it was added to conhost — to land in
/// the clipboard of whatever you are actually typing on. Nothing local is involved, which is
/// exactly why it is the fallback rather than the first choice: there is no way to find out
/// whether it worked.
///
/// `c` selects the clipboard proper rather than the X11 primary selection. Terminated with
/// `ESC \` rather than `BEL`: both are accepted, and the string terminator is the one the
/// specification actually defines.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", base64(text.as_bytes()))
}

/// Standard base64, padded.
///
/// Written out rather than pulled in: one alphabet and one loop against a dependency in a binary
/// that currently has no encoding crates at all.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        // Pad the group out to three bytes, remembering how many were real.
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let sextet = |shift: u32| ALPHABET[((n >> shift) & 0x3f) as usize] as char;
        out.push(sextet(18));
        out.push(sextet(12));
        // A group of one encodes to two characters plus two pads; a group of two, three plus one.
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

/// What horde asks for, when the hard limit allows it.
///
/// Each pane costs three descriptors — the pty master, a dup for the writer, a dup for the
/// reader thread — plus one per attached client, the socket, and the append logs. A large
/// session is therefore in the low hundreds, and `horde upgrade` briefly *doubles* the pane
/// share because the handoff dups every master before sending any of them. Asking for far more
/// than needed costs nothing; a descriptor limit is a ceiling, not an allocation.
const WANT_FILES: u64 = 16_384;

/// Raise this process's open-file limit, returning `(before, after)`.
///
/// macOS launches processes with a soft limit of **256** — `launchctl limit maxfiles` — while
/// the hard limit is effectively unbounded. A daemon that inherits 256 runs a normal session
/// fine and then fails at exactly the wrong moment: `horde upgrade` needs a dup per pane all at
/// once, so a fleet that has been running for hours dies with `Too many open files` while handing
/// over, which is the one operation that is supposed to be safe. Every multiplexer raises this
/// for the same reason.
///
/// Best effort. The value is clamped to the hard limit, and macOS additionally refuses anything
/// above `kern.maxfilesperproc` with `EINVAL` — so a refused request steps down rather than
/// giving up, and a host that will not raise it at all simply keeps what it had.
pub fn raise_file_limit() -> (u64, u64) {
    // SAFETY: `getrlimit` writes only the `rlimit` handed to it.
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return (0, 0);
    }
    let before = lim.rlim_cur as u64;
    let hard = lim.rlim_max as u64;
    // `RLIM_INFINITY` as a target is what macOS rejects, so an unbounded hard limit means
    // "ask for what we want" rather than "ask for everything".
    let ceiling = if hard == libc::RLIM_INFINITY { WANT_FILES } else { hard.min(WANT_FILES) };
    if ceiling <= before {
        return (before, before);
    }

    // Step down on refusal: the kernel's real per-process cap is not visible from here, so the
    // only way to find it is to ask for less until one is accepted.
    let mut target = ceiling;
    while target > before {
        lim.rlim_cur = target as libc::rlim_t;
        // SAFETY: raising our own soft limit, never above the hard limit we just read.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } == 0 {
            return (before, target);
        }
        target /= 2;
    }
    (before, before)
}

/// The current soft open-file limit, for reporting.
pub fn file_limit() -> u64 {
    // SAFETY: `getrlimit` writes only the `rlimit` handed to it.
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return 0;
    }
    lim.rlim_cur as u64
}

// ---------------------------------------------------------------------------
// Filesystems
// ---------------------------------------------------------------------------

/// Filesystem types that mean "this is a Windows drive seen from Linux".
///
/// `drvfs` is WSL1's, `9p` is WSL2's, `virtiofs` is what newer WSL2 mounts `/mnt/c` with, and
/// `cifs` covers a network drive mapped in Windows and inherited through the same path. All four
/// are the same fact for horde's purposes: git is an order of magnitude slower on them, and they
/// are not a place to put anything the daemon needs to be fast or exotic about.
const WINDOWS_DRIVE_FS: &[&str] = &["drvfs", "9p", "virtiofs", "cifs"];

/// Whether `path` lives on a Windows drive rather than in the Linux filesystem.
///
/// Answered from the mount table rather than by looking for a `/mnt/` prefix, because that
/// prefix is configurable — `wsl.conf`'s `automount.root` moves `/mnt/c` to `/c` — and because
/// the filesystem type is the fact that actually matters. Always false off WSL, where these
/// same type names would mean something else entirely.
pub fn on_windows_drive(path: &Path) -> bool {
    if !is_wsl() {
        return false;
    }
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    fstype_from_mounts(&path.to_string_lossy(), &mounts)
        .is_some_and(|fs| WINDOWS_DRIVE_FS.contains(&fs.as_str()))
}

/// The filesystem type `path` sits on, given the contents of `/proc/mounts`.
///
/// Longest matching mount point wins, which is the whole subtlety: `/` matches everything, so a
/// plain "does the line's mount point prefix this path" search would answer `ext4` for every
/// path on the system. Split out from [`on_windows_drive`] so the matching can be tested against
/// a real mount table without needing the machine that produced it.
fn fstype_from_mounts(path: &str, mounts: &str) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for line in mounts.lines() {
        // `device mountpoint fstype options dump pass`
        let mut f = line.split_whitespace();
        // `continue`, not `?`: one short line must skip its own iteration rather than abandon
        // the scan, or a single oddity near the top of the table hides every mount below it.
        let (Some(_dev), Some(point), Some(fstype)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        // Mount points are escaped in /proc/mounts; spaces arrive as `\040`.
        let point = point.replace("\\040", " ");
        let covers = path == point
            || (path.starts_with(&point) && (point.ends_with('/') || path[point.len()..].starts_with('/')));
        if covers && best.is_none_or(|(len, _)| point.len() > len) {
            best = Some((point.len(), fstype));
        }
    }
    best.map(|(_, fs)| fs.to_string())
}

/// Why a Windows drive is the wrong place for whatever the caller was about to do there.
pub fn windows_drive_hint(what: &str) -> String {
    format!(
        "{what} is on a Windows drive. Git is many times slower across it — worst of all for a \
         `--worktree` fleet — and it cannot host the daemon's unix socket. Keep repositories \
         under your Linux home instead."
    )
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// What the process leading `pgid` is running, as a path or a bare name.
///
/// Three routes, most trustworthy first, because the same question has three different answers
/// depending on the kernel:
///
/// - `/proc/<pid>/exe` is the full path to the running image. Linux only, and unreadable for a
///   process belonging to another user, but when it answers it answers exactly.
/// - `/proc/<pid>/comm` is the same fact **truncated to 15 characters** — enough for `claude`
///   and `cursor-agent`, not enough for a manifest naming anything longer, which is why it is
///   second rather than first.
/// - `ps -o comm=` is the portable question, and the one macOS answers with a full path.
///
/// Deliberately not gated on `cfg(target_os)`: a `/proc` that is not there simply fails to open,
/// which is the same branch as a `/proc` that refused, and one code path is easier to trust than
/// two that are each only ever compiled on one machine.
pub fn process_name(pgid: i32) -> Option<String> {
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pgid}/exe")) {
        let s = exe.to_string_lossy();
        // A deleted binary still resolves, with a marker glued on the end. Better to fall
        // through to `comm` than to hand back a name no manifest can match.
        if !s.is_empty() && !s.ends_with(" (deleted)") {
            return Some(s.into_owned());
        }
    }
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pgid}/comm")) {
        let s = comm.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    let out = Command::new("ps").args(["-o", "comm=", "-p", &pgid.to_string()]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_is_recognised_from_either_signal() {
        // WSL2, as the kernel reports itself.
        assert!(detect_wsl("5.15.167.4-microsoft-standard-WSL2", None));
        // WSL1 capitalises it.
        assert!(detect_wsl("4.4.0-19041-Microsoft", None));
        // A custom kernel can lose the tag; the environment still says where we are.
        assert!(detect_wsl("6.6.0-mykernel", Some("Ubuntu-24.04")));
        // Real Linux, and the empty-string case a missing variable produces.
        assert!(!detect_wsl("6.8.0-117-generic", None));
        assert!(!detect_wsl("6.8.0-117-generic", Some("")));
        // A Mac, where the file does not exist at all and the read yields nothing.
        assert!(!detect_wsl("", None));
    }

    #[test]
    fn which_finds_a_program_that_must_exist_and_not_one_that_cannot() {
        assert!(which("sh"), "every POSIX host has sh on PATH");
        assert!(!which("horde-nonexistent-program-xyzzy"));
        // An explicit path is checked as a file rather than searched for.
        assert!(which("/bin/sh"));
        assert!(!which("/bin/horde-nonexistent-xyzzy"));
    }

    /// Against RFC 4648's own vectors, so the padding cases are not merely self-consistent.
    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_survives_bytes_that_are_not_ascii() {
        // Both characters that need the high bits, and the 62/63 slots of the alphabet.
        assert_eq!(base64("é".as_bytes()), "w6k=");
        assert_eq!(base64(&[0xfb, 0xef, 0xbe]), "++++");
        assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
    }

    #[test]
    fn osc52_wraps_the_payload_in_the_sequence_a_terminal_expects() {
        assert_eq!(osc52("foo"), "\x1b]52;c;Zm9v\x1b\\");
    }

    /// A payload at the limit still has to come out as one well-formed sequence — the failure
    /// this guards against is visual, since a sequence cut off mid-payload does not error, it
    /// spills the rest onto the screen as text.
    #[test]
    fn a_payload_at_the_limit_is_still_one_well_formed_sequence() {
        let big = "x".repeat(OSC52_LIMIT);
        let seq = osc52(&big);
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with("\x1b\\"));
        // Base64 is four characters per three bytes, and the payload is the only variable part.
        assert_eq!(seq.len(), 7 + OSC52_LIMIT.div_ceil(3) * 4 + 2);
    }

    /// Not an assertion about which strategy wins — that depends on the host — but that asking
    /// is free of side effects and the answer is a command that could be spawned.
    #[test]
    fn resolving_a_clipboard_strategy_is_safe_to_ask_for() {
        if let Some(c) = clipboard_command() {
            assert!(!c.get_program().is_empty());
        }
    }

    #[test]
    fn a_notifier_is_either_a_runnable_command_or_an_explanation() {
        match system_notify("1 agent needs you") {
            Some(c) => assert!(!c.get_program().is_empty()),
            None => assert!(no_notifier_hint().contains("notify_command")),
        }
    }

    /// The limit must come out at least as high as it went in, and normally higher.
    ///
    /// Not asserted against a fixed number: CI, a container and a developer Mac all start from
    /// different soft limits, and the property that matters is "raising it never lowers it".
    #[test]
    fn raising_the_file_limit_never_lowers_it() {
        let (before, after) = raise_file_limit();
        assert!(after >= before, "raised {before} to {after}");
        assert_eq!(file_limit(), after, "the reported limit is the one now in force");

        // Idempotent: a second call finds the work already done and changes nothing.
        let (again_before, again_after) = raise_file_limit();
        assert_eq!(again_before, after);
        assert_eq!(again_after, after);
    }

    /// A real WSL2 mount table, trimmed to the lines that matter.
    const WSL_MOUNTS: &str = "\
/dev/sdc / ext4 rw,relatime,discard,errors=remount-ro 0 0
none /mnt/wsl tmpfs rw,relatime 0 0
drivers /usr/lib/wsl/drivers 9p ro,dirsync,noatime 0 0
C:\\134 /mnt/c 9p rw,noatime,dirsync,aname=drvfs 0 0
D:\\134 /mnt/d drvfs rw,noatime 0 0
snapfuse /snap/core/1 fuse.snapfuse ro,nodev,relatime 0 0";

    #[test]
    fn the_longest_matching_mount_point_wins() {
        // The trap: `/` prefixes every path, so a naive search answers ext4 for all of these.
        assert_eq!(fstype_from_mounts("/mnt/c/Users/josh/code", WSL_MOUNTS).unwrap(), "9p");
        assert_eq!(fstype_from_mounts("/mnt/d/repos", WSL_MOUNTS).unwrap(), "drvfs");
        assert_eq!(fstype_from_mounts("/home/josh/code", WSL_MOUNTS).unwrap(), "ext4");
        assert_eq!(fstype_from_mounts("/", WSL_MOUNTS).unwrap(), "ext4");
        // The mount point itself, with no trailing component.
        assert_eq!(fstype_from_mounts("/mnt/c", WSL_MOUNTS).unwrap(), "9p");
    }

    /// A mount point is a path component boundary, not a string prefix.
    #[test]
    fn a_similarly_named_directory_is_not_inside_a_mount() {
        // `/mnt/config` is not under `/mnt/c`, however much it starts with it.
        assert_eq!(fstype_from_mounts("/mnt/config", WSL_MOUNTS).unwrap(), "ext4");
        assert_eq!(fstype_from_mounts("/mnt/cheese/x", WSL_MOUNTS).unwrap(), "ext4");
    }

    #[test]
    fn every_flavour_of_windows_drive_is_recognised_and_a_linux_one_is_not() {
        for fs in ["drvfs", "9p", "virtiofs", "cifs"] {
            assert!(WINDOWS_DRIVE_FS.contains(&fs), "{fs} should count as a Windows drive");
        }
        for fs in ["ext4", "overlay", "btrfs", "tmpfs", "apfs"] {
            assert!(!WINDOWS_DRIVE_FS.contains(&fs), "{fs} is not a Windows drive");
        }
    }

    /// A mount table horde cannot read is not evidence of anything.
    #[test]
    fn an_absent_mount_table_answers_nothing_rather_than_guessing() {
        assert!(fstype_from_mounts("/home/josh", "").is_none());
        assert!(fstype_from_mounts("/home/josh", "garbage\nalso garbage").is_none());
    }

    /// Whatever route answers on this host, it must describe the test binary itself.
    #[test]
    fn a_process_can_name_itself() {
        let me = process_name(std::process::id() as i32).expect("a live process has a name");
        assert!(me.contains("horde"), "expected the test binary, got {me:?}");
    }
}
