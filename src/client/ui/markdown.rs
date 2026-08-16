//! Markdown to styled terminal lines.
//!
//! The reading view: headings that look like headings, emphasis that is emphasised, links
//! without their brackets, code on its own ground. Everything a terminal can honestly do,
//! and nothing it cannot — there is no font size here, so a heading earns its weight from
//! colour, boldness and the space around it rather than from being larger.
//!
//! Pure: text in, lines out. That is what lets it be tested as strings, and what will let
//! the same renderer move daemon-side when notes become real panes — at which point it
//! produces `proto::Row` instead of `Line`, and nothing else about it changes.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::color;
use crate::theme::Theme;

/// A rendered note, plus where its links are.
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// Link targets in reading order, with the line each sits on, so the reader can walk
    /// them without re-parsing anything.
    pub links: Vec<(usize, String)>,
    /// Pictures the terminal will draw itself: the line each starts on, where it is, and the
    /// box it wants. Their rows are left blank in `lines`, because a kitty image is not in
    /// ratatui's grid and anything written over it would half-erase it.
    pub images: Vec<Picture>,
}

/// A picture the terminal draws, and the rows reserved for it.
#[derive(Debug, Clone)]
pub struct Picture {
    pub line: usize,
    pub path: std::path::PathBuf,
    pub cols: u16,
    pub rows: u16,
}

/// Inline styling state while walking events.
#[derive(Clone, Copy, Default)]
struct Inline {
    bold: bool,
    italic: bool,
    strike: bool,
}

impl Inline {
    fn apply(&self, base: Style) -> Style {
        let mut s = base;
        if self.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }
}

/// Obsidian's callout syntax: `> [!note] Optional title`.
fn callout_kind(text: &str) -> Option<(String, String)> {
    let rest = text.trim_start().strip_prefix("[!")?;
    let (kind, after) = rest.split_once(']')?;
    Some((kind.trim().to_lowercase(), after.trim().to_string()))
}

fn callout_color(kind: &str, t: &Theme) -> crate::proto::Rgb {
    match kind {
        "warning" | "caution" | "attention" => t.ui.warn,
        "danger" | "error" | "bug" | "failure" => t.ui.error,
        "success" | "tip" | "check" | "done" => t.ui.ok,
        "question" | "help" | "faq" => t.ui.accent_alt,
        _ => t.ui.accent,
    }
}

/// The scheme a rewritten wikilink uses, so the renderer can tell one from a web link.
const WIKI_SCHEME: &str = "horde-note:";

/// Rewrite `[[wikilinks]]` as CommonMark links before parsing.
///
/// Necessary because CommonMark has no idea what a wikilink is, and does not hand one over
/// intact: `[[Some Note]]` arrives as six separate text events — `[`, `[`, `Some Note`, `]`,
/// `]` — so there is no single string to scan. Rewriting them into real links means the
/// parser does the inline work and the renderer gets a `Link` event like any other.
///
/// The destination is wrapped in angle brackets because note names contain spaces and
/// brackets: a real one in the vault this was built against is
/// `Item Spec 5.x (Omni) Migration Guide`, whose parentheses would end a bare destination
/// early and leave the rest as text.
fn rewrite_wikilinks(text: &str, code: &[std::ops::Range<usize>]) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let in_code = code.iter().any(|r| r.contains(&i));
        if !in_code && bytes[i] == b'[' && bytes.get(i + 1) == Some(&b'[') {
            // Never let a link span a line it was not written across.
            let line_end = text[i..].find('\n').map(|n| i + n).unwrap_or(bytes.len());
            if let Some(rel) = text[i..line_end].find("]]") {
                let inner = &text[i + 2..i + rel];
                let (target, heading, alias) = crate::daemon::vault::split_target(inner);
                let shown = alias.unwrap_or_else(|| {
                    if target.is_empty() {
                        heading.clone().unwrap_or_default()
                    } else {
                        target.clone()
                    }
                });
                let frag = heading.map(|h| format!("#{h}")).unwrap_or_default();
                out.push_str(&format!("[{shown}](<{WIKI_SCHEME}{target}{frag}>)"));
                i += rel + 2;
                continue;
            }
        }
        let ch_len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Byte ranges of code spans and blocks, which nothing may be rewritten inside.
fn code_ranges(text: &str, opts: Options) -> Vec<std::ops::Range<usize>> {
    Parser::new_ext(text, opts)
        .into_offset_iter()
        .filter_map(|(ev, r)| {
            matches!(ev, Event::Code(_) | Event::Start(Tag::CodeBlock(_))).then_some(r)
        })
        .collect()
}

