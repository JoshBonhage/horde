//! Notes a project keeps for its agents: `<project>/.horde/memory/*.md`.
//!
//! The problem this solves is one horde is uniquely placed to see. An agent's context is
//! finite, and the standard escape — compact, and lose the detail — throws away exactly the
//! working knowledge that took the session to build. The usual workaround is to tell the
//! agent to write a note first, which works, and then the note is a file in a repository that
//! nobody remembers is there, least of all the *next* agent, who is a fresh process with no
//! memory of the conversation that produced it.
//!
//! So the note becomes a thing in the session rather than a thing in a directory. It is
//! listed in the sidebar under the project it belongs to, and handing it to an agent is one
//! gesture, because "an agent that needs this context" and "the note holding it" are now both
//! on screen at the same time.
//!
//! # Why the project, and not horde's own state directory
//!
//! Everything else horde persists — the board, the bus log, the journal — lives in
//! `~/.config/horde` and is scoped to a project by *name*. A memory does not, and the
//! difference is what it is for: this is knowledge about a codebase, so it belongs beside the
//! codebase, where it can be committed, reviewed, diffed and read by a person with no horde
//! running. `.horde/handoff-<name>.md` already established the convention and this is the same
//! idea, generalised from "the agent that is being replaced" to "any agent that needs it".
//!
//! # Why ids
//!
//! The sidebar cursor is named by identity so it survives the list being rebuilt every frame.
//! A name would do for that, except that `Focus` is `Copy` and carried through the client's
//! hit tables — so each path gets a small stable id instead, minted once and kept for the life
//! of the daemon. A note that is deleted and rewritten gets the same id, which is right: it is
//! the same note.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Result};

use crate::proto::MemoryId;

/// How often a project's memory directory is re-read.
///
/// Slower than a frame and faster than you can notice. The contents change when an agent
/// writes a note or you edit one, both of which are human-scale events; scanning per frame
/// would put a directory read in the render path of every attached client.
const REFRESH: Duration = Duration::from_secs(3);

/// Longest note a `save` will accept, and the cap on what `show` returns.
///
/// Generous — a memory is prose, and 256KB is a very long note — but not unbounded: the
/// point of the feature is to spend less context, and a file that cannot be read in one turn
/// is not serving that.
const MAX_BYTES: u64 = 256 * 1024;

/// Longest title kept. Past this the sidebar has truncated it anyway.
const MAX_TITLE: usize = 120;

/// One note, as the session sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: MemoryId,
    /// The file stem, which is how it is addressed: `horde memory show api-shape`.
    pub name: String,
    /// Its first heading, for a row that has room for more than a filename.
    pub title: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// The memory directory for a project.
pub fn dir_of(project: &Path) -> PathBuf {
    project.join(".horde").join("memory")
}

/// A note's file, given a project and a name.
///
/// Names are a single path component with no separators, because the alternative is a note
/// called `../../.ssh/id_rsa` and a `save` that writes it. Rejected rather than sanitised:
/// silently rewriting what someone asked for is how you end up with two notes that disagree
/// about which file they are.
pub fn path_of(project: &Path, name: &str) -> Result<PathBuf> {
    let clean = name.trim().trim_end_matches(".md");
    if clean.is_empty() {
        return Err(anyhow!("a memory needs a name"));
    }
    if clean.len() > 64 {
        return Err(anyhow!("name too long (max 64 characters)"));
    }
    if !clean.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(anyhow!(
            "name may only hold letters, digits, dashes, dots and underscores: {clean:?}"
        ));
    }
    // `..` passes the character test and is the whole attack.
    if clean.split('.').all(|part| part.is_empty()) {
        return Err(anyhow!("name must hold something other than dots"));
    }
    Ok(dir_of(project).join(format!("{clean}.md")))
}

/// The line a reader would call this note, taken from its own text.
///
/// A `# heading` first, because that is what a person writing markdown puts there. Failing
/// that the first line with anything on it, which covers a note that was dumped rather than
/// composed. Failing *that* the name, so a row is never blank.
fn title_of(body: &str, name: &str) -> String {
    let head = body
        .lines()
        .take(20)
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let text = head.trim_start_matches('#').trim();
    let text = if text.is_empty() { name } else { text };
    text.chars().take(MAX_TITLE).collect()
}

