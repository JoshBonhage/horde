//! The note browser: a project's vault, full screen.
//!
//! The same shape as the roster — a pure content function, a hit list, a `Mode` — over data
//! that arrives from the daemon rather than from the snapshot. Notes are unbounded and
//! change often, so they travel as an answer to a question instead of riding every frame.
//!
//! Two panes: the list on the left, and what the cursor is on to the right. The right side
//! is where a vault stops being a directory and starts being a graph, so it leads with
//! backlinks — the thing a file manager cannot tell you.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{color, fill, put_line, truncate};
use crate::proto::{NoteLine, VaultReply};
use crate::theme::Theme;

/// A row of the list, and what it points at.
///
/// `depth` and `folder` are what turn a flat list of paths into the tree a vault actually
/// is. A note lives somewhere, and "somewhere" is half of how people remember which note
/// they want.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub path: String,
    pub title: String,
    pub tags: Vec<String>,
    pub backlinks: usize,
    /// How deep to indent it.
    pub depth: usize,
    /// A folder heading rather than a note.
    pub folder: bool,
    /// For a folder, whether its contents are showing.
    pub open: bool,
}

/// The notes to show, given what has been typed.
///
/// The daemon has already ranked and filtered by the time this runs — the query lives here
/// too so that typing feels instant rather than waiting for a round trip on every keystroke.
pub fn rows(vault: Option<&VaultReply>, query: &str) -> Vec<Row> {
    let Some(v) = vault else { return Vec::new() };
    let q = query.trim().to_lowercase();
    let mut hits: Vec<&NoteLine> = v
        .notes
        .iter()
        .filter(|n| {
            q.is_empty()
                || n.title.to_lowercase().contains(&q)
                || n.path.to_lowercase().contains(&q)
                || n.tags.iter().any(|t| t.to_lowercase().contains(&q))
        })
        .collect();
    // By path, so a folder's notes arrive together and in an order that does not move.
    hits.sort_by_key(|n| n.path.to_lowercase());

    let mut out = Vec::new();
    let mut folder: Vec<String> = Vec::new();
    for n in hits {
        let parts: Vec<String> =
            n.path.split('/').map(|s| s.to_string()).collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        // Emit any folder heading this note has just walked into.
        let common = folder
            .iter()
            .zip(dirs)
            .take_while(|(a, b)| a == b)
            .count();
        folder.truncate(common);
        for (i, d) in dirs.iter().enumerate().skip(common) {
            folder.push(d.clone());
            out.push(Row {
                path: String::new(),
                title: d.clone(),
                tags: Vec::new(),
                backlinks: 0,
                depth: i,
                folder: true,
                open: true,
            });
        }
        out.push(Row {
            path: n.path.clone(),
            title: n.title.clone(),
            tags: n.tags.clone(),
            backlinks: n.backlinks,
            depth: dirs.len(),
            folder: false,
            open: false,
        });
    }
    out
}

/// A project's files as a tree: directories first, then files, each level sorted.
///
/// Built as a tree rather than sorted as paths. A flat sort puts `Cargo.toml` in the middle
/// of the folder list — correct alphabetically, wrong in every other way, because a person
/// reading a project reads its shape first and its filenames second.
///
/// Folders are closed until asked for. Ninety files arriving at once is not a breakdown of a
/// project, it is a wall with the project behind it. A query overrides that and shows every
/// match wherever it lives, because when you are searching the shape is not what you want.
pub fn file_rows(
    files: Option<&crate::proto::FileList>,
    query: &str,
    open: &std::collections::HashSet<String>,
) -> Vec<Row> {
    let Some(f) = files else { return Vec::new() };
    let q = query.trim().to_lowercase();
    let searching = !q.is_empty();

    let paths: Vec<&String> =
        f.files.iter().filter(|p| !searching || p.to_lowercase().contains(&q)).collect();

    let mut out = Vec::new();
    walk_level(&paths, "", 0, open, searching, &mut out);
    out
}

