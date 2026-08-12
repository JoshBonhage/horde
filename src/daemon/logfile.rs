//! Append-only logs that do not grow forever.
//!
//! horde keeps three jsonl records — routed messages, the task board, and the event journal —
//! plus a plain-text daemon log. All four were append-only with no bound, which is fine for a
//! long time and then quietly is not.
//!
//! Rotation here is not the usual "rename and start empty", because two of these logs are
//! *replayed* to rebuild live state: the task board reconstructs open tasks from its log, and
//! the bus recovers undelivered messages from its own. Starting empty would silently drop open
//! tasks and forget queued messages. So a rotation **carries the live set forward** into the
//! fresh file, and the old file becomes `<name>.1` as pure history. Replay then needs no
//! knowledge of rotation at all, which is what keeps the rest of the code simple.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Rotate once a log passes this. Roughly 25k bus messages, so months of ordinary use.
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Appends between size checks. A `stat` per line would be wasteful for a file that takes
/// weeks to fill, and being a few lines late to rotate costs nothing.
const CHECK_EVERY: u32 = 256;

pub struct AppendLog {
    path: PathBuf,
    max_bytes: u64,
    since_check: u32,
}

impl AppendLog {
    pub fn new(path: PathBuf) -> AppendLog {
        AppendLog { path, max_bytes: MAX_BYTES, since_check: CHECK_EVERY }
    }

    #[cfg(test)]
    pub fn with_max(path: PathBuf, max_bytes: u64) -> AppendLog {
        AppendLog { path, max_bytes, since_check: CHECK_EVERY }
    }

    /// Append one line. Errors are swallowed: every one of these logs is a convenience, and
    /// failing a message delivery because a log line could not be written would be worse than
    /// losing the line.
    pub fn append_line(&mut self, line: &str) {
        if let Some(p) = self.path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if let Ok(mut f) =
            std::fs::OpenOptions::new().create(true).append(true).open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
        self.since_check = self.since_check.saturating_add(1);
    }

    /// Whether the file has grown past its limit, checked only every [`CHECK_EVERY`] appends.
    pub fn rotation_due(&mut self) -> bool {
        if self.since_check < CHECK_EVERY {
            return false;
        }
        self.since_check = 0;
        std::fs::metadata(&self.path).map(|m| m.len() > self.max_bytes).unwrap_or(false)
    }

    /// Move the current file aside and start a new one holding `carry`.
    ///
    /// `carry` is whatever the owner still considers live — the in-memory ring, typically — so
    /// that replaying the fresh file reconstructs the same state as before.
    pub fn rotate(&mut self, carry: &[String]) {
        let archive = archive_path(&self.path);
        // A previous `.1` is replaced. Keeping one generation bounds disk use at about twice
        // the limit, which is the point; keeping several would just defer the same decision.
        let _ = std::fs::remove_file(&archive);
        // Nothing on disk yet is not a failure — there is simply no history to archive, and
        // the carry still needs writing.
        if self.path.exists() && std::fs::rename(&self.path, &archive).is_err() {
            // Could not archive it. Leaving the log alone and growing is strictly better than
            // deleting history to enforce a size limit.
            return;
        }
        let body: String = carry.iter().map(|l| format!("{l}\n")).collect();
        // Write via a temp file and rename, so a crash here cannot leave a half-written log
        // that replay would read as truncated state.
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, &body).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
        self.since_check = 0;
        super::log_line(&format!(
            "rotated {} ({} live records carried forward)",
            self.path.display(),
            carry.len()
        ));
    }
}

/// `bus.jsonl` -> `bus.jsonl.1`. Appended rather than replacing the extension, so the archive
/// is still recognisably the same kind of file.
fn archive_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".1");
    path.with_file_name(name)
}

/// Rotate a plain text log that nothing replays, such as the daemon log.
///
/// Separate from [`AppendLog`] because the daemon log is written from a free function with no
/// state to hang a counter on, and because there is nothing to carry forward.
pub fn rotate_plain(path: &Path, max_bytes: u64) {
    let too_big = std::fs::metadata(path).map(|m| m.len() > max_bytes).unwrap_or(false);
    if !too_big {
        return;
    }
    let archive = archive_path(path);
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::rename(path, &archive);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("horde-logfile-{name}"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(archive_path(&p));
        p
    }

    fn lines(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn the_archive_keeps_the_original_name_plus_a_generation() {
        assert_eq!(
            archive_path(Path::new("/x/bus.jsonl")),
            PathBuf::from("/x/bus.jsonl.1")
        );
    }

    #[test]
    fn size_is_only_checked_periodically() {
        let p = temp("periodic.jsonl");
        let mut log = AppendLog::with_max(p.clone(), 1);
        // The very first append can rotate — the counter starts primed, so a daemon that
        // restarts often still gets its logs trimmed.
        log.append_line("first");
        assert!(log.rotation_due());
        // Immediately after, it stops asking until the interval has passed again.
        log.append_line("second");
        assert!(!log.rotation_due(), "should not stat on every line");
    }

    #[test]
    fn rotating_carries_the_live_set_into_the_new_file() {
        let p = temp("carry.jsonl");
        let mut log = AppendLog::with_max(p.clone(), 1);
        for i in 0..10 {
            log.append_line(&format!("old-{i}"));
        }
        let carry: Vec<String> = vec!["live-a".into(), "live-b".into()];
        log.rotate(&carry);

        // The fresh log holds exactly what was still live, so replay rebuilds the same state.
        assert_eq!(lines(&p), vec!["live-a", "live-b"]);
        // And the history is still there, in one generation.
        let old = lines(&archive_path(&p));
        assert_eq!(old.len(), 10);
        assert_eq!(old[0], "old-0");
    }

    #[test]
    fn a_second_rotation_replaces_the_previous_archive() {
        let p = temp("twice.jsonl");
        let mut log = AppendLog::with_max(p.clone(), 1);
        log.append_line("gen-one");
        log.rotate(&[]);
        log.append_line("gen-two");
        log.rotate(&[]);
        assert_eq!(lines(&archive_path(&p)), vec!["gen-two"], "only one generation is kept");
    }

    #[test]
    fn rotating_an_empty_log_is_harmless() {
        let p = temp("empty.jsonl");
        let mut log = AppendLog::with_max(p.clone(), 1);
        log.rotate(&["kept".into()]);
        assert_eq!(lines(&p), vec!["kept"]);
    }

    #[test]
    fn a_plain_log_rotates_without_carrying_anything() {
        let p = temp("plain.log");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        rotate_plain(&p, 1);
        assert!(!p.exists() || lines(&p).is_empty(), "the live file should start over");
        assert_eq!(lines(&archive_path(&p)), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_plain_log_under_the_limit_is_left_alone() {
        let p = temp("small.log");
        std::fs::write(&p, "a\n").unwrap();
        rotate_plain(&p, 1024);
        assert_eq!(lines(&p), vec!["a"]);
        assert!(!archive_path(&p).exists());
    }
}