/// Every project's notes, re-read on a timer.
#[derive(Default)]
pub struct Store {
    /// Stable ids by path. Never reclaimed: a `u64` counter outlasts any session, and reusing
    /// an id would let a stale click land on a different note.
    ids: HashMap<PathBuf, MemoryId>,
    next: MemoryId,
    entries: HashMap<PathBuf, (Instant, Vec<Note>)>,
}

impl Store {
    fn id_for(&mut self, path: &Path) -> MemoryId {
        if let Some(id) = self.ids.get(path) {
            return *id;
        }
        self.next += 1;
        self.ids.insert(path.to_path_buf(), self.next);
        self.next
    }

    /// The notes in a project, refreshing the listing when stale.
    pub fn get(&mut self, project: &Path) -> &[Note] {
        let stale = match self.entries.get(project) {
            Some((at, _)) => at.elapsed() >= REFRESH,
            None => true,
        };
        if stale {
            let mut notes = scan(&dir_of(project));
            for n in &mut notes {
                n.id = self.id_for(&n.path);
            }
            self.entries.insert(project.to_path_buf(), (Instant::now(), notes));
        }
        self.entries.get(project).map(|(_, n)| n.as_slice()).unwrap_or_default()
    }

    /// The cached listing without refreshing it, for readers that cannot take `&mut` — which
    /// is every one downstream of a snapshot. Same contract as `repo::Cache::peek`.
    pub fn peek(&self, project: &Path) -> &[Note] {
        self.entries.get(project).map(|(_, n)| n.as_slice()).unwrap_or_default()
    }

    /// A note by id, across every project the store has looked at.
    pub fn find(&self, id: MemoryId) -> Option<&Note> {
        self.entries.values().flat_map(|(_, n)| n).find(|n| n.id == id)
    }

    /// Drop the cached listing for a project, so the next read is fresh.
    ///
    /// Called after a write rather than waiting out `REFRESH`: a note you just saved that
    /// does not appear for three seconds reads as a save that failed.
    pub fn invalidate(&mut self, project: &Path) {
        self.entries.remove(project);
    }

    /// Forget projects nothing points at any more.
    pub fn retain(&mut self, live: impl Fn(&Path) -> bool) {
        self.entries.retain(|k, _| live(k));
    }
}

