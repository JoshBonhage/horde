//! Walking a project's files, so opening a project shows you the project.
//!
//! Not a git-aware ignore implementation: that is a dependency and a specification, and what
//! this needs is "do not walk into a node_modules". The skip list is the handful of
//! directories that are build output or vendored code in every language anyone uses here.
//! Anything it gets wrong is visible as an extra directory in a list, which is a far cheaper
//! failure than a missing file.

use std::path::{Path, PathBuf};

/// Directories never worth walking into.
const SKIP: &[&str] = &[
    "target", "node_modules", "dist", "build", "out", "vendor", "__pycache__", ".venv", "venv",
    ".next", ".nuxt", ".cache", "coverage", ".pytest_cache", ".mypy_cache", ".gradle",
];

/// Files never worth listing, by extension. Binaries a terminal cannot show and nobody edits.
const SKIP_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "tar", "so", "dylib", "dll",
    "a", "o", "class", "jar", "wasm", "bin", "exe", "lock", "woff", "woff2", "ttf", "mp4", "mov",
];

/// A file worth offering to open.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Relative to the project root.
    pub path: PathBuf,
    pub size: u64,
}

/// Every file under `root` worth editing, capped.
///
/// The cap is not a performance guard so much as an honesty one: a list of forty thousand
/// paths is not something a person picks from, and pretending to offer one is worse than
/// saying there are too many to show.
pub fn list(root: &Path, limit: usize) -> (Vec<Entry>, bool) {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut truncated = false;

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy().to_string();
            let Ok(meta) = e.metadata() else { continue };

            if meta.is_dir() {
                // Hidden directories are configuration and history, not the project's code.
                // `.horde` in particular is horde's own scratch, and listing it invites
                // someone to edit a file that exists to be written by a machine.
                if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            if SKIP_EXT.contains(&ext.as_str()) {
                continue;
            }
            // A file too large to hold in a terminal buffer comfortably is one this editor
            // has no business opening.
            if meta.len() > 2_000_000 {
                continue;
            }
            if out.len() >= limit {
                truncated = true;
                break;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            out.push(Entry { path: rel, size: meta.len() });
        }
        if truncated {
            break;
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horde-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for sub in ["src", "target/debug", "node_modules/pkg", ".git", "docs"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        std::fs::write(d.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(d.join("docs/readme.md"), "# hi\n").unwrap();
        std::fs::write(d.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(d.join("target/debug/binary"), "\0\0").unwrap();
        std::fs::write(d.join("node_modules/pkg/index.js"), "x\n").unwrap();
        std::fs::write(d.join(".git/HEAD"), "ref\n").unwrap();
        std::fs::write(d.join("logo.png"), "\0").unwrap();
        d
    }

    /// What a person means by "the project" is the code they wrote, not the output of
    /// building it or the libraries somebody else did.
    #[test]
    fn build_output_vendored_code_and_binaries_are_left_out() {
        let d = tree("basic");
        let (files, truncated) = list(&d, 100);
        let paths: Vec<String> =
            files.iter().map(|f| f.path.to_string_lossy().to_string()).collect();

        assert!(paths.contains(&"src/main.rs".to_string()), "{paths:?}");
        assert!(paths.contains(&"docs/readme.md".to_string()), "{paths:?}");
        assert!(paths.contains(&"Cargo.toml".to_string()), "{paths:?}");

        assert!(!paths.iter().any(|p| p.starts_with("target")), "build output: {paths:?}");
        assert!(!paths.iter().any(|p| p.contains("node_modules")), "vendored: {paths:?}");
        assert!(!paths.iter().any(|p| p.contains(".git")), "history: {paths:?}");
        assert!(!paths.iter().any(|p| p.ends_with(".png")), "binaries: {paths:?}");
        assert!(!truncated);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A cap that silently drops files would be worse than one that says so: the point of a
    /// file list is that what you are looking for is in it.
    #[test]
    fn a_project_too_large_to_list_says_so() {
        let d = tree("big");
        let (files, truncated) = list(&d, 2);
        assert_eq!(files.len(), 2);
        assert!(truncated, "and it admits there are more");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_listing_is_sorted_so_it_does_not_move_between_looks() {
        let d = tree("order");
        let (a, _) = list(&d, 100);
        let (b, _) = list(&d, 100);
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&d);
    }
}