/// One level of the tree: its directories, then its files.
fn walk_level(
    paths: &[&String],
    prefix: &str,
    depth: usize,
    open: &std::collections::HashSet<String>,
    searching: bool,
    out: &mut Vec<Row>,
) {
    let mut dirs: Vec<String> = Vec::new();
    let mut here: Vec<&String> = Vec::new();

    for p in paths {
        let rest = match p.strip_prefix(prefix) {
            Some(r) => r.trim_start_matches('/'),
            None => continue,
        };
        match rest.split_once('/') {
            Some((dir, _)) => {
                let dir = dir.to_string();
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
            None => here.push(p),
        }
    }
    dirs.sort_by_key(|d| d.to_lowercase());
    here.sort_by_key(|p| p.to_lowercase());

    for dir in dirs {
        let full = if prefix.is_empty() { dir.clone() } else { format!("{prefix}/{dir}") };
        // While searching, everything with a match inside it is open: a folder collapsed
        // over the thing you are looking for is a folder lying to you.
        let is_open = searching || open.contains(&full);
        out.push(Row {
            path: full.clone(),
            title: dir,
            tags: Vec::new(),
            backlinks: 0,
            depth,
            folder: true,
            open: is_open,
        });
        if is_open {
            walk_level(paths, &full, depth + 1, open, searching, out);
        }
    }
    for p in here {
        out.push(Row {
            path: (*p).clone(),
            title: p.rsplit('/').next().unwrap_or(p).to_string(),
            tags: Vec::new(),
            backlinks: 0,
            depth,
            folder: false,
            open: false,
        });
    }
}

/// The vault as a tree, beside a note rather than instead of one.
///
/// The same rows the browser lists, drawn narrow: no search field, no preview, no backlink
/// counts. What it adds is the one thing the browser cannot — telling you where the note you
/// are *in* sits among the others, which is the question you have while writing rather than
/// while looking.
///
/// Returns `(y, path)` for every note it drew, so a click opens the note under the pointer.
/// Folders are scenery here: the browser is where a vault is rearranged.
pub fn draw_tree(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    vault: Option<&VaultReply>,
    rows_in: &[Row],
    here: &str,
    scroll: usize,
) -> Vec<(u16, String)> {
    let mut hits = Vec::new();
    fill(buf, area, theme.ui.panel_bg);
    if area.width < 12 || area.height < 3 {
        return hits;
    }
    // A rule down the left, so the tree reads as beside the note rather than after it.
    for y in area.y..area.y + area.height {
        put_line(
            buf,
            area.x,
            y,
            1,
            Line::from(Span::styled(
                "│",
                Style::default().fg(color(theme.ui.border)).bg(color(theme.ui.panel_bg)),
            )),
        );
    }
    let inner = TRect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(3),
        height: area.height,
    };

    let n = vault.map(|v| v.notes.len()).unwrap_or(0);
    put_line(
        buf,
        inner.x,
        inner.y,
        inner.width,
        Line::from(Span::styled(
            format!("VAULT  {n}"),
            Style::default()
                .fg(color(theme.ui.text_faint))
                .bg(color(theme.ui.panel_bg))
                .add_modifier(Modifier::BOLD),
        )),
    );

    let room = (inner.height as usize).saturating_sub(2);
    for (i, r) in rows_in.iter().skip(scroll).take(room).enumerate() {
        let y = inner.y + 2 + i as u16;
        let indent = "  ".repeat(r.depth.min(4));
        if r.folder {
            put_line(
                buf,
                inner.x,
                y,
                inner.width,
                Line::from(Span::styled(
                    format!("{indent}{}/", truncate(&r.title, inner.width as usize)),
                    Style::default().fg(color(theme.ui.text_dim)).bg(color(theme.ui.panel_bg)),
                )),
            );
            continue;
        }
        // The note being edited is marked, not just highlighted: a tree whose current row is
        // only a background colour is unreadable the moment the terminal loses focus.
        let current = r.path == here;
        let (mark, fg) = match current {
            true => ("▸ ", theme.ui.accent),
            false => ("  ", theme.ui.text),
        };
        if current {
            fill(buf, TRect { x: area.x + 1, y, width: area.width - 1, height: 1 }, theme.ui.selection);
        }
        let bg = if current { theme.ui.selection } else { theme.ui.panel_bg };
        let room = (inner.width as usize).saturating_sub(indent.len() + 2);
        put_line(
            buf,
            inner.x,
            y,
            inner.width,
            Line::from(vec![
                Span::styled(
                    format!("{indent}{mark}"),
                    Style::default().fg(color(theme.ui.accent)).bg(color(bg)),
                ),
                Span::styled(
                    truncate(&r.title, room),
                    Style::default().fg(color(fg)).bg(color(bg)),
                ),
            ]),
        );
        hits.push((y, r.path.clone()));
    }
    hits
}