/// Read one directory of notes, newest first.
///
/// Newest first because a memory list is a stack, not a filing cabinet: the note you want is
/// almost always the one most recently written, and alphabetical order would bury it under
/// whatever happens to start with an `a`.
fn scan(dir: &Path) -> Vec<Note> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        // Only the head of the file: a title is in the first few lines or it is not there,
        // and reading a 200KB note to render a sidebar row would be a directory scan that
        // costs megabytes.
        let head = read_head(&path, 4096);
        out.push(Note {
            id: 0,
            title: title_of(&head, &name),
            name,
            path,
            bytes: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The first `cap` bytes of a file, as lossy UTF-8.
fn read_head(path: &Path, cap: usize) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let mut buf = vec![0u8; cap];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Write a note, creating the directory if this is the project's first.
pub fn save(project: &Path, name: &str, body: &str) -> Result<PathBuf> {
    let path = path_of(project, name)?;
    if body.len() as u64 > MAX_BYTES {
        return Err(anyhow!("note is {} bytes, over the {MAX_BYTES} limit", body.len()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A trailing newline, because these are files people open in an editor and every other
    // tool in the chain assumes one.
    let body = if body.ends_with('\n') { body.to_string() } else { format!("{body}\n") };
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Read a note back.
pub fn read(project: &Path, name: &str) -> Result<String> {
    let path = path_of(project, name)?;
    std::fs::read_to_string(&path).map_err(|e| anyhow!("{}: {e}", path.display()))
}

/// Delete a note.
pub fn remove(project: &Path, name: &str) -> Result<PathBuf> {
    let path = path_of(project, name)?;
    std::fs::remove_file(&path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    Ok(path)
}

/// What horde types into an agent's pane when a note is handed to it.
///
/// A path and one line of why, never the contents. The whole reason a memory exists is that
/// somebody was running out of context, and pasting a note into the pane spends again exactly
/// what writing it saved. The agent has file tools; let it read the file, when it needs it,
/// as many times as it needs it.
///
/// The path is relative to the project when the agent is standing in it, because that is what
/// the agent will type back and what will end up quoted in its own notes. Absolute otherwise —
/// an agent in a worktree resolving `.horde/memory/x.md` would find its own tree's copy, or
/// nothing at all.
pub fn handover_text(note: &Note, project: &Path, agent_cwd: &Path) -> String {
    let shown = if agent_cwd == project {
        note.path
            .strip_prefix(project)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| note.path.to_string_lossy().into_owned())
    } else {
        note.path.to_string_lossy().into_owned()
    };
    format!("Read {shown} — saved context for this project: {}", note.title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horde-memory-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_note_is_saved_beside_the_project_and_read_back() {
        let p = tmp("roundtrip");
        let at = save(&p, "api-shape", "# API shape\n\nthe v2 handler splits").unwrap();
        assert_eq!(at, p.join(".horde/memory/api-shape.md"));
        assert!(read(&p, "api-shape").unwrap().contains("v2 handler"));
    }

    /// `.md` is the extension, not part of the name, so both spellings address one note
    /// rather than making two.
    #[test]
    fn naming_a_note_with_its_extension_addresses_the_same_file() {
        let p = tmp("ext");
        save(&p, "notes", "x").unwrap();
        assert_eq!(path_of(&p, "notes.md").unwrap(), path_of(&p, "notes").unwrap());
    }

    /// The whole reason names are validated rather than sanitised.
    #[test]
    fn a_name_can_never_escape_the_memory_directory() {
        let p = tmp("escape");
        for bad in ["../secret", "a/b", "..", "/etc/passwd", ""] {
            assert!(path_of(&p, bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_title_comes_from_the_heading_then_the_first_line_then_the_name() {
        assert_eq!(title_of("# API shape\nbody", "x"), "API shape");
        assert_eq!(title_of("\n\njust prose here\n", "x"), "just prose here");
        assert_eq!(title_of("   \n \n", "fallback"), "fallback");
    }

    /// A memory list is a stack: the note you want is the one most recently written.
    #[test]
    fn notes_list_newest_first() {
        let p = tmp("order");
        save(&p, "old", "# old").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        save(&p, "new", "# new").unwrap();
        let mut store = Store::default();
        let names: Vec<&str> = store.get(&p).iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["new", "old"]);
    }

    /// A note deleted and rewritten is the same note, and a click held over the gap must not
    /// land on a different one.
    #[test]
    fn an_id_is_stable_across_a_rescan() {
        let p = tmp("ids");
        save(&p, "a", "# a").unwrap();
        let mut store = Store::default();
        let first = store.get(&p)[0].id;
        store.invalidate(&p);
        save(&p, "a", "# a, revised").unwrap();
        assert_eq!(store.get(&p)[0].id, first);
    }

    #[test]
    fn a_project_with_no_memory_directory_has_no_notes() {
        let p = tmp("empty");
        let mut store = Store::default();
        assert!(store.get(&p).is_empty());
    }

    /// Anything that is not a markdown file is somebody else's, including the directory an
    /// editor leaves behind.
    #[test]
    fn only_markdown_files_are_notes() {
        let p = tmp("filter");
        save(&p, "real", "# real").unwrap();
        std::fs::write(dir_of(&p).join("notes.txt"), "no").unwrap();
        std::fs::create_dir_all(dir_of(&p).join("subdir.md")).unwrap();
        let mut store = Store::default();
        let names: Vec<&str> = store.get(&p).iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    /// The point of the feature is to spend less context, so what is typed at the agent is a
    /// pointer — never the note.
    #[test]
    fn the_handover_hands_over_a_path_and_not_the_contents() {
        let p = tmp("handover");
        save(&p, "api-shape", "# API shape\n\nthe v2 handler splits into three").unwrap();
        let mut store = Store::default();
        let note = store.get(&p)[0].clone();
        let text = handover_text(&note, &p, &p);
        assert!(text.contains(".horde/memory/api-shape.md"), "{text}");
        assert!(text.contains("API shape"), "{text}");
        assert!(!text.contains("v2 handler"), "the body leaked into the pane: {text}");
    }

    /// An agent in a worktree resolving a relative path would find its own tree's copy, or
    /// nothing at all.
    #[test]
    fn an_agent_outside_the_project_is_given_an_absolute_path() {
        let p = tmp("worktree");
        save(&p, "n", "# n").unwrap();
        let mut store = Store::default();
        let note = store.get(&p)[0].clone();
        let text = handover_text(&note, &p, Path::new("/somewhere/else"));
        assert!(text.contains(&p.to_string_lossy().to_string()), "{text}");
    }
}
