//! Git facts about a pane's directory, and the worktrees horde makes for agents.
//!
//! Two jobs that share one subprocess vocabulary, which is why they are one module.
//!
//! **Facts.** Which branch a project is on and whether it is dirty. Read on a slow cadence
//! and cached, because it costs a fork per repository and the answer changes on the timescale
//! of a person committing, not of a frame being drawn.
//!
//! **Worktrees.** A fleet of agents in one repository stomp each other: two of them editing
//! the same file on the same branch is not a merge conflict you get to resolve, it is one
//! agent's work silently overwritten. A worktree per agent is the only real fix, and git has
//! supported it for a decade.
//!
//! # Where they live, and why
//!
//! `<repo>/.horde/worktrees/<name>`, with `.horde/` written to `.git/info/exclude`.
//!
//! Both halves of that are load-bearing, and both were checked rather than assumed:
//!
//! * **`info/exclude`, not `.gitignore`.** The exclude file is per-clone and is not itself
//!   tracked, so horde can write it without modifying a single file the repository owns.
//!   That matters most in repositories you do not control. Without it, every agent in the
//!   main tree sees `?? .horde/` and the first one to run `git add -A` commits a mess.
//! * **The leading dot.** Agent search tools skip dot-directories by default, so `.horde/`
//!   is invisible to a `rg` in the main tree while `horde-worktrees/` would flood every
//!   search with one hit per worktree. The dot is doing the work, not the nesting.
//!
//! The one hazard this placement keeps is `git clean -ffdx`, which removes nested
//! repositories. Plain `-fd` and even `-fdx` refuse ("Would skip repository"), so it takes a
//! deliberately violent reset to lose a worktree this way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// Where an agent's worktree lives, relative to the repository root.
pub const WORKTREE_DIR: &str = ".horde/worktrees";

/// What horde writes to `.git/info/exclude`. One entry covers every worktree.
const EXCLUDE_LINE: &str = ".horde/";

/// Branches horde creates are prefixed, so `git branch` says where they came from.
const BRANCH_PREFIX: &str = "horde/";

/// How often a repository's branch and dirty state are re-read.
///
/// Deliberately slow. This is two forks per project, and what it reports changes when you
/// commit or switch branches, not between frames.
const REFRESH: Duration = Duration::from_secs(5);

/// What git says about a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The working tree's own root, which for a worktree is the worktree rather than the
    /// repository it came from.
    pub root: PathBuf,
    /// Branch name, or `detached` when there is no branch to name.
    pub branch: String,
    /// Tracked files differ from `HEAD`.
    ///
    /// Untracked files deliberately do not count. A dev server's build output, a scratch
    /// file and a fresh `node_modules` would otherwise leave every project permanently
    /// marked, which is the fastest way to make a signal worth ignoring.
    pub dirty: bool,
}

