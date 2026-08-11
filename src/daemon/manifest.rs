//! Screen manifests: how horde works out what an agent is doing by looking at its UI.
//!
//! This is the fallback tier. When an agent reports through lifecycle hooks (see
//! `integration install`), those reports are authoritative and manifests are not consulted
//! at all — one status authority per pane, never two.
//!
//! Bundled manifests are compiled in; `~/.config/horde/agents/<name>.toml` overrides one
//! wholesale, so a broken upstream pattern is always fixable without rebuilding.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use regex::RegexBuilder;
use serde::Deserialize;

use crate::proto::AgentState;

/// Manifests shipped with the binary.
const BUNDLED: &[(&str, &str)] = &[
    ("claude", include_str!("../../agents/claude.toml")),
    ("codex", include_str!("../../agents/codex.toml")),
    ("gemini", include_str!("../../agents/gemini.toml")),
    ("cursor-agent", include_str!("../../agents/cursor-agent.toml")),
    ("aider", include_str!("../../agents/aider.toml")),
    ("opencode", include_str!("../../agents/opencode.toml")),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    #[serde(default)]
    processes: Vec<String>,
    #[serde(default)]
    detect: Vec<String>,
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    state: String,
    #[serde(default)]
    any: Vec<String>,
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    none: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub state: AgentState,
    any: Vec<regex::Regex>,
    all: Vec<regex::Regex>,
    none: Vec<regex::Regex>,
}

impl Rule {
    /// A rule fires when at least one `any` matches (or `any` is empty), every `all`
    /// matches, and no `none` matches.
    fn matches(&self, screen: &str) -> bool {
        if !self.any.is_empty() && !self.any.iter().any(|r| r.is_match(screen)) {
            return false;
        }
        if !self.all.iter().all(|r| r.is_match(screen)) {
            return false;
        }
        if self.none.iter().any(|r| r.is_match(screen)) {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    /// Foreground process names that indicate this agent.
    pub processes: Vec<String>,
    /// Patterns proving this agent's UI is on screen. Presence detection works even when
    /// the process name is unhelpful (a wrapper script, an interpreter, a versioned path).
    detect: Vec<regex::Regex>,
    pub rules: Vec<Rule>,
}

/// Why a state was chosen — surfaced by `horde agent explain`.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub state: AgentState,
    pub reason: String,
}

impl Manifest {
    /// Does this agent's UI appear to be on screen, or is its process in the foreground?
    pub fn present(&self, process: Option<&str>, screen: &str) -> bool {
        if let Some(p) = process {
            let base = p.rsplit('/').next().unwrap_or(p);
            if self.processes.iter().any(|want| want == base) {
                return true;
            }
        }
        self.detect.iter().any(|r| r.is_match(screen))
    }

    /// First matching rule wins.
    ///
    /// An unmatched screen resolves to `idle` with a labelled reason rather than
    /// `unknown`: a known agent sitting at a prompt horde does not recognise is far more
    /// likely idle than in some indeterminate state.
    pub fn evaluate(&self, screen: &str) -> Verdict {
        for rule in &self.rules {
            if rule.matches(screen) {
                return Verdict { state: rule.state, reason: rule.name.clone() };
            }
        }
        Verdict { state: AgentState::Idle, reason: "no rule matched".into() }
    }
}

fn compile(patterns: &[String], ctx: &str) -> Result<Vec<regex::Regex>> {
    patterns
        .iter()
        .map(|p| {
            RegexBuilder::new(p)
                // Patterns describe lines of a terminal, so `^`/`$` should mean line
                // boundaries, and case rarely carries meaning in a TUI.
                .multi_line(true)
                .case_insensitive(true)
                .size_limit(1 << 20)
                .build()
                .with_context(|| format!("{ctx}: bad pattern {p:?}"))
        })
        .collect()
}

fn parse_state(s: &str) -> Result<AgentState> {
    Ok(match s {
        "working" => AgentState::Working,
        "blocked" => AgentState::Blocked,
        "idle" => AgentState::Idle,
        "done" => AgentState::Done,
        "unknown" => AgentState::Unknown,
        other => return Err(anyhow!("unknown state {other:?}")),
    })
}

pub fn parse(text: &str) -> Result<Manifest> {
    let raw: RawManifest = toml::from_str(text)?;
    let mut rules = Vec::new();
    for r in &raw.rules {
        rules.push(Rule {
            name: r.name.clone(),
            state: parse_state(&r.state)
                .with_context(|| format!("rule {:?} in {:?}", r.name, raw.name))?,
            any: compile(&r.any, &format!("{}.{}.any", raw.name, r.name))?,
            all: compile(&r.all, &format!("{}.{}.all", raw.name, r.name))?,
            none: compile(&r.none, &format!("{}.{}.none", raw.name, r.name))?,
        });
    }
    Ok(Manifest {
        detect: compile(&raw.detect, &format!("{}.detect", raw.name))?,
        name: raw.name,
        processes: raw.processes,
        rules,
    })
}

/// Bundled manifests, with any user override in `dir` replacing one entirely.
pub fn load_all(dir: &Path) -> (HashMap<String, Manifest>, Vec<String>) {
    let mut out = HashMap::new();
    let mut warnings = Vec::new();

    for (name, text) in BUNDLED {
        match parse(text) {
            Ok(m) => {
                out.insert(m.name.clone(), m);
            }
            // A bundled manifest failing to parse is a bug in horde, not in the user's setup.
            Err(e) => warnings.push(format!("bundled manifest {name} is invalid: {e}")),
        }
    }

    let Ok(entries) = std::fs::read_dir(dir) else { return (out, warnings) };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|t| parse(&t)) {
            Ok(m) => {
                out.insert(m.name.clone(), m);
            }
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_manifest_parses() {
        for (name, text) in BUNDLED {
            let m = parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&m.name, name);
            assert!(!m.rules.is_empty(), "{name} has no rules");
            // Without presence signals an agent could never be detected at all.
            assert!(
                !m.processes.is_empty() || !m.rules.is_empty(),
                "{name} has no way to be detected"
            );
        }
    }

