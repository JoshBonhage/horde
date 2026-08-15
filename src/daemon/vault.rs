//! The per-project note index: what horde knows about a vault of markdown.
//!
//! Shaped after [`super::repo`]: an expensive per-directory fact, refreshed on the tick
//! loop's slow cadence, read by every snapshot and written by nothing else. The difference
//! is that this one parses files rather than forking git, so it carries a budget — see
//! [`Index::refresh`].
//!
//! **Notes are content, the index is cache.** The notes themselves live in a tracked
//! directory the human owns and commits; nothing here is ever written to disk. Rebuilding a
//! thousand notes costs tens of milliseconds, and a warm-start cache file would be an
//! invalidation bug bought to save nothing.
//!
//! The link forms it supports were chosen by looking at a real vault rather than at
//! Obsidian's documentation — see the tests, which carry the evidence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Files a wikilink may point at other than a note.
///
/// An allowlist rather than "does it have a dot", because a real vault contains
/// `Item Spec 5.x (Omni) Migration Guide`, whose extension is not `.x (Omni) Migration Guide`.
const LINK_EXTENSIONS: &[&str] =
    &["md", "canvas", "base", "png", "jpg", "jpeg", "gif", "svg", "webp", "pdf"];

/// Directories never worth walking into.
const SKIP_DIRS: &[&str] = &[".obsidian", ".git", ".horde", "node_modules", ".trash", "target"];

pub type NoteId = usize;

/// One link out of a note.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// The note or file being pointed at, with any alias and heading stripped off.
    pub target: String,
    /// The `#heading` part, when there was one.
    pub heading: Option<String>,
    /// `![[...]]` rather than `[[...]]`.
    pub embed: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Note {
    /// Relative to the vault root, so the index survives the vault moving.
    pub path: PathBuf,
    /// First H1, else the file stem — the same order Obsidian resolves a display name in.
    pub title: String,
    pub mtime: u64,
    pub size: u64,
    pub headings: Vec<String>,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub links: Vec<Link>,
}

impl Note {
    /// The name a `[[wikilink]]` would use: the file stem, not the title.
    pub fn stem(&self) -> String {
        self.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }
}

/// Split the inside of a `[[...]]` into target, heading and alias.
///
/// The escaped pipe is the whole reason this is a function rather than two `split` calls.
/// Inside a markdown table a wikilink alias must be written `\|`, and every one of the 31
/// such links in the vault this was built against would otherwise resolve to a target with a
/// trailing backslash — a phantom broken link, in the one place a table makes it least
/// visible.
pub fn split_target(inner: &str) -> (String, Option<String>, Option<String>) {
    let unescaped = inner.replace(r"\|", "|");
    let (target, alias) = match unescaped.split_once('|') {
        Some((t, a)) => (t.to_string(), Some(a.trim().to_string())),
        None => (unescaped, None),
    };
    // `#` splits after the alias, never before: `[[note#heading|shown]]` is a heading link.
    let (target, heading) = match target.split_once('#') {
        Some((t, h)) => (t.to_string(), Some(h.trim().to_string())),
        None => (target, None),
    };
    (target.trim().to_string(), heading, alias)
}

/// The stem a link target resolves against, minus a real file extension.
///
/// Only strips an extension from the allowlist, so a dot in a title stays part of the title.
fn target_stem(target: &str) -> String {
    let p = Path::new(target);
    let is_file = p
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| LINK_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if is_file {
        p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| target.into())
    } else {
        // Path-form links (`[[folder/note]]`) resolve on their last component.
        p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| target.into())
    }
}

/// Pull `tags:` and `aliases:` out of a YAML frontmatter block.
///
/// Handles the two forms that occur: an inline array (`tags: [taw, dev]`, which is what 165
/// of 170 notes in the reference vault use) and a `-` list under the key. Deliberately not a
/// YAML parser — a dependency for two keys would be a poor trade, and anything it could not
/// read it would have to guess at.
fn parse_frontmatter(block: &str) -> (Vec<String>, Vec<String>) {
    let mut tags = Vec::new();
    let mut aliases = Vec::new();
    let mut collecting: Option<&mut Vec<String>> = None;

    for line in block.lines() {
        let key = line.split_once(':').map(|(k, _)| k.trim());
        let is_new_key = !line.starts_with(char::is_whitespace)
            && !line.trim_start().starts_with('-')
            && key.is_some();

        if is_new_key {
            let (k, rest) = line.split_once(':').unwrap();
            let rest = rest.trim();
            let bucket = match k.trim() {
                "tags" => Some(&mut tags),
                "aliases" | "alias" => Some(&mut aliases),
                _ => None,
            };
            collecting = None;
            if let Some(b) = bucket {
                if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    b.extend(
                        inner
                            .split(',')
                            .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                            .filter(|s| !s.is_empty()),
                    );
                } else if rest.is_empty() {
                    collecting = Some(b); // a `-` list follows
                } else {
                    b.push(rest.trim_matches(['"', '\'']).to_string());
                }
            }
            continue;
        }

        if let Some(b) = collecting.as_deref_mut() {
            if let Some(item) = line.trim().strip_prefix('-') {
                let item = item.trim().trim_matches(['"', '\'']);
                if !item.is_empty() {
                    b.push(item.to_string());
                }
            }
        }
    }
    (tags, aliases)
}