/// How wide the tree is, and whether there is room for it at all.
///
/// `None` on a terminal too narrow to carry both, because a note squeezed into forty columns
/// to make room for a list of its neighbours is the wrong thing to protect. The note is what
/// you came for; the tree is context.
pub fn tree_width(area: TRect) -> Option<u16> {
    const TREE: u16 = 30;
    const NOTE_FLOOR: u16 = 56;
    (area.width >= TREE + NOTE_FLOOR).then_some(TREE)
}

/// Draw the project's files. Returns `(y, row index)` hits for the mouse.
pub fn draw_files(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    files: Option<&crate::proto::FileList>,
    rows_in: &[Row],
    query: &str,
    sel: usize,
) -> Vec<(u16, usize)> {
    fill(buf, area, theme.ui.bg);
    let mut hits = Vec::new();
    if area.height < 6 || area.width < 30 {
        return hits;
    }
    let root = files.map(|f| f.root.as_str()).unwrap_or("");
    let count = files.map(|f| f.files.len()).unwrap_or(0);
    let more = files.is_some_and(|f| f.truncated);
    let header = format!(
        "{}  ·  {count} files{}",
        super::statusbar::shorten_home(root),
        if more { " (more than horde will list)" } else { "" }
    );
    put_line(
        buf,
        area.x + 2,
        area.y,
        area.width.saturating_sub(4),
        Line::from(Span::styled(
            header,
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
        )),
    );
    put_line(
        buf,
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        Line::from(vec![
            Span::styled("find ".to_string(), Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg))),
            Span::styled(query.to_string(), Style::default().fg(color(theme.ui.text)).bg(color(theme.ui.bg))),
            Span::styled("█".to_string(), Style::default().fg(color(theme.ui.accent)).bg(color(theme.ui.bg))),
        ]),
    );

    let top = area.y + 3;
    let height = area.height.saturating_sub(4);
    let first_row = sel.saturating_sub(height.saturating_sub(1) as usize);
    let w = area.width.saturating_sub(4);

    for (i, row) in rows_in.iter().enumerate().skip(first_row).take(height as usize) {
        let y = top + (i - first_row) as u16;
        let selected = i == sel;
        if selected {
            fill(buf, TRect { x: area.x + 1, y, width: w, height: 1 }, theme.ui.selection);
        }
        let base =
            Style::default().bg(color(if selected { theme.ui.selection } else { theme.ui.bg }));
        // One marker column, then two spaces per level. Files line up under the folder
        // they are in rather than under its arrow, which is what makes a tree readable at a
        // glance instead of something you have to count.
        let indent = "  ".repeat(row.depth);
        let used = 2 + indent.chars().count() as u16;
        let spans = if row.folder {
            vec![
                Span::styled(
                    format!("{indent}{} ", if row.open { "▾" } else { "▸" }),
                    base.fg(color(theme.ui.text_faint)),
                ),
                Span::styled(
                    truncate(&row.title, w.saturating_sub(used) as usize),
                    base.fg(color(theme.ui.accent_alt)).add_modifier(Modifier::BOLD),
                ),
                Span::styled("/".to_string(), base.fg(color(theme.ui.text_faint))),
            ]
        } else {
            vec![
                Span::styled(format!("{indent}  "), base),
                Span::styled(
                    truncate(&row.title, w.saturating_sub(used) as usize),
                    base.fg(color(if selected { theme.ui.text } else { theme.ui.text_dim })),
                ),
            ]
        };
        put_line(buf, area.x + 2, y, w, Line::from(spans));
        hits.push((y, i));
    }

    put_line(
        buf,
        area.x + 2,
        area.y + area.height.saturating_sub(1),
        w,
        Line::from(Span::styled(
            "enter open   ctrl+p as a pane   ←→ fold   ctrl+t terminal   esc back"
                .to_string(),
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
        )),
    );
    hits
}

/// The next selectable row from `from`, stepping by `dir`.
///
/// Folders are scenery: they say where a note lives, and there is nothing to do to one, so
/// the cursor walks past them rather than stopping on a row where `enter` means nothing.
pub fn step(rows: &[Row], from: usize, dir: isize) -> usize {
    let mut i = from as isize;
    loop {
        let next = i + dir;
        if next < 0 || next as usize >= rows.len() {
            return from;
        }
        i = next;
        if !rows[i as usize].folder {
            return i as usize;
        }
    }
}

/// The first row a cursor may sit on.
pub fn first(rows: &[Row]) -> usize {
    rows.iter().position(|r| !r.folder).unwrap_or(0)
}