    #[test]
    fn load_all_returns_bundled_manifests_when_the_override_dir_is_missing() {
        let (all, warnings) = load_all(Path::new("/nonexistent/horde/agents"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(all.contains_key("claude"));
        assert_eq!(all.len(), BUNDLED.len());
    }

    #[test]
    fn a_user_override_replaces_the_bundled_manifest() {
        let dir = std::env::temp_dir().join("horde-manifest-override-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("claude.toml"),
            r#"
name = "claude"
processes = ["claude"]
[[rules]]
name = "only-rule"
state = "working"
any = ["ZZZ"]
"#,
        )
        .unwrap();

        let (all, warnings) = load_all(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let m = &all["claude"];
        assert_eq!(m.rules.len(), 1);
        assert_eq!(m.rules[0].name, "only-rule");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_override_warns_and_keeps_the_bundled_version() {
        let dir = std::env::temp_dir().join("horde-manifest-broken-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("claude.toml"), "name = \"claude\"\n[[rules]]\nbroken").unwrap();

        let (all, warnings) = load_all(&dir);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(all["claude"].rules.len() > 1, "bundled rules should survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rule(any: &[&str], none: &[&str], all: &[&str], state: AgentState) -> Rule {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Rule {
            name: "t".into(),
            state,
            any: compile(&v(any), "t").unwrap(),
            all: compile(&v(all), "t").unwrap(),
            none: compile(&v(none), "t").unwrap(),
        }
    }

    #[test]
    fn rule_matching_honours_any_all_and_none() {
        let r = rule(&["hello"], &[], &[], AgentState::Working);
        assert!(r.matches("say hello there"));
        assert!(!r.matches("nothing here"));

        // `none` vetoes an otherwise matching rule.
        let r = rule(&["hello"], &["goodbye"], &[], AgentState::Working);
        assert!(r.matches("hello"));
        assert!(!r.matches("hello and goodbye"));

        // `all` requires every pattern.
        let r = rule(&[], &[], &["a", "b"], AgentState::Working);
        assert!(r.matches("a then b"));
        assert!(!r.matches("only a"));

        // An empty rule matches anything, which is how catch-all rules work.
        assert!(rule(&[], &[], &[], AgentState::Idle).matches("whatever"));
    }

    #[test]
    fn first_matching_rule_wins() {
        let m = Manifest {
            name: "t".into(),
            processes: vec![],
            detect: vec![],
            rules: vec![
                rule(&["x"], &[], &[], AgentState::Blocked),
                rule(&["x"], &[], &[], AgentState::Working),
            ],
        };
        assert_eq!(m.evaluate("x").state, AgentState::Blocked);
    }

    #[test]
    fn unmatched_screen_falls_back_to_idle_with_a_reason() {
        let m = Manifest {
            name: "t".into(),
            processes: vec![],
            detect: vec![],
            rules: vec![rule(&["nope"], &[], &[], AgentState::Working)],
        };
        let v = m.evaluate("something else");
        assert_eq!(v.state, AgentState::Idle);
        assert_eq!(v.reason, "no rule matched");
    }

    #[test]
    fn presence_matches_process_basename_or_screen_pattern() {
        let m = parse(
            r#"
name = "t"
processes = ["claude"]
detect = ["the claude ui"]
[[rules]]
name = "r"
state = "idle"
"#,
        )
        .unwrap();

        // Absolute paths still resolve by basename.
        assert!(m.present(Some("claude"), ""));
        assert!(m.present(Some("/usr/local/bin/claude"), ""));
        // But a versioned binary basename does NOT match the process list. This is exactly
        // why `detect` patterns exist: `claude` on disk is a symlink to
        // `.../versions/2.1.227`, so the process name alone cannot be relied on.
        assert!(!m.present(Some("/Users/x/.local/share/claude/versions/2.1.227"), ""));
        assert!(!m.present(Some("zsh"), ""));
        // Screen patterns catch the cases the process name misses.
        assert!(m.present(Some("zsh"), "here is The Claude UI"));
    }

    #[test]
    fn patterns_are_line_anchored_and_case_insensitive() {
        let m = parse(
            r#"
name = "t"
[[rules]]
name = "anchored"
state = "blocked"
any = ["^> yes"]
"#,
        )
        .unwrap();
        // multi_line means `^` matches at the start of any line, not just the input.
        assert_eq!(m.evaluate("first line\n> YES please").state, AgentState::Blocked);
        assert_eq!(m.evaluate("not at line start > yes").state, AgentState::Idle);
    }

    #[test]
    fn a_bad_pattern_is_reported_with_context() {
        let err = parse(
            r#"
name = "t"
[[rules]]
name = "bad"
state = "idle"
any = ["([unclosed"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("t.bad.any"), "{err}");
    }

    #[test]
    fn an_unknown_state_is_rejected() {
        let err = parse(
            r#"
name = "t"
[[rules]]
name = "r"
state = "confused"
"#,
        )
        .unwrap_err();
        // `{:#}` walks the context chain; the offending value is the root cause, while the
        // top-level message only names the rule it came from.
        let chain = format!("{err:#}");
        assert!(chain.contains("confused"), "{chain}");
        assert!(chain.contains("rule \"r\""), "{chain}");
    }
}