/// Everything the index keeps about one file, from its text.
///
/// Pure, so the whole parser is testable without touching a disk.
pub fn parse(text: &str, stem: &str) -> Note {
    let mut note = Note { title: stem.to_string(), ..Default::default() };

    // Frontmatter first: pulldown-cmark can emit it as a metadata block, but taking it here
    // keeps the two concerns apart and means the body parser never sees it at all.
    let body = match text.strip_prefix("---\n") {
        Some(rest) => match rest.split_once("\n---") {
            Some((front, after)) => {
                let (tags, aliases) = parse_frontmatter(front);
                note.tags = tags;
                note.aliases = aliases;
                after.trim_start_matches('\n').trim_start_matches("-\n")
            }
            None => text,
        },
        None => text,
    };

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    // Two jobs, and they need opposite things from the parser.
    //
    // Headings come from the event stream, because a heading's *text* is what a `#heading`
    // link points at. Links and tags are scanned from the raw source instead, with code
    // blanked out: `[[note]]` is a reference link as far as CommonMark is concerned, so by
    // the time it reaches a `Text` event its brackets are gone. Masking rather than
    // reconstructing keeps the source exactly as written and still knows what is code.
    let mut heading_depth: Option<HeadingLevel> = None;
    let mut heading_text = String::new();
    let mut first_h1: Option<String> = None;
    let mut masked: Vec<char> = body.chars().collect();
    let char_at = |b: usize| body[..b.min(body.len())].chars().count();

    for (ev, range) in Parser::new_ext(body, opts).into_offset_iter() {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                heading_depth = Some(level);
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                let text = heading_text.trim().to_string();
                if !text.is_empty() {
                    if heading_depth == Some(HeadingLevel::H1) && first_h1.is_none() {
                        first_h1 = Some(text.clone());
                    }
                    note.headings.push(text);
                }
                heading_depth = None;
            }
            Event::Text(ref t) if heading_depth.is_some() => heading_text.push_str(t),
            // Blank out code so a fenced block full of `[[x]]` reads as what it is:
            // documentation *about* links, not links.
            Event::Code(_) | Event::Start(Tag::CodeBlock(_)) => {
                for c in masked.iter_mut().take(char_at(range.end)).skip(char_at(range.start)) {
                    if *c != '\n' {
                        *c = ' ';
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(h1) = first_h1 {
        note.title = h1;
    }
    let scannable: String = masked.into_iter().collect();
    note.links = scan_links(&scannable);
    note.tags.extend(scan_inline_tags(&scannable));
    note.tags.sort();
    note.tags.dedup();
    note
}

/// Every `[[...]]` and `![[...]]` in already-code-stripped text.
fn scan_links(text: &str) -> Vec<Link> {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == '[' && bytes[i + 1] == '[' {
            let embed = i > 0 && bytes[i - 1] == '!';
            let start = i + 2;
            let mut j = start;
            // A link never spans a line: an unclosed `[[` should not swallow the document.
            while j + 1 < bytes.len() && bytes[j] != '\n' && !(bytes[j] == ']' && bytes[j + 1] == ']')
            {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == ']' {
                let inner: String = bytes[start..j].iter().collect();
                let (target, heading, _alias) = split_target(&inner);
                if !target.is_empty() || heading.is_some() {
                    out.push(Link { target, heading, embed });
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Inline `#tags`, which are rare in practice but free to collect.
///
/// Wikilinks are removed first: `[[#Module C]]` is a link to a heading in the same note, and
/// reading it as a tag called `Module` is exactly the kind of wrong a scanner gets quietly.
fn scan_inline_tags(text: &str) -> Vec<String> {
    let without_links = strip_wikilinks(text);
    let mut out = Vec::new();
    let chars: Vec<char> = without_links.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != '#' {
            continue;
        }
        // Must start a word, and `#1` is a number, not a tag.
        if i > 0 && !chars[i - 1].is_whitespace() {
            continue;
        }
        let tag: String = chars[i + 1..]
            .iter()
            .take_while(|c| c.is_alphanumeric() || **c == '/' || **c == '-' || **c == '_')
            .collect();
        if !tag.is_empty() && tag.chars().next().is_some_and(|c| c.is_alphabetic()) {
            out.push(tag);
        }
    }
    out
}

fn strip_wikilinks(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '[' && chars[i + 1] == '[' {
            let mut j = i + 2;
            while j + 1 < chars.len() && !(chars[j] == ']' && chars[j + 1] == ']') {
                j += 1;
            }
            i = (j + 2).min(chars.len());
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The whole vault, as horde understands it.
#[derive(Debug, Default)]
pub struct Index {
    pub root: PathBuf,
    pub notes: Vec<Note>,
    /// Lowercase stem to note, for link resolution.
    by_stem: HashMap<String, NoteId>,
    /// Lowercase alias to note. Consulted only after stems, because an alias that shadows a
    /// real filename should lose to it.
    by_alias: HashMap<String, NoteId>,
    backlinks: HashMap<NoteId, Vec<NoteId>>,
    /// Link targets that match nothing — the graph's ghost nodes, and a decent spell-check.
    pub unresolved: Vec<(NoteId, String)>,
}

impl Index {
    pub fn new(root: PathBuf) -> Self {
        Self { root, ..Default::default() }
    }

    #[allow(clippy::len_without_is_empty)] // an empty vault and no vault are different
    pub fn len(&self) -> usize {
        self.notes.len()
    }

    pub fn note(&self, id: NoteId) -> Option<&Note> {
        self.notes.get(id)
    }

    /// Resolve a link target: filename first, then alias.
    pub fn resolve(&self, target: &str) -> Option<NoteId> {
        let key = target_stem(target).to_lowercase();
        self.by_stem.get(&key).or_else(|| self.by_alias.get(&key)).copied()
    }

    /// Notes that link to this one.
    pub fn backlinks(&self, id: NoteId) -> &[NoteId] {
        self.backlinks.get(&id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Notes whose title, stem or tags contain `q`, best matches first.
    ///
    /// Substring rather than fuzzy: the fuzzy matcher belongs in the finder, where a ranked
    /// list is the product. Here the caller is often an agent asking a plain question.
    pub fn search(&self, q: &str) -> Vec<NoteId> {
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            let mut all: Vec<NoteId> = (0..self.notes.len()).collect();
            all.sort_by_key(|i| std::cmp::Reverse(self.notes[*i].mtime));
            return all;
        }
        let mut hits: Vec<(u8, NoteId)> = Vec::new();
        for (i, n) in self.notes.iter().enumerate() {
            let title = n.title.to_lowercase();
            let stem = n.stem().to_lowercase();
            let rank = if title == q || stem == q {
                0
            } else if title.starts_with(&q) || stem.starts_with(&q) {
                1
            } else if title.contains(&q) || stem.contains(&q) {
                2
            } else if n.tags.iter().any(|t| t.to_lowercase().contains(&q)) {
                3
            } else {
                continue;
            };
            hits.push((rank, i));
        }
        hits.sort_by_key(|(r, i)| (*r, std::cmp::Reverse(self.notes[*i].mtime)));
        hits.into_iter().map(|(_, i)| i).collect()
    }

    /// The link graph: every note, every link between them, and every note somebody meant
    /// to write.
    ///
    /// Ghosts earn their place. A vault's unwritten notes are the shape of what it is
    /// missing, and a graph that hid them would draw a tidier picture than the truth.
    pub fn graph(&self) -> crate::proto::VaultGraph {
        use crate::proto::{GraphNode, VaultGraph};
        let mut g = VaultGraph::default();
        let mut degree: Vec<u16> = vec![0; self.notes.len()];

        // Ghosts first by name, so several links to the same unwritten note share one node.
        let mut ghosts: Vec<String> = Vec::new();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (from, n) in self.notes.iter().enumerate() {
            for l in &n.links {
                if l.target.is_empty() {
                    continue;
                }
                match self.resolve(&l.target) {
                    Some(to) if to != from => {
                        edges.push((from, to));
                        degree[from] += 1;
                        degree[to] += 1;
                    }
                    Some(_) => {}
                    None => {
                        let key = target_stem(&l.target).to_lowercase();
                        let gi = match ghosts.iter().position(|x| *x == key) {
                            Some(i) => i,
                            None => {
                                ghosts.push(key);
                                g.nodes.push(GraphNode {
                                    path: String::new(),
                                    label: l.target.clone(),
                                    degree: 0,
                                    group: String::new(),
                                    ghost: true,
                                });
                                ghosts.len() - 1
                            }
                        };
                        edges.push((from, usize::MAX - gi));
                        degree[from] += 1;
                    }
                }
            }
        }

        // Real notes come after the ghosts in the node list, so ghost indices stay put.
        let ghost_count = g.nodes.len();
        for (i, n) in self.notes.iter().enumerate() {
            g.nodes.push(GraphNode {
                path: n.path.to_string_lossy().to_string(),
                label: n.title.clone(),
                degree: degree[i],
                // A cluster is a colour, and the first tag is the best one-word answer to
                // "what is this note about". Folder is the fallback for an untagged vault.
                group: n.tags.first().cloned().unwrap_or_else(|| {
                    n.path
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "·".into())
                }),
                ghost: false,
            });
        }
        for (i, node) in g.nodes.iter_mut().enumerate().take(ghost_count) {
            node.degree = edges.iter().filter(|(_, t)| *t == usize::MAX - i).count() as u16;
        }

        let idx_of = |x: usize| -> u16 {
            if x >= usize::MAX - ghost_count {
                (usize::MAX - x) as u16
            } else {
                (x + ghost_count) as u16
            }
        };
        g.edges = edges.into_iter().map(|(a, b)| (idx_of(a), idx_of(b))).collect();
        g.edges.sort_unstable();
        g.edges.dedup();
        g
    }

    /// Rebuild the name maps and backlinks from `notes`.
    ///
    /// Cheap and total rather than incremental: it is a few thousand hash inserts over data
    /// already in memory, and a link whose *target* was renamed has to be reconsidered even
    /// though the file holding it never changed.
    fn reindex(&mut self) {
        self.by_stem.clear();
        self.by_alias.clear();
        self.backlinks.clear();
        self.unresolved.clear();

        for (i, n) in self.notes.iter().enumerate() {
            self.by_stem.insert(n.stem().to_lowercase(), i);
        }
        for (i, n) in self.notes.iter().enumerate() {
            for a in &n.aliases {
                self.by_alias.entry(a.to_lowercase()).or_insert(i);
            }
        }

        let mut edges: Vec<(NoteId, NoteId)> = Vec::new();
        let mut missing: Vec<(NoteId, String)> = Vec::new();
        for (from, n) in self.notes.iter().enumerate() {
            for l in &n.links {
                if l.target.is_empty() {
                    continue; // a same-note heading link
                }
                match self.resolve(&l.target) {
                    Some(to) if to != from => edges.push((from, to)),
                    Some(_) => {}
                    None => missing.push((from, l.target.clone())),
                }
            }
        }
        for (from, to) in edges {
            let list = self.backlinks.entry(to).or_default();
            if !list.contains(&from) {
                list.push(from);
            }
        }
        self.unresolved = missing;
    }

    /// Walk the vault and reparse whatever changed.
    ///
    /// Returns true when anything moved, so the caller can mark snapshots dirty. `budget`
    /// caps files parsed per call: this runs on the same tick as pane pumping, and a cold
    /// five-thousand-note vault must not stall the terminal to answer a question nobody
    /// asked yet.
    pub fn refresh(&mut self, budget: usize) -> bool {
        let Ok(found) = walk(&self.root) else { return false };

        let mut changed = false;
        let mut seen: Vec<PathBuf> = Vec::with_capacity(found.len());
        let mut parsed = 0usize;

        for (path, mtime, size) in found {
            let rel = path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
            seen.push(rel.clone());
            let existing = self.notes.iter().position(|n| n.path == rel);
            if let Some(i) = existing {
                if self.notes[i].mtime == mtime && self.notes[i].size == size {
                    continue; // unchanged
                }
            }
            if parsed >= budget {
                continue; // the rest can wait for the next pass
            }
            parsed += 1;
            changed = true;
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let stem = rel.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let mut note = parse(&text, &stem);
            note.path = rel;
            note.mtime = mtime;
            note.size = size;
            match existing {
                Some(i) => self.notes[i] = note,
                None => self.notes.push(note),
            }
        }

        // Deletions. Done every pass regardless of budget: a note that is gone should not
        // keep answering searches until the queue drains.
        let before = self.notes.len();
        self.notes.retain(|n| seen.contains(&n.path));
        changed |= self.notes.len() != before;

        if changed {
            self.reindex();
        }
        changed
    }
}

/// Every markdown file under `root`, with its mtime and size.
fn walk(root: &Path) -> std::io::Result<Vec<(PathBuf, u64, u64)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && path.is_dir() {
                continue;
            }
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                out.push((path, mtime, meta.len()));
            }
        }
    }
    Ok(out)
}

/// Where a space's notes live, if anywhere.
///
/// Prefers a real Obsidian vault — a directory containing `.obsidian/` — so pointing horde
/// at a project that already has one just works. Otherwise the configured directory, which
/// is tracked, human-owned content: `.horde/` is deliberately not an option, being the one
/// directory in a repo that is excluded from git on purpose.
pub fn locate(cwd: &Path, dir: &str) -> Option<PathBuf> {
    // The directory itself, or the one named in config. Deliberately *not* a scan of the
    // children: adopting any vault that happens to sit under the directory you opened is a
    // surprise, and a bad one — open a space on your home directory and horde would index
    // whichever vault it found first. Point a space at a vault to use it, or name one in
    // `vault.dir`; anything else is the home vault's job.
    if is_vault(cwd) {
        return Some(cwd.to_path_buf());
    }
    let configured = cwd.join(dir);
    (configured.is_dir() && (is_vault(&configured) || configured.exists()))
        .then_some(configured)
}

/// Join a vault-relative path, refusing anything that climbs out of it.
///
/// The one place a path from outside becomes a path horde writes to, so it is the one place
/// that has to care: `../../.ssh/authorized_keys` is a note title an agent could send, and
/// "write a note" must never be a way to write anything else.
pub fn safe_join(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    use std::path::Component;
    let rel = Path::new(rel);
    if rel.is_absolute() {
        anyhow::bail!("note paths are relative to the vault");
    }
    let mut out = root.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => anyhow::bail!("note paths cannot leave the vault"),
        }
    }
    Ok(out)
}

/// A directory that has declared itself a vault.
///
/// Two markers, and horde writes only one of them. `.obsidian/` means a vault somebody
/// already keeps, and finding it is how an existing one is adopted without being asked.
/// `MARKER` is the one horde creates, so a directory it set up is recognised on sight
/// without needing to be named in config — the same trick, for the same reason.
pub fn is_vault(dir: &Path) -> bool {
    dir.join(".obsidian").is_dir() || dir.join(MARKER).exists()
}

/// What horde drops in a directory to mark it as a vault of its own.
pub const MARKER: &str = ".horde-vault";

/// Create a vault: the directory, its marker, and something to read inside it.
///
/// Idempotent, so running it on an existing vault adopts it rather than complaining or
/// overwriting anything. Returns whether it made something new.
pub fn init(root: &Path) -> std::io::Result<bool> {
    let fresh = !is_vault(root);
    std::fs::create_dir_all(root)?;
    if !root.join(MARKER).exists() && !root.join(".obsidian").is_dir() {
        std::fs::write(
            root.join(MARKER),
            "# horde vault\n\nThis file marks the directory as a vault, so horde finds it\n\
             without being told where it is. Plain markdown lives alongside it; delete this\n\
             and horde stops treating the directory as one.\n",
        )?;
    }
    // A vault with nothing in it looks broken rather than new.
    let welcome = root.join("Welcome.md");
    if fresh && !welcome.exists() {
        std::fs::write(
            &welcome,
            "# Welcome\n\n\
             This is a horde vault: plain markdown files, nothing else.\n\n\
             - Link notes with `[[double brackets]]`. A link to a note that does not exist\n\
               yet is fine — it shows on the graph as a note waiting to be written.\n\
             - Tag them in frontmatter: `tags: [one, two]`.\n\
             - Everything here opens in any editor, and in Obsidian, unchanged.\n",
        )?;
    }
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the traps found by reading a real vault -----------------------------

    /// Inside a markdown table an alias pipe must be escaped, and every one of the 31 such
    /// links in the reference vault would otherwise resolve to a target ending in `\` — a
    /// broken link invented by the parser, in the place a table hides it best.
    #[test]
    fn an_alias_pipe_escaped_for_a_table_is_still_an_alias() {
        let (target, heading, alias) = split_target(r"Week 01 — Foundations\|1");
        assert_eq!(target, "Week 01 — Foundations");
        assert_eq!(alias.as_deref(), Some("1"));
        assert_eq!(heading, None);
    }

    /// A dot in a title is not a file extension. The reference vault contains exactly one
    /// note that proves it, and suffix-parsing would send its every link to a ghost.
    #[test]
    fn a_dot_in_a_title_is_not_an_extension() {
        assert_eq!(target_stem("Item Spec 5.x (Omni) Migration Guide"), "Item Spec 5.x (Omni) Migration Guide");
        assert_eq!(target_stem("Dev.base"), "Dev");
        assert_eq!(target_stem("TAW Agentic Layer Master Plan.canvas"), "TAW Agentic Layer Master Plan");
        assert_eq!(target_stem("folder/note"), "note", "path links resolve on the last part");
    }

    /// The four forms that actually occur, in the proportions they occur in: 65% plain, 34%
    /// aliased, a handful of heading links, and same-note links with no target at all.
    #[test]
    fn the_link_forms_that_occur_in_a_real_vault_all_parse() {
        let links = scan_links(
            "see [[Plain Note]] and [[Other Note|call it this]]\n\
             and [[Ref#Some Heading]] and [[#Local Heading]]\n\
             and an embed ![[Dev.base]]\n",
        );
        assert_eq!(links.len(), 5, "{links:?}");
        assert_eq!(links[0], Link { target: "Plain Note".into(), heading: None, embed: false });
        assert_eq!(links[1].target, "Other Note", "the alias is not part of the target");
        assert_eq!(links[2], Link { target: "Ref".into(), heading: Some("Some Heading".into()), embed: false });
        assert_eq!(links[3].target, "", "a same-note link has no target");
        assert!(links[4].embed, "`![[` is an embed");
    }

    /// Block references do not occur in the reference vault at all, so they are declared
    /// unsupported rather than half-implemented. Parsing one as a heading link is the
    /// honest failure: it resolves to the right *note*, and nothing pretends to find a block.
    #[test]
    fn a_block_reference_resolves_to_its_note_and_claims_nothing_more() {
        let (target, heading, _) = split_target("Some Note#^abc123");
        assert_eq!(target, "Some Note");
        assert_eq!(heading.as_deref(), Some("^abc123"));
    }

    // -- parsing ------------------------------------------------------------

    #[test]
    fn the_title_is_the_first_h1_and_falls_back_to_the_filename() {
        let n = parse("# Real Title\n\nbody\n", "file-stem");
        assert_eq!(n.title, "Real Title");
        assert_eq!(parse("no heading here\n", "file-stem").title, "file-stem");
        assert_eq!(
            parse("## Not H1\n\n# Actual\n", "stem").title,
            "Actual",
            "an h2 above the h1 does not win"
        );
    }

    /// Both frontmatter shapes that occur: the inline array 165 of 170 notes use, and the
    /// `-` list. Not a YAML parser, and honest about it.
    #[test]
    fn frontmatter_tags_and_aliases_parse_in_both_shapes() {
        let inline = parse("---\naliases: [horde-full]\ntags: [taw, dev]\nstatus: active\n---\n\n# T\n", "s");
        assert_eq!(inline.tags, vec!["dev", "taw"]);
        assert_eq!(inline.aliases, vec!["horde-full"]);

        let listed = parse("---\ntags:\n  - one\n  - two/nested\naliases:\n  - Another Name\n---\n\n# T\n", "s");
        assert_eq!(listed.tags, vec!["one", "two/nested"]);
        assert_eq!(listed.aliases, vec!["Another Name"]);
    }

    /// A fenced block full of wikilinks is documentation *about* links. Counting them would
    /// have put four ghost nodes on the graph from this plan's own notes.
    #[test]
    fn links_and_tags_inside_code_are_not_links_or_tags() {
        let n = parse(
            "# T\n\nreal [[Actual Note]]\n\n```\n[[Not A Link]]\n#not-a-tag\n```\n\nand `[[Inline Code]]` too\n",
            "s",
        );
        let targets: Vec<&str> = n.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["Actual Note"], "{:?}", n.links);
        assert!(!n.tags.iter().any(|t| t == "not-a-tag"), "{:?}", n.tags);
    }

    /// `[[#Module C]]` is a link to a heading, not a tag called Module — the exact false
    /// positive a naive scan produces on the reference vault.
    #[test]
    fn a_heading_link_is_not_mistaken_for_an_inline_tag() {
        let n = parse("# T\n\nsee [[#Module C: the ask]] below\n\nand a real #review tag\n", "s");
        assert_eq!(n.tags, vec!["review"], "{:?}", n.tags);
    }

    // -- the index -----------------------------------------------------------

    fn index_of(notes: &[(&str, &str)]) -> Index {
        let mut idx = Index::new(PathBuf::from("/vault"));
        idx.notes = notes
            .iter()
            .map(|(name, text)| {
                let mut n = parse(text, name);
                n.path = PathBuf::from(format!("{name}.md"));
                n
            })
            .collect();
        idx.reindex();
        idx
    }

    #[test]
    fn a_link_makes_a_backlink_on_the_note_it_points_at() {
        let idx = index_of(&[
            ("Home", "# Home\n\nsee [[Project]] and [[Project|the project]]\n"),
            ("Project", "# Project\n"),
        ]);
        let project = idx.resolve("Project").unwrap();
        assert_eq!(idx.backlinks(project).len(), 1, "two links from one note is still one backlink");
        assert_eq!(idx.note(idx.backlinks(project)[0]).unwrap().title, "Home");
    }

    /// Resolution is case-insensitive and filename-first: an alias that happens to match a
    /// real note's name must lose to the file, or renaming a note could silently redirect
    /// every link in the vault.
    #[test]
    fn resolution_prefers_a_filename_over_an_alias_and_ignores_case() {
        let idx = index_of(&[
            ("Real Note", "# Real Note\n"),
            ("Decoy", "---\naliases: [Real Note]\n---\n\n# Decoy\n"),
        ]);
        let real = idx.resolve("Real Note").unwrap();
        assert_eq!(idx.note(real).unwrap().title, "Real Note");
        assert_eq!(idx.resolve("rEaL nOtE"), Some(real), "case does not matter");
        assert!(idx.resolve("Decoy").is_some());
    }

    /// A link to nothing is a ghost node, not an error and not silence: it is how the graph
    /// shows a note somebody meant to write.
    #[test]
    fn a_link_to_a_note_that_does_not_exist_becomes_a_ghost() {
        let idx = index_of(&[("Home", "# Home\n\nsee [[Never Written]]\n")]);
        assert_eq!(idx.unresolved.len(), 1);
        assert_eq!(idx.unresolved[0].1, "Never Written");
    }

    /// The graph carries what to draw and nothing about where. Ghosts are nodes too: an
    /// unwritten note is the shape of what a vault is missing, and several links to the same
    /// missing name are one ghost rather than three.
    #[test]
    fn the_graph_has_a_node_per_note_and_one_ghost_per_missing_target() {
        let idx = index_of(&[
            ("Home", "---\ntags: [moc]\n---\n\n# Home\n\n[[Project]] and [[Unwritten]]\n"),
            ("Project", "# Project\n\nback to [[Home]] and also [[Unwritten]]\n"),
        ]);
        let g = idx.graph();

        let ghosts: Vec<&str> =
            g.nodes.iter().filter(|n| n.ghost).map(|n| n.label.as_str()).collect();
        assert_eq!(ghosts, vec!["Unwritten"], "two links to it, one ghost");
        assert_eq!(g.nodes.iter().filter(|n| !n.ghost).count(), 2, "a node per real note");

        let home = g.nodes.iter().find(|n| n.label == "Home").unwrap();
        assert_eq!(home.group, "moc", "the first tag is the cluster");
        assert_eq!(home.degree, 3, "two out, one in");

        // Every edge index is in range, which is the thing an index-based wire format can
        // get quietly wrong.
        for (a, b) in &g.edges {
            assert!((*a as usize) < g.nodes.len() && (*b as usize) < g.nodes.len(), "{a},{b}");
        }
        assert_eq!(g.edges.len(), 4, "home->project, project->home, and two to the ghost");
    }

    /// An untagged vault still clusters: the folder is the fallback answer to "what is this
    /// note about", and a graph with one colour is a graph with no information in it.
    #[test]
    fn an_untagged_note_is_grouped_by_its_folder() {
        let mut idx = Index::new(PathBuf::from("/v"));
        let mut n = parse("# Deep\n", "Deep");
        n.path = PathBuf::from("10 Projects/Deep.md");
        idx.notes = vec![n];
        idx.reindex();
        assert_eq!(idx.graph().nodes[0].group, "10 Projects");
    }

    #[test]
    fn search_ranks_an_exact_name_above_a_substring_and_a_tag() {
        let idx = index_of(&[
            ("Notes About Horde", "# Notes About Horde\n"),
            ("Horde", "# Horde\n"),
            ("Unrelated", "---\ntags: [horde]\n---\n\n# Unrelated\n"),
        ]);
        let hits: Vec<&str> =
            idx.search("horde").iter().map(|i| idx.note(*i).unwrap().title.as_str()).collect();
        assert_eq!(hits, vec!["Horde", "Notes About Horde", "Unrelated"], "exact, then substring, then tag");
    }

    // -- refresh against a real directory -------------------------------------

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horde-vault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_note_written_edited_and_deleted_is_picked_up_each_time() {
        let dir = tmpdir("lifecycle");
        std::fs::write(dir.join("one.md"), "# One\n\nlinks to [[two]]\n").unwrap();
        std::fs::write(dir.join("two.md"), "# Two\n").unwrap();

        let mut idx = Index::new(dir.clone());
        assert!(idx.refresh(100), "the first pass finds them");
        assert_eq!(idx.len(), 2);
        let two = idx.resolve("two").unwrap();
        assert_eq!(idx.backlinks(two).len(), 1);

        // An edit that removes the link removes the backlink.
        std::fs::write(dir.join("one.md"), "# One\n\nnothing here now\n").unwrap();
        // mtime has a resolution; make the change unmistakable.
        std::fs::write(dir.join("one.md"), "# One\n\nnothing here now, and longer\n").unwrap();
        assert!(idx.refresh(100));
        let two = idx.resolve("two").unwrap();
        assert!(idx.backlinks(two).is_empty(), "the backlink went with the link");

        std::fs::remove_file(dir.join("two.md")).unwrap();
        assert!(idx.refresh(100));
        assert_eq!(idx.len(), 1, "a deleted note leaves the index");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The budget is what keeps a cold vault off the tick loop's back. It has to bound the
    /// work of one pass without ever losing a file.
    #[test]
    fn a_large_vault_indexes_across_several_passes_without_dropping_a_note() {
        let dir = tmpdir("budget");
        for i in 0..50 {
            std::fs::write(dir.join(format!("n{i}.md")), format!("# Note {i}\n")).unwrap();
        }
        let mut idx = Index::new(dir.clone());
        idx.refresh(10);
        assert_eq!(idx.len(), 10, "one pass parses only its budget");
        for _ in 0..10 {
            idx.refresh(10);
        }
        assert_eq!(idx.len(), 50, "and the rest arrive on later passes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "Write a note" must never be a way to write anything else. A note title arrives from
    /// a text field and, one day, from an agent.
    #[test]
    fn a_note_path_cannot_climb_out_of_its_vault() {
        let root = Path::new("/vault");
        assert!(safe_join(root, "Ideas/one.md").is_ok());
        assert!(safe_join(root, "./one.md").is_ok());
        for bad in ["../escape.md", "a/../../escape.md", "/etc/passwd", "~/notes/x.md"] {
            let r = safe_join(root, bad);
            // `~` is not special to a filesystem, so it is a directory name, not an escape.
            if bad.starts_with('~') {
                assert!(r.unwrap().starts_with(root));
            } else {
                assert!(r.is_err(), "{bad} should be refused");
            }
        }
    }

    /// Setting up a vault twice is something a walkthrough will do, and it must adopt rather
    /// than complain or overwrite.
    #[test]
    fn making_a_vault_is_safe_to_repeat() {
        let dir = tmpdir("init");
        assert!(init(&dir).unwrap(), "the first time makes one");
        assert!(is_vault(&dir), "and it is recognisable afterwards");
        std::fs::write(dir.join("Welcome.md"), "# mine now\n").unwrap();

        assert!(!init(&dir).unwrap(), "the second time adopts it");
        assert_eq!(
            std::fs::read_to_string(dir.join("Welcome.md")).unwrap(),
            "# mine now\n",
            "and never overwrites what is already there"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A vault horde made has to be found the same way an Obsidian one is: by looking, not
    /// by being told where it is in config.
    #[test]
    fn a_vault_horde_made_is_found_without_being_configured() {
        let dir = tmpdir("marker");
        let vault = dir.join("anything");
        init(&vault).unwrap();
        assert_eq!(
            locate(&vault, "notes"),
            Some(vault.clone()),
            "the marker is enough, whatever the directory is called"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_obsidian_vault_is_adopted_and_a_plain_directory_is_not() {
        let dir = tmpdir("locate");
        assert_eq!(locate(&dir, "notes"), None, "nothing to find yet");

        std::fs::create_dir_all(dir.join("notes")).unwrap();
        assert_eq!(locate(&dir, "notes"), Some(dir.join("notes")), "the configured directory");

        // A vault *inside* the directory is not adopted: you opened this directory, not
        // that one, and guessing which of several is meant is how horde would end up
        // indexing something nobody asked about.
        std::fs::create_dir_all(dir.join("Brain/.obsidian")).unwrap();
        assert_eq!(locate(&dir, "notes"), Some(dir.join("notes")), "still the configured one");
        assert_eq!(
            locate(&dir.join("Brain"), "notes"),
            Some(dir.join("Brain")),
            "but opening it directly finds it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