/// Where a note is, so the pictures it embeds can be found.
///
/// `None` renders the note without them — which is what the graph's little panel wants, and
/// what anything that has text but no idea where it came from gets.
#[derive(Clone, Copy, Default)]
pub struct Where<'a> {
    /// The directory the note itself is in.
    pub dir: Option<&'a std::path::Path>,
    pub vault: Option<&'a std::path::Path>,
    /// Rows an image may take. A picture that fills the screen buries the note it is in.
    pub tall: u16,
    /// Whether the terminal draws real pixels, in which case the rows are reserved for it
    /// rather than filled with half blocks.
    ///
    /// Passed in rather than read from the environment here. A renderer that asks the
    /// environment what to do is a renderer whose output depends on which terminal the test
    /// suite happens to be running under — which is the same class of mistake as reading
    /// somebody's real notes directory during a test.
    pub pixels: bool,
}

/// Where a note lives, owned — because [`Where`] borrows and the paths have to be somewhere.
///
/// Built from the reply that carried the note, so every view that renders one resolves its
/// pictures the same way. Three places needed this and two of them had it wrong: the graph's
/// panel drew a placeholder because it never said where the note was, and the reader's *key*
/// handler re-renders to count lines for scrolling — so with images drawn but not counted,
/// scrolling past one stopped early.
#[derive(Default)]
pub struct Home {
    pub dir: Option<std::path::PathBuf>,
    pub vault: Option<std::path::PathBuf>,
}

impl Home {
    pub fn of(v: Option<&crate::proto::VaultReply>) -> Home {
        let Some(v) = v else { return Home::default() };
        let vault = std::path::PathBuf::from(&v.root);
        let dir = v
            .notes
            .first()
            .and_then(|n| std::path::Path::new(&n.path).parent())
            .map(|rel| vault.join(rel));
        Home { dir, vault: Some(vault) }
    }

    /// The borrowed form, with a limit on how many rows a picture may take.
    pub fn at(&self, tall: u16) -> Where<'_> {
        Where {
            dir: self.dir.as_deref(),
            vault: self.vault.as_deref(),
            tall,
            pixels: crate::client::kitty::supported(),
        }
    }
}