/// Run git in `dir`, returning stdout when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run git in `dir`, reporting the failure rather than swallowing it.
fn git_checked(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        // git's own message is far better than anything horde could synthesise, and it is
        // what the user would see running the command by hand.
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!(if err.is_empty() { format!("git {} failed", args.join(" ")) } else { err }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What branch this directory is on, and whether it is dirty. `None` when it is not a
/// repository at all, which is the common case for a plain shell in a home directory.
pub fn probe(dir: &Path) -> Option<Repo> {
    let out = git(dir, &["rev-parse", "--show-toplevel", "--abbrev-ref", "HEAD"])?;
    let mut lines = out.lines();
    let root = PathBuf::from(lines.next()?);
    let branch = lines.next()?.to_string();
    // `--abbrev-ref` says `HEAD` when there is no branch, which is a name that would read as
    // a branch called HEAD.
    let branch = if branch == "HEAD" { "detached".to_string() } else { branch };
    let dirty = git(dir, &["diff", "--quiet", "HEAD"]).is_none();
    Some(Repo { root, branch, dirty })
}

/// The repository a worktree belongs to, which is itself when it is not one.
///
/// `--show-toplevel` answers "which tree am I in", and that is the wrong question when the
/// caller wants to add a worktree: adding one from inside a worktree must still hang it off
/// the original repository.
pub fn main_root(dir: &Path) -> Option<PathBuf> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    // `<repo>/.git` for an ordinary clone. The parent is the repository root.
    PathBuf::from(common).parent().map(|p| p.to_path_buf())
}

/// Whether this directory is one of horde's own agent worktrees rather than a tree you made
/// yourself. The client cannot work this out for itself: it never sees a path.
pub fn is_agent_worktree(dir: &Path) -> bool {
    dir.parent().is_some_and(|p| p.ends_with(WORKTREE_DIR))
}

/// Branch and dirty state per directory, re-read on [`REFRESH`].
///
/// Keyed by the directory asked about rather than by repository root, because the caller has
/// a pane's cwd and finding the root is itself one of the forks being avoided.
#[derive(Default)]
pub struct Cache {
    entries: HashMap<PathBuf, (Instant, Option<Repo>)>,
}

impl Cache {
    /// The cached answer, refreshing it when stale.
    pub fn get(&mut self, dir: &Path) -> Option<&Repo> {
        let stale = match self.entries.get(dir) {
            Some((at, _)) => at.elapsed() >= REFRESH,
            None => true,
        };
        if stale {
            self.entries.insert(dir.to_path_buf(), (Instant::now(), probe(dir)));
        }
        self.entries.get(dir).and_then(|(_, r)| r.as_ref())
    }

    /// The cached answer without refreshing it. For readers that cannot take `&mut`, which is
    /// every one of them downstream of a snapshot: the refresh happens once per tick, and a
    /// reader is describing the session as of that tick anyway.
    pub fn peek(&self, dir: &Path) -> Option<&Repo> {
        self.entries.get(dir).and_then(|(_, r)| r.as_ref())
    }

    /// Forget directories nothing points at any more, so closing panes does not leave the
    /// cache growing for the life of the daemon.
    pub fn retain(&mut self, live: impl Fn(&Path) -> bool) {
        self.entries.retain(|k, _| live(k));
    }
}

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

/// One worktree horde made, as `worktree list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    /// Tracked files differ from `HEAD`, so removing it would lose work.
    pub dirty: bool,
}

/// Directory names are path components and branch names are refs, and neither tolerates
/// everything an agent might be called.
fn sanitise(name: &str) -> Option<String> {
    let s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let s = s.trim_matches(['-', '.']).to_string();
    (!s.is_empty()).then_some(s)
}

