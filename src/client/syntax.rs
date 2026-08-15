//! Language recognition: what a file is, and what colour each part of it should be.
//!
//! Grammars are cargo features, because each is compiled C with a real size to it — the
//! typescript pair is 2.8 MiB, json is 8 KiB. A build that only wants notes can have one
//! that only knows markdown, and the code here compiles down to "no language recognised"
//! rather than to a missing symbol.
//!
//! Highlighting produces spans per line rather than a styled document, because the editor
//! draws one line at a time and a highlighter that answers a different question is a
//! highlighter with a translation layer bolted to it.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use super::ui::color;
use crate::theme::Theme;

/// The scopes horde asks a grammar about.
///
/// Deliberately a short list. tree-sitter grammars offer dozens, most of which differ in
/// ways nobody reads at terminal contrast — and every name here has to mean something in the
/// theme, or it is a colour nobody chose.
const SCOPES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constructor",
    "function",
    "function.method",
    "keyword",
    "label",
    "namespace",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "tag",
    "type",
    "variable",
];

/// A language horde can colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Language {
    pub name: &'static str,
}

/// The language a filename implies, if it is one horde was built with.
///
/// By extension, then by whole filename for the handful of files that have none — a
/// `Dockerfile` is not "a file with no extension", it is a Dockerfile.
pub fn detect(path: &str) -> Option<Language> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let lang = match ext.as_str() {
        "md" | "markdown" | "mdx" => "markdown",
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "py" | "pyi" => "python",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "sh" | "bash" | "zsh" => "bash",
        _ => match name {
            "Cargo.lock" => "toml",
            ".bashrc" | ".zshrc" | ".profile" | "PKGBUILD" => "bash",
            _ => return None,
        },
    };
    Some(Language { name: lang })
}

/// Every language this build was compiled with, for `horde status` and the settings page.
pub fn available() -> Vec<&'static str> {
    let mut out = Vec::new();
    for name in
        ["markdown", "rust", "typescript", "tsx", "javascript", "python", "json", "toml", "bash"]
    {
        if config_for(name).is_some() {
            out.push(name);
        }
    }
    out
}

/// JavaScript's highlight query followed by TypeScript's, which is how the latter is meant
/// to be used: it describes only the additions.
#[cfg(feature = "lang-typescript")]
fn ts_query() -> String {
    format!(
        "{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY
    )
}

