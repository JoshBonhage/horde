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
//! Beside the project, as siblings named after it: an agent called `ads` working on
//! `~/dev/WCP` gets `~/dev/WCP-ads`.
//!
//! ```text
//! ~/dev/WCP          main            you
//! ~/dev/WCP-ads      horde/ads       ads
//! ~/dev/WCP-ops      horde/ops       ops
//! ```
//!
//! Beside rather than inside, which is the opposite of where this started. A worktree nested
//! at `<repo>/.horde/worktrees/<name>` needs `.git/info/exclude` written so the main tree does
//! not report it as untracked, is inside the blast radius of `git clean -ffdx`, and is a
//! directory an agent can wander into while searching its own project. A sibling needs none of
//! that: it is not in the repository, so there is nothing to hide it from, nothing to clean it
//! up by accident, and no way to recurse into it from the main tree.
//!
//! It is also the layout you can *see*. Worktrees are where the work is, and work you cannot
//! find in your editor's file list may as well not exist.
//!
//! # Which trees are horde's
//!
//! The branch, not the path. Every worktree horde creates is on `horde/<name>`, so a tree you
//! made yourself is never listed and never removable — wherever you put it, and whatever you
//! called the directory. It also means the trees an older horde nested inside the repository
//! are still found, still listed, and still removable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// Branches horde creates are prefixed, so `git branch` says where they came from — and so
/// horde can tell its own worktrees from yours without owning the path they sit at.
pub const BRANCH_PREFIX: &str = "horde/";

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
///
/// Both halves are needed. The branch prefix says horde made it; the `.git` check says this is
/// a linked worktree at all — in one it is a *file* pointing at the real git directory, in a
/// main tree it is a directory. Without that second half, checking out `horde/ads` in the main
/// tree by hand would make the main tree report itself as an agent's.
pub fn is_agent_worktree(dir: &Path, branch: &str) -> bool {
    branch.starts_with(BRANCH_PREFIX) && dir.join(".git").is_file()
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

/// Where a named worktree goes: beside the project, named after it.
///
/// `~/dev/WCP` + `ads` becomes `~/dev/WCP-ads`. A repository at the filesystem root has no
/// sibling to be, and falls back to nesting rather than failing.
pub fn worktree_path(root: &Path, name: &str) -> PathBuf {
    let project = root.file_name().map(|n| n.to_string_lossy().to_string());
    match (root.parent(), project) {
        (Some(parent), Some(project)) if !project.is_empty() => {
            parent.join(format!("{project}-{name}"))
        }
        _ => root.join(name),
    }
}

/// The worktree already on `branch`, if there is one.
///
/// Asked before creating, because "already there" is the resume case rather than an error —
/// and because it finds a tree an older horde nested inside the repository, so upgrading does
/// not orphan the work in it.
fn worktree_on_branch(root: &Path, branch: &str) -> Option<PathBuf> {
    let out = git(root, &["worktree", "list", "--porcelain"])?;
    let mut path: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            if b.strip_prefix("refs/heads/").unwrap_or(b) == branch {
                return path;
            }
        }
    }
    None
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

    // Already checked out somewhere: that is a resume, not an error. Asked by branch rather
    // than by path so it also finds a tree an older horde nested inside the repository, and so
    // a tree you moved yourself is still found where you moved it.
    if let Some(existing) = worktree_on_branch(&root, &branch) {
        return Ok(existing);
    }

    // The path is free of git but not of the filesystem. git would refuse with "already
    // exists"; saying whose it is saves the caller working that out.
    if path.exists() {
        return Err(anyhow!(
            "{} already exists and is not a horde worktree — move it, or give the agent \
             another name",
            path.display()
        ));
    }

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
/// Identified by the `horde/` branch prefix, not by where the directory sits. A worktree you
/// made yourself is yours wherever it is, and listing it here would invite
/// `horde worktree remove` to delete it. Trees an older horde nested inside the repository
/// carry the same prefix, so they keep showing up and stay removable.
pub fn list_worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    let root = main_root(dir).ok_or_else(|| anyhow!("{} is not in a git repository", dir.display()))?;
    let out = git_checked(&root, &["worktree", "list", "--porcelain"])?;

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
                // The name is the half of the branch after the prefix, not the directory's —
                // the directory is `WCP-ads` and the agent is `ads`, and the name is what you
                // type at `worktree remove`.
                if let Some(name) = branch.strip_prefix(BRANCH_PREFIX) {
                    let dirty = probe(&p).map(|r| r.dirty).unwrap_or(false);
                    found.push(Worktree {
                        name: name.to_string(),
                        path: p,
                        branch: branch.clone(),
                        dirty,
                    });
                }
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// The worktree horde made for `name`, whatever directory it ended up in.
pub fn worktree_for(dir: &Path, name: &str) -> Result<Worktree> {
    let name = sanitise(name).ok_or_else(|| anyhow!("{name:?} is not a worktree name"))?;
    list_worktrees(dir)?
        .into_iter()
        .find(|w| w.name == name)
        .ok_or_else(|| anyhow!("no worktree called {name}"))
}