/// Make sure `.horde/` is excluded, so the main tree never reports the worktrees as
/// untracked and no agent can commit one by accident.
///
/// `info/exclude` rather than `.gitignore`: it is per-clone and untracked, so this modifies
/// nothing the repository owns. Safe to call repeatedly.
pub fn ensure_excluded(root: &Path) -> Result<()> {
    let git_dir = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .ok_or_else(|| anyhow!("{} is not a git repository", root.display()))?;
    let info = PathBuf::from(git_dir).join("info");
    std::fs::create_dir_all(&info).with_context(|| format!("creating {}", info.display()))?;
    let path = info.join("exclude");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.lines().any(|l| l.trim() == EXCLUDE_LINE) {
        return Ok(());
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("# horde: agent worktrees\n");
    next.push_str(EXCLUDE_LINE);
    next.push('\n');
    std::fs::write(&path, next).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Where a named worktree goes.
pub fn worktree_path(root: &Path, name: &str) -> PathBuf {
    root.join(WORKTREE_DIR).join(name)
}

/// Create a worktree for `name`, or hand back the one that is already there.
///
/// Returns the directory to run the agent in. Re-running is not an error: a pane that closed
/// and is being replaced should land back in the same tree, with whatever work is in it.
pub fn add_worktree(dir: &Path, name: &str, branch: Option<&str>) -> Result<PathBuf> {
    let name = sanitise(name).ok_or_else(|| anyhow!("{name:?} has no usable characters for a directory name"))?;
    let root = main_root(dir).ok_or_else(|| anyhow!("{} is not in a git repository", dir.display()))?;
    let path = worktree_path(&root, &name);
    let branch = match branch {
        Some(b) => b.to_string(),
        None => format!("{BRANCH_PREFIX}{name}"),
    };

    // Already there. Checking the path rather than parsing `worktree list` first, because the
    // directory existing is the case that would otherwise fail with git's own confusing
    // "already exists" and leave the caller unable to tell "yours" from "someone else's".
    if path.is_dir() {
        let on = probe(&path).map(|r| r.branch).unwrap_or_default();
        if on == branch || on.is_empty() {
            return Ok(path);
        }
        return Err(anyhow!(
            "{} already exists and is on {on}, not {branch}",
            path.display()
        ));
    }

    ensure_excluded(&root)?;
    std::fs::create_dir_all(path.parent().unwrap_or(&root))?;

    let path_str = path.to_string_lossy().to_string();
    // An existing branch is checked out rather than recreated, so `--worktree` twice for one
    // name across a daemon restart resumes instead of failing.
    let exists = git(&root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .is_some();
    let args: Vec<&str> = if exists {
        vec!["worktree", "add", &path_str, &branch]
    } else {
        vec!["worktree", "add", &path_str, "-b", &branch]
    };
    git_checked(&root, &args)?;
    Ok(path)
}

/// Every worktree horde made for this repository.
///
/// Only the ones under [`WORKTREE_DIR`]. A worktree you made yourself elsewhere is yours, and
/// listing it here would invite `horde worktree remove` to delete it.
pub fn list_worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    let root = main_root(dir).ok_or_else(|| anyhow!("{} is not in a git repository", dir.display()))?;
    let out = git_checked(&root, &["worktree", "list", "--porcelain"])?;
    let want = root.join(WORKTREE_DIR);

    let mut found = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = String::new();
    // Records are blank-line separated, so a trailing empty push flushes the last one.
    for line in out.lines().chain(std::iter::once("")) {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
            branch = "detached".into();
        } else if let Some(b) = line.strip_prefix("branch ") {
            // Only the `refs/heads/` prefix comes off. Splitting on every slash would turn
            // horde's own `horde/builder` into `builder` and lose which tool made it.
            branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        } else if line.is_empty() {
            if let Some(p) = path.take() {
                if p.starts_with(&want) {
                    let name =
                        p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let dirty = probe(&p).map(|r| r.dirty).unwrap_or(false);
                    found.push(Worktree { name, path: p, branch: branch.clone(), dirty });
                }
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Remove one worktree. The branch survives: it may hold commits, and this is a tidy-up
/// command rather than a delete-my-work command.
///
/// Refuses a dirty tree unless forced, which is git's own rule and the right one.
pub fn remove_worktree(dir: &Path, name: &str, force: bool) -> Result<PathBuf> {
    let name = sanitise(name).ok_or_else(|| anyhow!("{name:?} is not a worktree name"))?;
    let root = main_root(dir).ok_or_else(|| anyhow!("{} is not in a git repository", dir.display()))?;
    let path = worktree_path(&root, &name);
    if !path.is_dir() {
        return Err(anyhow!("no worktree called {name}"));
    }
    let path_str = path.to_string_lossy().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_str);
    git_checked(&root, &args)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway repository with one commit.
    fn repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("horde-repo-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            git_checked(&dir, &args).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
        git_checked(&dir, &["add", "-A"]).unwrap();
        git_checked(&dir, &["commit", "-qm", "init"]).unwrap();
        // macOS puts the temp dir behind a symlink, and git reports the resolved path. Without
        // this every `starts_with` in this module compares two spellings of one directory.
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn probe_reads_the_branch_and_notices_a_change() {
        let dir = repo("probe");
        let r = probe(&dir).unwrap();
        assert_eq!(r.branch, "main");
        assert!(!r.dirty, "a fresh commit is clean");

        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        assert!(probe(&dir).unwrap().dirty);
    }

    /// Untracked files are not dirtiness. A build directory would otherwise leave every
    /// project permanently marked.
    #[test]
    fn an_untracked_file_does_not_count_as_dirty() {
        let dir = repo("untracked");
        std::fs::write(dir.join("scratch.log"), "noise\n").unwrap();
        assert!(!probe(&dir).unwrap().dirty);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_not_one() {
        let dir = std::env::temp_dir().join("horde-repo-none");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(probe(&dir), None);
    }

    /// The whole reason for the placement: the main tree must not see the worktrees.
    #[test]
    fn a_worktree_is_invisible_to_the_main_tree() {
        let dir = repo("hidden");
        add_worktree(&dir, "builder", None).unwrap();
        let status = git(&dir, &["status", "--short"]).unwrap();
        assert!(status.is_empty(), "main tree should be clean, got {status:?}");
    }

    #[test]
    fn the_exclude_entry_is_written_once_and_leaves_gitignore_alone() {
        let dir = repo("exclude");
        ensure_excluded(&dir).unwrap();
        ensure_excluded(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(EXCLUDE_LINE).count(), 1, "{text}");
        assert!(!dir.join(".gitignore").exists(), "the repository's own files are untouched");
    }

    #[test]
    fn adding_a_worktree_puts_an_agent_on_its_own_branch() {
        let dir = repo("add");
        let path = add_worktree(&dir, "builder", None).unwrap();
        assert!(path.ends_with(".horde/worktrees/builder"));
        assert_eq!(probe(&path).unwrap().branch, "horde/builder");
        // And the main tree is untouched by whatever happens in there.
        assert_eq!(probe(&dir).unwrap().branch, "main");
    }

    /// A pane that closed and is being replaced must land back in its own tree, with the work
    /// that is in it, rather than failing on git's "already exists".
    #[test]
    fn adding_the_same_worktree_twice_resumes_it() {
        let dir = repo("resume");
        let a = add_worktree(&dir, "builder", None).unwrap();
        std::fs::write(a.join("wip.txt"), "half done\n").unwrap();
        let b = add_worktree(&dir, "builder", None).unwrap();
        assert_eq!(a, b);
        assert!(b.join("wip.txt").exists(), "work in the tree survived");
    }

    #[test]
    fn listing_finds_hordes_worktrees_and_ignores_your_own() {
        let dir = repo("list");
        add_worktree(&dir, "builder", None).unwrap();
        add_worktree(&dir, "reviewer", None).unwrap();
        // One you made yourself, somewhere else. It is not horde's to list or to remove.
        let mine = dir.join("elsewhere");
        git_checked(&dir, &["worktree", "add", &mine.to_string_lossy(), "-b", "mine"]).unwrap();

        let found = list_worktrees(&dir).unwrap();
        let names: Vec<&str> = found.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["builder", "reviewer"]);
        assert_eq!(found[0].branch, "horde/builder");
    }

    #[test]
    fn removing_refuses_to_throw_away_uncommitted_work() {
        let dir = repo("remove");
        let path = add_worktree(&dir, "builder", None).unwrap();
        std::fs::write(path.join("a.txt"), "unsaved\n").unwrap();
        assert!(remove_worktree(&dir, "builder", false).is_err(), "a dirty tree is not tidy-up");
        remove_worktree(&dir, "builder", true).unwrap();
        assert!(!path.is_dir());
        // The branch survives a removed worktree: it may hold commits.
        assert!(git(&dir, &["rev-parse", "--verify", "--quiet", "refs/heads/horde/builder"])
            .is_some());
    }

    #[test]
    fn a_name_that_would_not_survive_a_path_is_reshaped() {
        assert_eq!(sanitise("Code Reviewer"), Some("Code-Reviewer".into()));
        assert_eq!(sanitise("../escape"), Some("escape".into()));
        assert_eq!(sanitise("///"), None);
    }
}