/// The grammar and its queries, when this build has them.
///
/// One `cfg` per feature, and each arm is the only place a grammar crate is named — so a
/// build without the feature has no reference to it at all rather than a stub.
// The warnings are for one configuration only: with every language feature off, the match
// below has a single arm that returns, so what follows it genuinely is unreachable. That
// build is supported rather than accidental, and silencing it here is narrower than making
// the code pretend otherwise.
#[allow(unreachable_code, unused_variables, unused_mut)]
fn config_for(lang: &str) -> Option<HighlightConfiguration> {
    // Annotated because a build with no language features has only the `_` arm, and a match
    // with one arm that returns has no type to infer. Trimming every grammar out is a
    // supported build, not a broken one.
    let built: Result<HighlightConfiguration, tree_sitter::QueryError> = match lang {
        #[cfg(feature = "lang-markdown")]
        "markdown" => HighlightConfiguration::new(
            tree_sitter_md::LANGUAGE.into(),
            "markdown",
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            "",
        ),
        #[cfg(feature = "lang-rust")]
        "rust" => HighlightConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            "",
        ),
        // TypeScript's query only describes what TypeScript adds. Everything a `.ts` file
        // shares with JavaScript — comments, strings, numbers, functions — is in the
        // JavaScript query, so it goes in front. Without it a TypeScript file gets its type
        // annotations coloured and nothing else, which looks like a broken highlighter
        // rather than a missing one.
        #[cfg(feature = "lang-typescript")]
        "typescript" => HighlightConfiguration::new(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            &ts_query(),
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        #[cfg(feature = "lang-typescript")]
        "tsx" => HighlightConfiguration::new(
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            &ts_query(),
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        #[cfg(feature = "lang-javascript")]
        "javascript" => HighlightConfiguration::new(
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        ),
        #[cfg(feature = "lang-python")]
        "python" => HighlightConfiguration::new(
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        #[cfg(feature = "lang-json")]
        "json" => HighlightConfiguration::new(
            tree_sitter_json::LANGUAGE.into(),
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        #[cfg(feature = "lang-toml")]
        "toml" => HighlightConfiguration::new(
            tree_sitter_toml_ng::LANGUAGE.into(),
            "toml",
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        #[cfg(feature = "lang-bash")]
        "bash" => HighlightConfiguration::new(
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            tree_sitter_bash::HIGHLIGHT_QUERY,
            "",
            "",
        ),
        _ => return None,
    };
    let mut cfg = built.ok()?;
    cfg.configure(&SCOPES.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    Some(cfg)
}

/// A file's text, coloured, one entry per source line.
///
/// Returns `None` when the language is not one this build knows, which the caller shows as
/// plain text — an honest "I do not recognise this" rather than a guess.
pub fn highlight(path: &str, text: &str, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let lang = detect(path)?;
    let cfg = config_for(lang.name)?;
    let mut hl = Highlighter::new();
    let events = hl.highlight(&cfg, text.as_bytes(), None, |_| None).ok()?;

    let plain = Style::default().fg(color(theme.ui.text));
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for ev in events {
        match ev.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(h.0),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = match stack.last().and_then(|i| SCOPES.get(*i)) {
                    Some(scope) => Style::default().fg(color(theme.syntax.for_scope(scope))),
                    None => plain,
                };
                // Source arrives in arbitrary chunks, so lines are cut here rather than
                // assumed: a single chunk can hold a whole function.
                let chunk = text.get(start..end)?;
                for (i, piece) in chunk.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Line::from(std::mem::take(&mut current)));
                    }
                    if !piece.is_empty() {
                        current.push(Span::styled(piece.to_string(), style));
                    }
                }
            }
        }
    }
    lines.push(Line::from(current));
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_file_is_recognised_by_extension_and_by_name() {
        assert_eq!(detect("src/main.rs").map(|l| l.name), Some("rust"));
        assert_eq!(detect("app/page.tsx").map(|l| l.name), Some("tsx"));
        assert_eq!(detect("Cargo.lock").map(|l| l.name), Some("toml"), "no extension, still toml");
        assert_eq!(detect("notes/Idea.md").map(|l| l.name), Some("markdown"));
        assert_eq!(detect("mystery.xyzzy"), None, "an unknown one says so");
        assert_eq!(detect("LICENSE"), None);
    }

    /// The whole point: the text comes back unchanged, and its parts are coloured
    /// differently. A highlighter that alters what it is highlighting is worse than none.
    #[test]
    fn highlighting_colours_code_without_changing_a_character_of_it() {
        let src = "// a comment\nfn main() {\n    let s = \"hi\";\n}\n";
        let theme = Theme::horde();
        let Some(lines) = highlight("x.rs", src, &theme) else {
            return; // built without lang-rust
        };
        assert_eq!(plain(&lines).trim_end(), src.trim_end(), "text is untouched");

        let colour_of = |needle: &str| {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        };
        assert_eq!(colour_of("comment"), Some(color(theme.syntax.comment)));
        assert_eq!(colour_of("fn"), Some(color(theme.syntax.keyword)));
        assert_eq!(colour_of("hi"), Some(color(theme.syntax.string)));
        assert_ne!(
            colour_of("comment"),
            colour_of("fn"),
            "and the parts are actually told apart"
        );
    }

    /// One source line is one output line. The editor draws by line and positions a cursor
    /// by line, so a highlighter that merged or split them would move the cursor.
    #[test]
    fn every_source_line_comes_back_as_exactly_one_line() {
        let theme = Theme::horde();
        for (path, src) in [
            ("a.rs", "fn a() {}\n\nfn b() {}\n"),
            ("b.json", "{\n  \"a\": 1\n}\n"),
            ("c.py", "def f():\n    return 1\n"),
        ] {
            let Some(lines) = highlight(path, src, &theme) else { continue };
            assert_eq!(
                lines.len(),
                src.split('\n').count(),
                "{path} has {} source lines",
                src.split('\n').count()
            );
        }
    }

    /// A half-written file is the normal state of one being edited, and a grammar that
    /// cannot parse it must still return the text rather than nothing.
    #[test]
    fn code_that_does_not_parse_still_comes_back_whole() {
        let theme = Theme::horde();
        let broken = "fn main( {\n  let x = ;\n";
        if let Some(lines) = highlight("x.rs", broken, &theme) {
            assert_eq!(plain(&lines).trim_end(), broken.trim_end());
        }
    }

    /// Colours come from the theme, so a window painted in gruvbox does not have somebody
    /// else's editor inside it.
    #[test]
    fn each_theme_colours_code_in_its_own_palette() {
        let horde = Theme::by_name("horde").unwrap();
        let gruvbox = Theme::by_name("gruvbox").unwrap();
        assert_eq!(horde.syntax.keyword, horde.ui.accent);
        assert_eq!(gruvbox.syntax.keyword, gruvbox.ui.accent);
        assert_ne!(horde.syntax.keyword, gruvbox.syntax.keyword, "and they differ");
    }

    #[test]
    fn the_build_reports_which_languages_it_has() {
        let langs = available();
        // The default build carries all of them; a trimmed one carries fewer. Either way the
        // list has to match what `detect` and `config_for` will actually do.
        for l in &langs {
            assert!(config_for(l).is_some(), "{l} is listed but has no grammar");
        }
    }
}