/// Render `text` to styled lines wrapped at `width` columns, knowing where the note lives —
/// which is what turns `![[shot.png]]` into the picture rather than into its filename.
///
/// There is no location-free version. Every caller that had one was either drawing a
/// placeholder where a picture belonged, or counting lines a different way from the way they
/// were drawn, and the second of those is a scroll that stops short of the end of a note.
pub fn render_in(text: &str, width: u16, theme: &Theme, at: Where<'_>) -> Rendered {
    let width = width.max(20) as usize;
    let mut out = Rendered { lines: Vec::new(), links: Vec::new(), images: Vec::new() };
    // An image's alt text is a description of the picture, so it is not read out beside one
    // that is actually on screen.
    let mut skip_alt = false;
    let mut image_rows = 0usize;

    // Frontmatter is metadata, not prose. Shown as a dim key line rather than as a wall of
    // YAML: the browser already lists tags, and a reader wants the note.
    let body = match text.strip_prefix("---\n") {
        Some(rest) => match rest.split_once("\n---") {
            Some((front, after)) => {
                let keys: Vec<String> = front
                    .lines()
                    .filter_map(|l| l.split_once(':').map(|(k, _)| k.trim().to_string()))
                    .filter(|k| !k.is_empty() && !k.starts_with('-'))
                    .collect();
                if !keys.is_empty() {
                    out.lines.push(Line::from(Span::styled(
                        format!("  {}", keys.join(" · ")),
                        Style::default().fg(color(theme.ui.text_faint)),
                    )));
                    out.lines.push(Line::from(""));
                }
                after.trim_start_matches('\n')
            }
            None => text,
        },
        None => text,
    };

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let body = rewrite_wikilinks(body, &code_ranges(body, opts));
    let body = body.as_str();

    let text_style = Style::default().fg(color(theme.ui.text));
    let mut inline = Inline::default();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut prefix = String::new();
    let mut heading: Option<HeadingLevel> = None;
    let mut in_code = false;
    let mut quote_depth = 0usize;
    let mut callout: Option<crate::proto::Rgb> = None;
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut pending_link: Option<String> = None;
    // Collects a callout marker, which the parser delivers in pieces.
    let mut marker = String::new();
    // A callout whose header is waiting for the title that follows its marker.
    let mut title_for: Option<(crate::proto::Rgb, String, String)> = None;

    // Flush the current spans as one or more wrapped lines.
    //
    // Splits *inclusively* on spaces so each word carries its own trailing separator. The
    // obvious version — split on spaces, rejoin with spaces — invents whitespace that was
    // never written: `**bold**:` renders as "bold :", because the colon arrives as its own
    // span and gets a separator it never had.
    let flush = |spans: &mut Vec<Span<'static>>, out: &mut Rendered, prefix: &str| {
        if spans.is_empty() {
            return;
        }
        let indent = " ".repeat(prefix.chars().count());
        let avail = width.saturating_sub(prefix.chars().count()).max(8);
        let mut line: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        let mut first = true;

        for span in spans.drain(..) {
            for token in span.content.split_inclusive(' ') {
                let trimmed = token.trim_end_matches(' ');
                let w = super::width(trimmed);
                // Break before a word that will not fit, never in the middle of one.
                if used + w > avail && used > 0 {
                    // The space that ended the previous line is not worth carrying to this one.
                    if let Some(last) = line.last_mut() {
                        let t = last.content.trim_end().to_string();
                        *last = Span::styled(t, last.style);
                    }
                    let mut full =
                        vec![Span::styled(if first { prefix.to_string() } else { indent.clone() }, Style::default())];
                    full.append(&mut line);
                    out.lines.push(Line::from(full));
                    first = false;
                    used = 0;
                    if trimmed.is_empty() {
                        continue;
                    }
                }
                if token.is_empty() {
                    continue;
                }
                used += super::width(token);
                line.push(Span::styled(token.to_string(), span.style));
            }
        }
        if !line.is_empty() {
            let mut full =
                vec![Span::styled(if first { prefix.to_string() } else { indent.clone() }, Style::default())];
            full.append(&mut line);
            out.lines.push(Line::from(full));
        }
    };

    for ev in Parser::new_ext(body, opts) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some(level);
                if !out.lines.is_empty() {
                    out.lines.push(Line::from(""));
                }
            }
            Event::End(TagEnd::Heading(level)) => {
                // Colour and weight carry the level, since a terminal has one font size.
                let style = match level {
                    HeadingLevel::H1 => Style::default()
                        .fg(color(theme.ui.accent))
                        .add_modifier(Modifier::BOLD),
                    HeadingLevel::H2 => Style::default()
                        .fg(color(theme.ui.accent_alt))
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default().fg(color(theme.ui.text)).add_modifier(Modifier::BOLD),
                };
                let text: String = spans.iter().map(|s| s.content.to_string()).collect();
                spans.clear();
                out.lines.push(Line::from(Span::styled(text.clone(), style)));
                // A rule under the top two levels, which is the terminal's version of a
                // bigger font: it gives the heading the room a reader uses to find it.
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    let rule = "─".repeat(super::width(&text).min(width).max(1));
                    out.lines.push(Line::from(Span::styled(
                        rule,
                        Style::default().fg(color(theme.ui.border)),
                    )));
                }
                out.lines.push(Line::from(""));
                heading = None;
            }

            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush(&mut spans, &mut out, &prefix);
                if quote_depth == 0 {
                    out.lines.push(Line::from(""));
                }
            }

            Event::Start(Tag::BlockQuote(_)) => {
                quote_depth += 1;
                prefix = format!("{}│ ", "  ".repeat(quote_depth - 1));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut spans, &mut out, &prefix);
                quote_depth = quote_depth.saturating_sub(1);
                prefix = if quote_depth == 0 { String::new() } else { "│ ".to_string() };
                callout = None;
                marker.clear();
                out.lines.push(Line::from(""));
            }

            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut spans, &mut out, &prefix);
                in_code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                out.lines.push(Line::from(""));
            }

            Event::Start(Tag::List(start)) => list_stack.push(start),
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() {
                    out.lines.push(Line::from(""));
                }
            }
            Event::Start(Tag::Item) => {
                let depth = list_stack.len().saturating_sub(1);
                let bullet = match list_stack.last_mut() {
                    Some(Some(n)) => {
                        let s = format!("{n}. ");
                        *n += 1;
                        s
                    }
                    _ => "• ".to_string(),
                };
                prefix = format!("{}{bullet}", "  ".repeat(depth + 1));
            }
            Event::End(TagEnd::Item) => {
                flush(&mut spans, &mut out, &prefix);
                prefix = String::new();
            }

            Event::Start(Tag::Emphasis) => inline.italic = true,
            Event::End(TagEnd::Emphasis) => inline.italic = false,
            Event::Start(Tag::Strong) => inline.bold = true,
            Event::End(TagEnd::Strong) => inline.bold = false,
            Event::Start(Tag::Strikethrough) => inline.strike = true,
            Event::End(TagEnd::Strikethrough) => inline.strike = false,
            // A picture, where there is somewhere to look for it and a way to draw it.
            //
            // Drawn where it sits in the note rather than collected at the end, because an
            // embed in Obsidian is part of the prose — the sentence before it is usually
            // "here is what that looks like".
            Event::Start(Tag::Image { dest_url, .. }) => {
                let target = dest_url.strip_prefix(WIKI_SCHEME).unwrap_or(&dest_url).to_string();
                flush(&mut spans, &mut out, &prefix);
                skip_alt = true;
                // `![[Some Note]]` is an embed too, and arrives here as an image. horde does
                // not transclude one note into another yet, so it says what it is and offers
                // it as a link rather than hunting for a picture that was never there.
                if !crate::client::image::is_image(&target) {
                    out.links.push((out.lines.len(), target.clone()));
                    out.lines.push(Line::from(Span::styled(
                        format!("▸ {target}"),
                        Style::default().fg(color(theme.ui.accent)),
                    )));
                    continue;
                }
                let found =
                    crate::client::image::locate(&target, at.dir, at.vault).filter(|_| at.tall > 0);

                // Where the terminal can draw real pixels, let it: reserve the rows and
                // record the box. Half blocks are a 40x40 thumbnail of whatever this is, and
                // a photograph does not survive that.
                if let Some(p) = found.as_ref().filter(|_| at.pixels) {
                    if let Ok((pw, ph)) = image::image_dimensions(p) {
                        let (cols, rows) =
                            crate::client::kitty::fit(pw, ph, width as u16, at.tall);
                        if cols > 0 && rows > 0 {
                            out.images.push(Picture {
                                line: out.lines.len(),
                                path: p.clone(),
                                cols,
                                rows,
                            });
                            for _ in 0..rows {
                                out.lines.push(Line::from(""));
                            }
                            out.lines.push(Line::from(""));
                            continue;
                        }
                    }
                }

                let drawn = found.and_then(|p| {
                    crate::client::image::cells(&p, width as u16, at.tall, theme)
                });
                match drawn {
                    Some(rows) => {
                        image_rows = rows.len();
                        out.lines.extend(rows);
                        out.lines.push(Line::from(""));
                    }
                    // Named rather than silently dropped. A missing attachment is a thing to
                    // notice — usually it means the file did not come across with the note.
                    None => {
                        let name = target.rsplit('/').next().unwrap_or(&target).to_string();
                        out.lines.push(Line::from(Span::styled(
                            format!("🖼 {name}"),
                            Style::default().fg(color(theme.ui.text_faint)),
                        )));
                    }
                }
            }
            Event::End(TagEnd::Image) => {
                skip_alt = false;
                spans.clear();
                let _ = image_rows;
            }

            Event::Start(Tag::Link { dest_url, .. }) => {
                let dest = dest_url.to_string();
                if let Some(target) = dest.strip_prefix(WIKI_SCHEME) {
                    // Recorded against the line it will land on, so the reader can walk
                    // links without re-parsing the note.
                    out.links.push((out.lines.len(), target.to_string()));
                }
                pending_link = Some(dest);
            }
            Event::End(TagEnd::Link) => pending_link = None,

            Event::Rule => {
                flush(&mut spans, &mut out, &prefix);
                out.lines.push(Line::from(Span::styled(
                    "─".repeat(width),
                    Style::default().fg(color(theme.ui.border)),
                )));
                out.lines.push(Line::from(""));
            }

            // Alt text belongs to the picture, not to the page.
            Event::Text(_) | Event::Code(_) if skip_alt => {}

            Event::Text(ref t) if title_for.is_some() => {
                let (c, kind, mut title) = title_for.take().expect("checked");
                title.push_str(t.trim());
                let label = if title.trim().is_empty() { kind } else { title.trim().to_string() };
                out.lines.push(Line::from(vec![
                    Span::styled("│ ".to_string(), Style::default().fg(color(c))),
                    Span::styled(label, Style::default().fg(color(c)).add_modifier(Modifier::BOLD)),
                ]));
            }
            Event::Text(t) => {
                if in_code {
                    for l in t.lines() {
                        out.lines.push(Line::from(Span::styled(
                            format!("    {l}"),
                            Style::default().fg(color(theme.ui.text_dim)).bg(color(theme.ui.panel_bg)),
                        )));
                    }
                    continue;
                }
                // A callout's marker is the first text of a quote. CommonMark hands it over
                // in pieces — `[`, `!warning`, `]`, ` Watch out` — so it is accumulated
                // until the closing bracket rather than matched against one string.
                if quote_depth > 0 && callout.is_none() && spans.is_empty() {
                    marker.push_str(&t);
                    if marker.starts_with('[') && !marker.contains(']') {
                        continue; // still collecting
                    }
                    let taken = std::mem::take(&mut marker);
                    if let Some((kind, title)) = callout_kind(&taken) {
                        let c = callout_color(&kind, theme);
                        callout = Some(c);
                        prefix = "│ ".to_string();
                        // The title is whatever follows the marker on the same line, and
                        // arrives as its own event. Hold the header open for it rather than
                        // labelling the callout "warning" and dropping its actual title into
                        // the body, one line below where it was written.
                        title_for = Some((c, kind, title));
                        continue;
                    }
                    // Not a callout after all: put back what was collected.
                    if !taken.is_empty() {
                        spans.push(Span::styled(taken, inline.apply(text_style)));
                    }
                }

                let rest = t.to_string();
                if !rest.is_empty() {
                    let style = match (&pending_link, callout) {
                        (Some(_), _) => inline
                            .apply(Style::default().fg(color(theme.ui.accent)))
                            .add_modifier(Modifier::UNDERLINED),
                        (None, Some(_)) => inline.apply(Style::default().fg(color(theme.ui.text))),
                        _ => inline.apply(text_style),
                    };
                    spans.push(Span::styled(rest, style));
                }
            }
            Event::Code(c) => spans.push(Span::styled(
                c.to_string(),
                Style::default().fg(color(theme.ui.accent_alt)).bg(color(theme.ui.panel_bg)),
            )),
            Event::SoftBreak => {
                if let Some((c, kind, title)) = title_for.take() {
                    let label = if title.trim().is_empty() { kind } else { title };
                    out.lines.push(Line::from(vec![
                        Span::styled("│ ".to_string(), Style::default().fg(color(c))),
                        Span::styled(
                            label,
                            Style::default().fg(color(c)).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                } else {
                    spans.push(Span::styled(" ".to_string(), text_style));
                }
            }
            Event::HardBreak => flush(&mut spans, &mut out, &prefix),
            Event::TaskListMarker(done) => spans.push(Span::styled(
                if done { "[x] ".to_string() } else { "[ ] ".to_string() },
                Style::default().fg(color(if done { theme.ui.ok } else { theme.ui.text_faint })),
            )),
            _ => {}
        }
        // Headings collect their text but never wrap mid-flow.
        if heading.is_none() && spans.len() > 400 {
            flush(&mut spans, &mut out, &prefix);
        }
    }
    flush(&mut spans, &mut out, &prefix);

    while out.lines.last().is_some_and(|l| l.width() == 0) {
        out.lines.pop();
    }
    out
}

/// Render one source line as it should look while being written.
///
/// Live preview, in the sense Obsidian means it: markers disappear and what they did stays.
/// `**bold**` becomes bold, `# ` gives its line weight, `[[a link]]` loses its brackets.
///
/// Deliberately line-by-line rather than reusing [`render`], which reflows and inserts and
/// would leave no way to say which rendered row a cursor on source line 12 belongs to. Here
/// one source line is one rendered line, always — which is what makes a cursor possible.
///
/// The caller shows the *cursor's* line as raw source instead of calling this, because
/// hiding characters under a cursor makes the arrow keys lie about where they are going.
pub fn live_line(src: &str, theme: &Theme) -> Line<'static> {
    let text = Style::default().fg(color(theme.ui.text));
    let faint = Style::default().fg(color(theme.ui.text_faint));
    let mut spans: Vec<Span<'static>> = Vec::new();

    // Block markers first: they own the whole line.
    let (body, base, indent) = if let Some(rest) = src.trim_start().strip_prefix("###") {
        (rest.trim_start().to_string(), text.add_modifier(Modifier::BOLD), String::new())
    } else if let Some(rest) = src.trim_start().strip_prefix("##") {
        (
            rest.trim_start().to_string(),
            Style::default().fg(color(theme.ui.accent_alt)).add_modifier(Modifier::BOLD),
            String::new(),
        )
    } else if let Some(rest) = src.trim_start().strip_prefix('#') {
        (
            rest.trim_start().to_string(),
            Style::default().fg(color(theme.ui.accent)).add_modifier(Modifier::BOLD),
            String::new(),
        )
    } else if let Some(rest) = src.trim_start().strip_prefix("> ") {
        spans.push(Span::styled("│ ".to_string(), faint));
        (rest.to_string(), Style::default().fg(color(theme.ui.text_dim)), String::new())
    } else if let Some(rest) = src.trim_start().strip_prefix("- ") {
        let lead = src.len() - src.trim_start().len();
        ( rest.to_string(), text, format!("{}• ", " ".repeat(lead)) )
    } else if src.trim() == "---" {
        return Line::from(Span::styled("─".repeat(40), faint));
    } else {
        (src.to_string(), text, String::new())
    };
    if !indent.is_empty() {
        spans.push(Span::styled(indent, faint));
    }

    // Inline markers, in one pass. Each is a pair that hides itself and styles what it wraps.
    let chars: Vec<char> = body.chars().collect();
    let mut buf = String::new();
    let mut i = 0;
    let push = |spans: &mut Vec<Span<'static>>, buf: &mut String, st: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), st));
        }
    };
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let pair = |open: &str, close: &str| -> Option<usize> {
            rest.strip_prefix(open).and_then(|r| r.find(close)).filter(|n| *n > 0)
        };

        if let Some(n) = pair("[[", "]]") {
            push(&mut spans, &mut buf, base);
            let inner: String = rest[2..2 + n].to_string();
            let (target, _h, alias) = crate::daemon::vault::split_target(&inner);
            spans.push(Span::styled(
                alias.unwrap_or(target),
                base.fg(color(theme.ui.accent)).add_modifier(Modifier::UNDERLINED),
            ));
            i += 2 + n + 2;
        } else if let Some(n) = pair("**", "**") {
            push(&mut spans, &mut buf, base);
            spans.push(Span::styled(rest[2..2 + n].to_string(), base.add_modifier(Modifier::BOLD)));
            i += 2 + n + 2;
        } else if let Some(n) = pair("`", "`") {
            push(&mut spans, &mut buf, base);
            spans.push(Span::styled(
                rest[1..1 + n].to_string(),
                base.fg(color(theme.ui.accent_alt)).bg(color(theme.ui.panel_bg)),
            ));
            i += 1 + n + 1;
        } else if let Some(n) = pair("*", "*") {
            push(&mut spans, &mut buf, base);
            spans
                .push(Span::styled(rest[1..1 + n].to_string(), base.add_modifier(Modifier::ITALIC)));
            i += 1 + n + 1;
        } else {
            buf.push(chars[i]);
            i += 1;
        }
    }
    push(&mut spans, &mut buf, base);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(r: &Rendered) -> String {
        r.lines
            .iter()
            .map(|l| {
                l.spans.iter().map(|s| s.content.to_string()).collect::<String>().trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Style of the span holding `word`. Wrapping splits a phrase across spans, so tests
    /// ask about a single word rather than about a sentence.
    fn styled(r: &Rendered, word: &str) -> Option<Style> {
        r.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.split_whitespace().any(|w| w == word))
            .map(|s| s.style)
    }

    /// The whole point: a heading reads as a heading. A terminal has one font size, so the
    /// weight has to come from colour, boldness and a rule.
    #[test]
    fn a_heading_is_bold_accented_and_underlined_by_a_rule() {
        let r = render_in("# Title\n\nbody text\n", 40, &Theme::horde(), Where::default());
        let s = styled(&r, "Title").expect("the heading is there");
        assert!(s.add_modifier.contains(Modifier::BOLD), "bold");
        assert_eq!(s.fg, Some(color(Theme::horde().ui.accent)), "and accented");
        assert!(plain(&r).contains("─────"), "with a rule under it:\n{}", plain(&r));
    }

    /// Emphasis has to actually be emphasised, or the reading view is just reflowed source.
    #[test]
    fn bold_and_italic_become_real_terminal_attributes() {
        let r = render_in("**strong** and *soft* and ~~gone~~\n", 60, &Theme::horde(), Where::default());
        assert!(styled(&r, "strong").unwrap().add_modifier.contains(Modifier::BOLD));
        assert!(styled(&r, "soft").unwrap().add_modifier.contains(Modifier::ITALIC));
        assert!(styled(&r, "gone").unwrap().add_modifier.contains(Modifier::CROSSED_OUT));
        // And the markers themselves are gone — that is what "rendered" means.
        assert!(!plain(&r).contains("**"), "{}", plain(&r));
        assert!(!plain(&r).contains("~~"), "{}", plain(&r));
    }

    /// A wikilink shows what it says, not how it is written, and is recorded so the reader
    /// can follow it.
    #[test]
    fn a_wikilink_shows_its_text_without_the_brackets_and_is_followable() {
        let r = render_in("see [[Some Note]] and [[Other|call it this]]\n", 60, &Theme::horde(), Where::default());
        let text = plain(&r);
        assert!(text.contains("Some Note"), "{text}");
        assert!(text.contains("call it this"), "the alias is what shows: {text}");
        assert!(!text.contains("[["), "brackets are gone: {text}");
        assert_eq!(
            r.links.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(),
            vec!["Some Note", "Other"],
            "and both targets are followable"
        );
        assert!(styled(&r, "Note").unwrap().add_modifier.contains(Modifier::UNDERLINED));
    }

    /// Obsidian's callouts are common enough in a real vault to be worth knowing: 69 of them
    /// in the one this was built against. Rendered as a coloured bar with a title, never as
    /// the literal `[!warning]` a reader would have to decode.
    #[test]
    fn a_callout_becomes_a_coloured_bar_rather_than_its_own_syntax() {
        let r = render_in("> [!warning] Watch out\n> the body of it\n", 60, &Theme::horde(), Where::default());
        let text = plain(&r);
        assert!(text.contains("Watch out"), "{text}");
        assert!(!text.contains("[!warning]"), "the marker is not shown: {text}");
        assert!(text.contains("│"), "a bar down the side: {text}");
        assert_eq!(styled(&r, "Watch").unwrap().fg, Some(color(Theme::horde().ui.warn)));
    }

    #[test]
    fn lists_get_bullets_and_numbers_and_checkboxes() {
        let r = render_in("- one\n- two\n\n1. first\n2. second\n\n- [x] done\n- [ ] not\n", 40, &Theme::horde(), Where::default());
        let text = plain(&r);
        assert!(text.contains("• one"), "{text}");
        assert!(text.contains("1. first") && text.contains("2. second"), "{text}");
        assert!(text.contains("[x] done") && text.contains("[ ] not"), "{text}");
    }

    /// Code is shown as written — that is the one place reflowing would be wrong.
    #[test]
    fn a_fenced_block_keeps_its_lines_and_gets_its_own_ground() {
        let r = render_in("text\n\n```rust\nfn main() {\n    let x = 1;\n}\n```\n", 60, &Theme::horde(), Where::default());
        let text = plain(&r);
        assert!(text.contains("fn main() {"), "{text}");
        assert!(text.contains("    let x = 1;"), "indentation survives: {text}");
        assert_eq!(
            styled(&r, "fn").unwrap().bg,
            Some(color(Theme::horde().ui.panel_bg)),
            "and it sits on its own background"
        );
    }

    /// Long prose has to wrap to the pane it is being read in, and keep its list indent.
    #[test]
    fn prose_wraps_to_the_width_it_is_given() {
        let long = "word ".repeat(60);
        let r = render_in(&format!("- {long}\n"), 30, &Theme::horde(), Where::default());
        assert!(r.lines.len() > 3, "it wrapped into several lines");
        for l in &r.lines {
            assert!(l.width() <= 30, "line of {} columns: {l:?}", l.width());
        }
    }

    /// Styling splits a sentence into spans, and rejoining them with spaces invents
    /// whitespace nobody wrote: `**bold**:` becomes "bold :" and `(~31k)` becomes "( ~ 31k)".
    /// Caught by reading a real note, and cheap to catch again.
    #[test]
    fn styling_a_word_does_not_add_a_space_after_it() {
        let r = render_in("Being grown into **horde-full**: a thing (~31.7k lines).\n", 100, &Theme::horde(), Where::default());
        let text = plain(&r);
        assert!(text.contains("horde-full: a thing"), "no space before the colon: {text}");
        assert!(text.contains("(~31.7k lines)"), "and none inside the parentheses: {text}");
    }

    /// Live preview: the markers do their job and then get out of the way. This is what
    /// makes writing in horde feel like writing a note rather than editing a file.
    #[test]
    fn a_line_being_written_renders_without_its_markers() {
        let th = Theme::horde();
        let plain = |l: &Line| l.spans.iter().map(|s| s.content.to_string()).collect::<String>();

        assert_eq!(plain(&live_line("# Title", &th)), "Title", "the hash is gone");
        assert_eq!(plain(&live_line("say **this** now", &th)), "say this now");
        assert_eq!(plain(&live_line("a `code` bit", &th)), "a code bit");
        assert_eq!(plain(&live_line("see [[A Note|it]]", &th)), "see it", "and the alias shows");
        assert_eq!(plain(&live_line("- item", &th)), "• item");
        assert_eq!(plain(&live_line("> quoted", &th)), "│ quoted");

        // The styling actually happened, rather than the markers merely being deleted.
        let bold = live_line("**loud**", &th);
        assert!(bold.spans.iter().any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
        let head = live_line("# Big", &th);
        assert_eq!(head.spans[0].style.fg, Some(color(th.ui.accent)));
    }

    /// Source is not markdown. `**p` in C is a pointer to a pointer, and a live preview
    /// that decided it was bold would delete two characters from a line of code — which is
    /// why the editor only calls this for markdown files, and why that rule is worth stating
    /// where the renderer lives rather than only where it is used.
    #[test]
    fn the_renderer_is_only_correct_for_markdown() {
        let th = Theme::horde();
        let plain = |s: &str| {
            live_line(s, &th).spans.iter().map(|x| x.content.to_string()).collect::<String>()
        };
        // One unclosed marker is safe — it stays literal, which is the half-typed case.
        assert_eq!(plain("let x = **p;"), "let x = **p;");
        // Two are not: C sees pointers, markdown sees emphasis, and the renderer deletes
        // four characters of somebody's code.
        assert_eq!(plain("let a = **p + **q;"), "let a = p + q;");
        assert_eq!(plain("# not a heading in rust"), "not a heading in rust");
    }

    /// One source line is one rendered line, always. Without that there is no answer to
    /// "which row is the cursor on", and a live preview with a lying cursor is worse than
    /// no live preview.
    #[test]
    fn live_preview_never_changes_how_many_lines_there_are() {
        let th = Theme::horde();
        for src in ["# Heading", "", "- a list item", "plain prose", "> quote", "---", "**x**"] {
            let _: Line = live_line(src, &th); // one line in, one line out, by construction
        }
        // An unclosed marker is a half-typed one, which is the normal state of a line being
        // written. It must render as itself rather than swallowing the rest of the line.
        let plain = |s: &str| {
            live_line(s, &th).spans.iter().map(|x| x.content.to_string()).collect::<String>()
        };
        assert_eq!(plain("half **way through"), "half **way through");
        assert_eq!(plain("a [[link being typ"), "a [[link being typ");
    }

    /// Frontmatter is metadata. A reader wants the note, not eight lines of YAML.
    #[test]
    fn frontmatter_collapses_to_a_single_faint_line() {
        let r = render_in("---\ntags: [a, b]\nstatus: active\n---\n\n# Real\n", 40, &Theme::horde(), Where::default());
        let text = plain(&r);
        assert!(text.contains("tags · status"), "{text}");
        assert!(!text.contains("[a, b]"), "the values are not the point: {text}");
        assert!(text.contains("Real"));
    }

    /// `![[shot.png]]` is a picture and `![[Some Note]]` is not, and both arrive here as the
    /// same event. Drawing the second as a broken image is the failure worth pinning.
    #[test]
    fn an_embedded_picture_is_drawn_and_an_embedded_note_is_offered() {
        let dir = std::env::temp_dir().join(format!("horde-md-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        let mut img = image::RgbaImage::new(8, 4);
        for p in img.pixels_mut() {
            *p = image::Rgba([200, 30, 40, 255]);
        }
        img.save(dir.join("attachments").join("shot.png")).unwrap();

        let t = Theme::horde();
        let at = Where { dir: Some(&dir), vault: Some(&dir), tall: 8, pixels: false };
        let out = render_in("before\n\n![[shot.png]]\n\nafter", 40, &t, at);
        let text: Vec<String> = out
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();

        assert!(text.iter().any(|l| l.contains('▀')), "the picture is drawn: {text:?}");
        assert!(text.iter().any(|l| l.contains("before")) && text.iter().any(|l| l.contains("after")));
        assert!(!text.iter().any(|l| l.contains("shot.png")), "and not named as well: {text:?}");

        // A note embed is not a picture. It says what it is and can be followed.
        let out = render_in("![[Some Note]]", 40, &t, at);
        let text: String = out.lines.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.to_string()).collect();
        assert!(text.contains("Some Note"), "{text}");
        assert!(!text.contains('▀'), "not drawn as a broken picture: {text}");
        assert_eq!(out.links.first().map(|(_, t)| t.as_str()), Some("Some Note"), "and followable");

        // Where the terminal draws the pixels itself, the rows are left empty for it and
        // recorded — anything written into them would half-erase the picture, because a
        // kitty image is not in ratatui's grid at all.
        let at = Where { dir: Some(&dir), vault: Some(&dir), tall: 8, pixels: true };
        let out = render_in("before\n\n![[shot.png]]\n\nafter", 40, &t, at);
        assert_eq!(out.images.len(), 1, "one picture recorded");
        let pic = &out.images[0];
        // 8x4 pixels is 2:1, which in cells twice as tall as they are wide is 4:1 — so it
        // hits the row limit first and the width comes in to match, rather than squashing.
        assert_eq!((pic.cols, pic.rows), (32, 8));
        for i in 0..pic.rows as usize {
            let row: String = out.lines[pic.line + i].spans.iter().map(|s| s.content.to_string()).collect();
            assert!(row.trim().is_empty(), "row {i} was not left empty: {row:?}");
        }
        let text: String = out.lines.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.to_string()).collect();
        assert!(!text.contains('▀'), "and no half blocks under it: {text}");

        let at = Where { dir: Some(&dir), vault: Some(&dir), tall: 8, pixels: false };
        // A picture that is not there is named rather than silently dropped — usually it
        // means the file did not travel with the note.
        let out = render_in("![[missing.png]]", 40, &t, at);
        let text: String = out.lines.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.to_string()).collect();
        assert!(text.contains("missing.png"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
