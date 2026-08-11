//! Documentation, compiled into the binary.
//!
//! An agent running in a pane has no idea any of this exists and no reliable path to a
//! checkout, so the docs ship inside the binary and are readable with `horde docs <topic>`.
//! Every pane also gets `HORDE_DOCS` in its environment holding that command.

use anyhow::{anyhow, Result};

/// `(topic, one-line summary, contents)`.
pub const PAGES: &[(&str, &str, &str)] = &[
    ("index", "table of contents", include_str!("../../docs/README.md")),
    (
        "orchestration",
        "agent-to-agent messaging — start here if you are an agent",
        include_str!("../../docs/orchestration.md"),
    ),
    ("quick-start", "install, first session, first agent", include_str!("../../docs/quick-start.md")),
    ("concepts", "spaces, tabs, panes, the daemon", include_str!("../../docs/concepts.md")),
    ("agents", "detection, states, lifecycle hooks", include_str!("../../docs/agents.md")),
    ("socket-api", "the control protocol, every method", include_str!("../../docs/socket-api.md")),
    ("configuration", "config.toml and the settings page", include_str!("../../docs/configuration.md")),
    ("keys", "keybindings, mouse, right-click menus", include_str!("../../docs/keys.md")),
    ("troubleshooting", "when something looks wrong", include_str!("../../docs/troubleshooting.md")),
];

pub fn topics() -> Vec<&'static str> {
    PAGES.iter().map(|(t, _, _)| *t).collect()
}

/// Print one page, or the list of topics when none is named.
pub fn show(topic: Option<&str>) -> Result<()> {
    let Some(topic) = topic else {
        println!("horde documentation — read a page with `horde docs <topic>`\n");
        let width = topics().iter().map(|t| t.len()).max().unwrap_or(12);
        for (name, summary, _) in PAGES {
            println!("  {name:<width$}  {summary}");
        }
        println!(
            "\nIf you are an agent and want to talk to other agents:\n  horde docs orchestration"
        );
        return Ok(());
    };

    // Accept a filename or a partial name, since the docs cross-reference each other as
    // `orchestration.md` and an agent may well paste that in verbatim.
    let key = topic.trim().trim_end_matches(".md");
    if let Some((_, _, body)) = PAGES.iter().find(|(t, _, _)| *t == key) {
        print!("{body}");
        return Ok(());
    }
    let matches: Vec<&str> = topics().into_iter().filter(|t| t.starts_with(key)).collect();
    match matches.as_slice() {
        [one] => {
            let (_, _, body) = PAGES.iter().find(|(t, _, _)| t == one).unwrap();
            print!("{body}");
            Ok(())
        }
        [] => Err(anyhow!(
            "no such topic {topic:?} — try one of: {}",
            topics().join(", ")
        )),
        many => Err(anyhow!("{topic:?} is ambiguous: {}", many.join(", "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::HashSet;

    #[test]
    fn every_page_is_non_empty_and_has_a_summary() {
        for (topic, summary, body) in PAGES {
            assert!(!summary.trim().is_empty(), "{topic} has no summary");
            assert!(body.len() > 200, "{topic} looks empty ({} bytes)", body.len());
            // Each page should open with a heading, so piped output reads sensibly.
            assert!(body.trim_start().starts_with('#'), "{topic} has no title heading");
        }
    }

    #[test]
    fn topics_are_unique() {
        let set: HashSet<&str> = topics().into_iter().collect();
        assert_eq!(set.len(), topics().len(), "duplicate topic: {:?}", topics());
    }

    /// Docs rot. Every cross-reference between pages must resolve to a real topic, or an
    /// agent following a link ends up nowhere.
    #[test]
    fn internal_links_all_resolve() {
        let known: HashSet<&str> = topics().into_iter().collect();
        let link = regex::Regex::new(r"\]\(([A-Za-z0-9._-]+)\.md(#[^)]*)?\)").unwrap();
        let mut checked = 0;
        for (topic, _, body) in PAGES {
            for cap in link.captures_iter(body) {
                let target = cap.get(1).unwrap().as_str();
                assert!(known.contains(target), "{topic}.md links to missing {target}.md");
                checked += 1;
            }
        }
        assert!(checked > 5, "expected the docs to cross-reference each other");
    }

    #[test]
    fn lookup_accepts_exact_names_filenames_and_prefixes() {
        assert!(show(Some("orchestration")).is_ok());
        // The docs link to each other as `orchestration.md`, so that form must work too.
        assert!(show(Some("orchestration.md")).is_ok());
        assert!(show(Some("orch")).is_ok());
        assert!(show(None).is_ok());
    }

    #[test]
    fn unknown_and_ambiguous_topics_explain_themselves() {
        let err = show(Some("nonsense")).unwrap_err().to_string();
        assert!(err.contains("no such topic"), "{err}");
        assert!(err.contains("orchestration"), "the error should list the options: {err}");
    }

    /// The orchestration page is the one an agent is pointed at, so hold it to a standard:
    /// it must actually contain the commands it is describing.
    #[test]
    fn orchestration_page_documents_the_real_commands() {
        let (_, _, body) = PAGES.iter().find(|(t, _, _)| *t == "orchestration").unwrap();
        for needed in [
            "HORDE_PANE",
            "horde roster",
            "horde send",
            "horde broadcast",
            "horde wait",
            "horde spawn",
            "horde pane read",
            "horde bus tail",
            "[horde] message from",
            "--now",
        ] {
            assert!(body.contains(needed), "orchestration.md never mentions {needed}");
        }
    }

    /// Anything the page tells an agent to run must be a real subcommand.
    #[test]
    fn documented_commands_parse_as_real_cli_invocations() {
        for args in [
            vec!["horde", "roster"],
            vec!["horde", "roster", "--json"],
            vec!["horde", "send", "reviewer", "text"],
            vec!["horde", "send", "reviewer", "text", "--now"],
            vec!["horde", "broadcast", "text"],
            vec!["horde", "broadcast", "text", "--space", "api"],
            vec!["horde", "wait", "reviewer", "--until", "idle", "--timeout", "300"],
            vec!["horde", "spawn", "--cmd", "claude", "--name", "reviewer", "--split", "right"],
            vec!["horde", "pane", "read", "reviewer", "--source", "detection", "--lines", "40"],
            vec!["horde", "pane", "current"],
            vec!["horde", "pane", "rename", "3", "reviewer"],
            vec!["horde", "bus", "tail", "--limit", "30"],
            vec!["horde", "bus", "tail", "-f"],
            vec!["horde", "agent", "explain", "reviewer"],
            vec!["horde", "layout", "quad"],
            vec!["horde", "status"],
            vec!["horde", "docs", "orchestration"],
            vec!["horde", "api", "server.status"],
        ] {
            let joined = args.join(" ");
            assert!(
                crate::cli::Cli::try_parse_from(&args).is_ok(),
                "documented command does not parse: {joined}"
            );
        }
    }
}