/// Draw the browser. Returns `(y, row index)` hits for the mouse.
pub fn draw(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    vault: Option<&VaultReply>,
    rows_in: &[Row],
    query: &str,
    sel: usize,
) -> Vec<(u16, usize)> {
    fill(buf, area, theme.ui.bg);
    let mut hits = Vec::new();
    if area.height < 6 || area.width < 40 {
        return hits;
    }

    let root = vault.map(|v| v.root.as_str()).unwrap_or("");
    let header = if root.is_empty() {
        "no vault in this project — see vault.dir in the config".to_string()
    } else {
        format!("{}  ·  {} notes", super::statusbar::shorten_home(root), vault.map(|v| v.notes.len()).unwrap_or(0))
    };
    put_line(
        buf,
        area.x + 2,
        area.y,
        area.width.saturating_sub(4),
        Line::from(Span::styled(
            header,
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
        )),
    );

    // The query line, with a block cursor so it is obviously a field you are typing into.
    put_line(
        buf,
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        Line::from(vec![
            Span::styled(
                "search ".to_string(),
                Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
            ),
            Span::styled(
                query.to_string(),
                Style::default().fg(color(theme.ui.text)).bg(color(theme.ui.bg)),
            ),
            Span::styled(
                "█".to_string(),
                Style::default().fg(color(theme.ui.accent)).bg(color(theme.ui.bg)),
            ),
        ]),
    );

    let list_w = (area.width / 2).clamp(24, 60);
    let top = area.y + 3;
    let height = area.height.saturating_sub(4);

    // Keep the cursor on screen without moving it: scrolling is a consequence of where the
    // selection is, never a separate thing to steer.
    let first = sel.saturating_sub(height.saturating_sub(1) as usize);

    for (i, row) in rows_in.iter().enumerate().skip(first).take(height as usize) {
        let y = top + (i - first) as u16;
        let selected = i == sel;
        if selected {
            fill(buf, TRect { x: area.x + 1, y, width: list_w, height: 1 }, theme.ui.selection);
        }
        let base =
            Style::default().bg(color(if selected { theme.ui.selection } else { theme.ui.bg }));
        let indent = "  ".repeat(row.depth);
        let spans = if row.folder {
            vec![
                Span::styled(format!("{indent}▾ "), base.fg(color(theme.ui.text_faint))),
                Span::styled(
                    truncate(&row.title, list_w.saturating_sub(4) as usize),
                    base.fg(color(theme.ui.accent_alt)),
                ),
            ]
        } else {
            let mark = if row.backlinks > 0 { "←" } else { " " };
            vec![
                Span::styled(format!("{indent}{mark} "), base.fg(color(theme.ui.text_faint))),
                Span::styled(
                    truncate(&row.title, list_w.saturating_sub(4 + indent.len() as u16) as usize),
                    base.fg(color(if selected { theme.ui.text } else { theme.ui.text_dim })),
                ),
            ]
        };
        put_line(buf, area.x + 2, y, list_w, Line::from(spans));
        hits.push((y, i));
    }

    if rows_in.is_empty() {
        put_line(
            buf,
            area.x + 2,
            top,
            area.width.saturating_sub(4),
            Line::from(Span::styled(
                if root.is_empty() { "".to_string() } else { "no notes match".to_string() },
                Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
            )),
        );
    }

    // The detail side.
    if let Some(row) = rows_in.get(sel) {
        let x = area.x + list_w + 3;
        let w = area.width.saturating_sub(list_w + 5);
        let mut y = top;
        let put = |buf: &mut Buffer, y: u16, spans: Vec<Span<'static>>| {
            put_line(buf, x, y, w, Line::from(spans));
        };

        put(
            buf,
            y,
            vec![Span::styled(
                truncate(&row.title, w as usize),
                Style::default()
                    .fg(color(theme.ui.text))
                    .bg(color(theme.ui.bg))
                    .add_modifier(Modifier::BOLD),
            )],
        );
        y += 1;
        put(
            buf,
            y,
            vec![Span::styled(
                truncate(&row.path, w as usize),
                Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
            )],
        );
        y += 2;

        if !row.tags.is_empty() {
            put(
                buf,
                y,
                vec![Span::styled(
                    row.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join("  "),
                    Style::default().fg(color(theme.ui.accent_alt)).bg(color(theme.ui.bg)),
                )],
            );
            y += 2;
        }

        // Backlinks last and named plainly. "3 notes link here" is the sentence a vault can
        // answer and a directory listing cannot.
        let text = match row.backlinks {
            0 => "nothing links here yet".to_string(),
            1 => "1 note links here".to_string(),
            n => format!("{n} notes link here"),
        };
        put(
            buf,
            y,
            vec![Span::styled(
                text,
                Style::default().fg(color(theme.ui.text_dim)).bg(color(theme.ui.bg)),
            )],
        );
    }

    put_line(
        buf,
        area.x + 2,
        area.y + area.height.saturating_sub(1),
        area.width.saturating_sub(4),
        Line::from(Span::styled(
            "enter read   ctrl+e write   ctrl+p as a pane   ctrl+n new   esc close"
                .to_string(),
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
        )),
    );
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> VaultReply {
        VaultReply {
            space: 1,
            root: "/home/j/Brain".into(),
            notes: vec![
                NoteLine {
                    path: "10 Projects/Horde.md".into(),
                    title: "Horde".into(),
                    tags: vec!["dev".into(), "taw".into()],
                    mtime: 3,
                    backlinks: 2,
                },
                NoteLine {
                    path: "Vault Home.md".into(),
                    title: "Vault Home".into(),
                    tags: vec!["moc".into()],
                    mtime: 2,
                    backlinks: 0,
                },
            ],
            body: None,
            backlinks: Vec::new(),
            graph: None,
            tasks: Vec::new(),
        }
    }

    /// Typing filters on what a person can see — title, path, tag — rather than only on the
    /// title, because "which note was that" is as often answered by a folder as by a name.
    #[test]
    fn typing_filters_on_title_path_and_tag() {
        let r = reply();
        let notes = |q: &str| -> Vec<String> {
            rows(Some(&r), q).into_iter().filter(|x| !x.folder).map(|x| x.title).collect()
        };
        assert_eq!(notes("").len(), 2, "an empty query shows everything");
        assert_eq!(notes("horde"), vec!["Horde"]);
        assert_eq!(notes("projects"), vec!["Horde"], "matched on its folder");
        assert_eq!(notes("moc"), vec!["Vault Home"], "matched on a tag");
        assert!(rows(Some(&r), "nothing here").is_empty());
    }

    /// The browser has to say something useful when a project has no vault at all, since
    /// that is the state every project starts in.
    #[test]
    fn a_project_with_no_vault_says_so_rather_than_looking_broken() {
        let area = TRect::new(0, 0, 90, 20);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), None, &[], "", 0);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert!(text.contains("no vault in this project"), "{text}");
        assert!(text.contains("vault.dir"), "and points at the setting that fixes it");
    }

    /// What the right-hand side is for: the question a directory listing cannot answer.
    /// A vault is a tree, and where a note lives is half of how anyone remembers which one
    /// they want. Folders are headings the cursor walks past — there is nothing to do to
    /// one, so stopping on it would only ever be a keystroke wasted.
    #[test]
    fn notes_are_grouped_under_their_folders_and_the_cursor_skips_them() {
        let r = reply();
        let rows_in = rows(Some(&r), "");
        let folders: Vec<&str> =
            rows_in.iter().filter(|x| x.folder).map(|x| x.title.as_str()).collect();
        assert_eq!(folders, vec!["10 Projects"], "{rows_in:?}");
        let idx = rows_in.iter().position(|x| x.folder).unwrap();
        assert!(rows_in[idx + 1].depth > rows_in[idx].depth, "its notes indent under it");

        assert_eq!(first(&rows_in), idx + 1, "opening lands on a note, never a heading");
        assert_eq!(step(&rows_in, idx + 1, -1), idx + 1, "and up from the first stays put");
        let last = rows_in.len() - 1;
        assert!(!rows_in[step(&rows_in, last, 1)].folder, "stepping never lands on a folder");
    }

    #[test]
    fn the_detail_panel_leads_with_backlinks() {
        let r = reply();
        let rows_in = rows(Some(&r), "");
        let sel_note = first(&rows_in);
        let area = TRect::new(0, 0, 90, 20);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), Some(&r), &rows_in, "", sel_note);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(text.contains("2 notes link here"), "{text}");
        assert!(text.contains("#dev"), "tags are shown too");

        let mut buf2 = Buffer::empty(area);
        let other = step(&rows_in, sel_note, 1);
        draw(&mut buf2, area, &Theme::horde(), Some(&r), &rows_in, "", other);
        let text2: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf2[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(text2.contains("nothing links here yet"), "and says so plainly when there are none:\n{text2}");
    }
}