/// Remove one worktree. The branch survives: it may hold commits, and this is a tidy-up
/// command rather than a delete-my-work command.
///
/// Refuses a dirty tree unless forced, which is git's own rule and the right one.
pub fn remove_worktree(dir: &Path, name: &str, force: bool) -> Result<PathBuf> {
    let name = sanitise(name).ok_or_else(|| anyhow!("{name:?} is not a worktree name"))?;
    let root = main_root(dir).ok_or_else(|| anyhow!("{} is not in a git repository", dir.display()))?;
    // Resolved from the listing rather than computed, so a tree an older horde put inside the
    // repository is removable by the same name as one beside it.
    let path = worktree_for(dir, &name)?.path;
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
    ///
    /// Clears the siblings too: worktrees now live *beside* the repository, so removing only
    /// the repository leaves last run's `horde-repo-<label>-builder` behind — and the second
    /// run then fails on a directory it is right to refuse to overwrite.
    fn repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("horde-repo-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            let prefix = format!("horde-repo-{label}-");
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
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

    /// The whole reason for the placement: a worktree beside the repository is not *in* it, so
    /// there is nothing for the main tree to report and nothing to exclude.
    #[test]
    fn a_worktree_is_invisible_to_the_main_tree() {
        let dir = repo("hidden");
        add_worktree(&dir, "builder", None).unwrap();
        let status = git(&dir, &["status", "--short"]).unwrap();
        assert!(status.is_empty(), "main tree should be clean, got {status:?}");
        assert!(
            !dir.join(".horde").exists(),
            "nothing of horde's belongs inside the repository any more"
        );
    }

    /// The layout, in one assertion: an agent named `builder` on a project named `X` works in
    /// `X-builder`, beside it, where you can see it.
    #[test]
    fn a_worktree_sits_beside_the_project_named_after_it() {
        let dir = repo("add");
        let path = add_worktree(&dir, "builder", None).unwrap();

        assert_eq!(path.parent(), dir.parent(), "the worktree is a sibling of the project");
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("{}-builder", dir.file_name().unwrap().to_string_lossy()),
            "named <project>-<agent>"
        );
        assert_eq!(probe(&path).unwrap().branch, "horde/builder");
        // And the main tree is untouched by whatever happens in there.
        assert_eq!(probe(&dir).unwrap().branch, "main");
        let _ = std::fs::remove_dir_all(&path);
    }

    /// Which trees are horde's is decided by the branch, so a main tree that happens to be on
    /// a `horde/` branch is still yours — and a linked worktree on one is still horde's.
    #[test]
    fn a_tree_is_hordes_by_its_branch_and_by_being_a_linked_worktree() {
        let dir = repo("ident");
        let path = add_worktree(&dir, "builder", None).unwrap();

        assert!(is_agent_worktree(&path, "horde/builder"));
        // The main tree, even wearing the same branch name.
        assert!(!is_agent_worktree(&dir, "horde/builder"), "the main tree is never an agent's");
        // A linked worktree you made yourself, on your own branch.
        assert!(!is_agent_worktree(&path, "my-feature"));
        let _ = std::fs::remove_dir_all(&path);
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
        let _ = std::fs::remove_dir_all(&a);
    }

    /// An upgrade must not orphan the work an older horde left inside the repository.
    ///
    /// Those trees are at `<repo>/.horde/worktrees/<name>`, which is not where the current
    /// scheme looks. They carry the same `horde/` branch, so listing finds them, `remove` takes
    /// them by name, and asking for the same agent again resumes the tree it is already in
    /// rather than trying to check the branch out twice.
    #[test]
    fn a_worktree_an_older_horde_nested_inside_the_repo_is_still_managed() {
        let dir = repo("legacy");
        let legacy = dir.join(".horde/worktrees/builder");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        git_checked(
            &dir,
            &["worktree", "add", &legacy.to_string_lossy(), "-b", "horde/builder"],
        )
        .unwrap();

        let found = list_worktrees(&dir).unwrap();
        assert_eq!(found.len(), 1, "the nested tree is still listed: {found:?}");
        assert_eq!(found[0].name, "builder");
        assert_eq!(found[0].path, legacy);

        // Resuming lands back in it rather than trying to create a sibling on a branch that is
        // already checked out.
        assert_eq!(add_worktree(&dir, "builder", None).unwrap(), legacy);
        // And it is removable by the name you would use for any other.
        remove_worktree(&dir, "builder", true).unwrap();
        assert!(!legacy.is_dir());
    }

    #[test]
    fn listing_finds_hordes_worktrees_and_ignores_your_own() {
        let dir = repo("list");
        let a = add_worktree(&dir, "builder", None).unwrap();
        let b = add_worktree(&dir, "reviewer", None).unwrap();
        // One you made yourself. Now that horde no longer owns a directory, the branch is the
        // only thing keeping this one yours — including when you put it right beside the
        // others, where the old path rule would have said nothing about it either way.
        let mine = dir.parent().unwrap().join(format!(
            "{}-mine",
            dir.file_name().unwrap().to_string_lossy()
        ));
        git_checked(&dir, &["worktree", "add", &mine.to_string_lossy(), "-b", "mine"]).unwrap();

        let found = list_worktrees(&dir).unwrap();
        let names: Vec<&str> = found.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["builder", "reviewer"], "a tree on your own branch is yours");
        assert_eq!(found[0].branch, "horde/builder");
        assert!(remove_worktree(&dir, "mine", true).is_err(), "and is not removable by horde");

        for p in [a, b, mine] {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    #[test]
    fn removing_refuses_to_throw_away_uncommitted_work() {
        let dir = repo("remove");
        let path = add_worktree(&dir, "builder", None).unwrap();
        std::fs::write(path.join("a.txt"), "unsaved\n").unwrap();
        assert!(remove_worktree(&dir, "builder", false).is_err(), "a dirty tree is not tidy-up");
        assert!(remove_worktree(&dir, "nobody", true).is_err(), "and a name with no tree is an error");
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
