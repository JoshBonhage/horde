//! Screen manifests: how horde works out what an agent is doing by looking at its UI.
//!
//! This is the fallback tier. When an agent reports through lifecycle hooks (see
//! `integration install`), those reports are authoritative and manifests are not consulted
//! at all — one status authority per pane, never two.
//!
//! Bundled manifests are compiled in; `~/.config/horde/agents/<name>.toml` overrides one
//! wholesale, so a broken upstream pattern is always fixable without rebuilding.
//!
//! # Regions
//!
//! A rule matches against a *region*, not the whole snapshot. That matters more than it
//! sounds, because the two ways screen detection goes wrong are both regional:
//!
//! * **Scrollback staleness.** An agent that printed "Thinking…" ten minutes ago still has
//!   those words on screen. A live-status rule scoped to the whole snapshot keeps firing
//!   forever, and the spinner never stops.
//! * **Truncation.** Agents elide their own status lines to fit the pane, so at narrow widths
//!   the very marker a rule depends on is never rendered.
//!
//! `osc_title` sidesteps both. The terminal title is set with an escape sequence rather than
//! drawn into the grid, so it is neither truncated nor stale. Claude Code puts a spinner
//! glyph there while working and a different glyph at rest, which makes it the most reliable
//! signal available without hooks.
//!
//! # Priority
//!
//! Rules carry an explicit priority rather than relying on declaration order. Order
//! dependence is how a manifest becomes unmaintainable: inserting a rule silently changes
//! what the ones below it mean.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use regex::{Regex, RegexBuilder};
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

// ---------------------------------------------------------------------------
// What a rule looks at
// ---------------------------------------------------------------------------

/// The slice of a pane a rule is tested against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Region {
    /// The terminal title, set by escape sequence. Immune to width and to scrollback.
    OscTitle,
    /// The whole detection snapshot.
    WholeRecent,
    /// The last N non-empty lines. Blanks are skipped so a trailing gap cannot push the
    /// status area out of range.
    BottomLines(usize),
    /// Everything after the last horizontal rule, where most agents put the current dialog.
    AfterLastRule,
    /// Between the last two horizontal rules: the composer box itself.
    PromptBoxBody,
}

impl Region {
    fn parse(s: &str) -> Result<Region> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("bottom_non_empty_lines(") {
            let n = rest
                .strip_suffix(')')
                .ok_or_else(|| anyhow!("unclosed bottom_non_empty_lines("))?
                .trim()
                .parse::<usize>()
                .context("bottom_non_empty_lines needs a number")?;
            return Ok(Region::BottomLines(n));
        }
        Ok(match s {
            "osc_title" => Region::OscTitle,
            "whole_recent" => Region::WholeRecent,
            "after_last_horizontal_rule" => Region::AfterLastRule,
            "prompt_box_body" => Region::PromptBoxBody,
            other => {
                return Err(anyhow!(
                    "unknown region {other:?} (osc_title, whole_recent, \
                     bottom_non_empty_lines(N), after_last_horizontal_rule, prompt_box_body)"
                ))
            }
        })
    }

    /// Extract this region from a pane.
    fn slice(&self, screen: &Screen<'_>) -> String {
        match self {
            Region::OscTitle => screen.osc_title.to_string(),
            Region::WholeRecent => screen.lines.join("\n"),
            Region::BottomLines(n) => {
                let non_empty: Vec<&str> = screen
                    .lines
                    .iter()
                    .map(|l| l.as_str())
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                let start = non_empty.len().saturating_sub(*n);
                non_empty[start..].join("\n")
            }
            Region::AfterLastRule => match last_rule(screen.lines) {
                Some(i) => screen.lines[i + 1..].join("\n"),
                None => screen.lines.join("\n"),
            },
            Region::PromptBoxBody => {
                let rules = rule_lines(screen.lines);
                match rules.len() {
                    n if n >= 2 => {
                        let (a, b) = (rules[n - 2], rules[n - 1]);
                        screen.lines[a + 1..b].join("\n")
                    }
                    1 => screen.lines[rules[0] + 1..].join("\n"),
                    _ => screen.lines.join("\n"),
                }
            }
        }
    }
}

/// A line made entirely of box-drawing horizontals is a rule, not content.
fn is_rule_line(l: &str) -> bool {
    let t = l.trim();
    if t.chars().count() < 4 {
        return false;
    }
    t.chars().all(|c| matches!(c, '─' | '━' | '-' | '═' | '╌' | '┄' | '┈'))
}

fn rule_lines(lines: &[String]) -> Vec<usize> {
    lines.iter().enumerate().filter(|(_, l)| is_rule_line(l)).map(|(i, _)| i).collect()
}

fn last_rule(lines: &[String]) -> Option<usize> {
    rule_lines(lines).pop()
}

/// What a rule is matched against.
pub struct Screen<'a> {
    pub lines: &'a [String],
    pub osc_title: &'a str,
}

// ---------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPred {
    /// Every substring must be present. Case-insensitive, with no regex escaping to get
    /// wrong — which is what most patterns actually need.
    #[serde(default)]
    contains: Vec<String>,
    /// Any of these matching the region.
    #[serde(default)]
    regex: Vec<String>,
    /// Any of these matching, read as line-anchored.
    #[serde(default)]
    line_regex: Vec<String>,
    #[serde(default)]
    any: Vec<RawPred>,
    #[serde(default)]
    all: Vec<RawPred>,
    #[serde(default)]
    not: Vec<RawPred>,
}

#[derive(Debug, Clone, Default)]
pub struct Pred {
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
    any: Vec<Pred>,
    all: Vec<Pred>,
    not: Vec<Pred>,
}

impl Pred {
    fn compile(raw: &RawPred, ctx: &str) -> Result<Pred> {
        Ok(Pred {
            contains: raw.contains.iter().map(|s| s.to_lowercase()).collect(),
            regex: compile(&raw.regex, &format!("{ctx}.regex"))?,
            line_regex: compile(&raw.line_regex, &format!("{ctx}.line_regex"))?,
            any: raw
                .any
                .iter()
                .enumerate()
                .map(|(i, p)| Pred::compile(p, &format!("{ctx}.any[{i}]")))
                .collect::<Result<_>>()?,
            all: raw
                .all
                .iter()
                .enumerate()
                .map(|(i, p)| Pred::compile(p, &format!("{ctx}.all[{i}]")))
                .collect::<Result<_>>()?,
            not: raw
                .not
                .iter()
                .enumerate()
                .map(|(i, p)| Pred::compile(p, &format!("{ctx}.not[{i}]")))
                .collect::<Result<_>>()?,
        })
    }

    fn is_empty(&self) -> bool {
        self.contains.is_empty()
            && self.regex.is_empty()
            && self.line_regex.is_empty()
            && self.any.is_empty()
            && self.all.is_empty()
            && self.not.is_empty()
    }

    /// `contains` is conjunctive, pattern lists are disjunctive, and the nested forms mean
    /// what they say. An empty predicate matches nothing, so a rule cannot fire by accident.
    fn matches(&self, hay: &str, lower: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        if !self.contains.iter().all(|c| lower.contains(c)) {
            return false;
        }
        if !self.regex.is_empty() && !self.regex.iter().any(|r| r.is_match(hay)) {
            return false;
        }
        if !self.line_regex.is_empty() && !self.line_regex.iter().any(|r| r.is_match(hay)) {
            return false;
        }
        if !self.any.is_empty() && !self.any.iter().any(|p| p.matches(hay, lower)) {
            return false;
        }
        if !self.all.iter().all(|p| p.matches(hay, lower)) {
            return false;
        }
        if self.not.iter().any(|p| p.matches(hay, lower)) {
            return false;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Rules and manifests
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    state: String,
    #[serde(default = "default_priority")]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    /// Match, but leave the state alone. For UI that would otherwise be misread — a
    /// transcript viewer or a model picker looks a lot like a permission prompt.
    #[serde(default)]
    skip_state_update: bool,
    #[serde(flatten)]
    pred: RawPred,
}

fn default_priority() -> i32 {
    500
}

fn default_region() -> String {
    "whole_recent".to_string()
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub state: AgentState,
    pub priority: i32,
    pub region: Region,
    pub skip_state_update: bool,
    pred: Pred,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    /// Foreground process names that indicate this agent.
    pub processes: Vec<String>,
    /// Patterns proving this agent's UI is on screen. Must be unique to this agent: anything
    /// several agents show would make two manifests claim one pane.
    detect: Vec<Regex>,
    /// Highest priority first.
    pub rules: Vec<Rule>,
}

/// Why a state was chosen — surfaced by `horde agent explain`.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub state: AgentState,
    pub reason: String,
}

impl Manifest {
    /// Is this agent's process in the foreground? A definite answer, when available.
    pub fn matches_process(&self, process: Option<&str>) -> bool {
        let Some(p) = process else { return false };
        let base = p.rsplit('/').next().unwrap_or(p);
        self.processes.iter().any(|want| want == base)
    }

    /// Does this agent's UI appear to be on screen? A guess, used only when the process name
    /// tells us nothing.
    pub fn matches_screen(&self, screen: &str) -> bool {
        self.detect.iter().any(|r| r.is_match(screen))
    }

    /// Either signal. Kept for diagnostics; detection itself prefers the process.
    pub fn present(&self, process: Option<&str>, screen: &str) -> bool {
        self.matches_process(process) || self.matches_screen(screen)
    }

    /// Highest-priority matching rule wins.
    ///
    /// Returns `None` when the winning rule says to leave the state alone, and an `idle`
    /// fallback when nothing matches — a known agent sitting at a prompt horde does not
    /// recognise is far more likely idle than indeterminate.
    pub fn evaluate(&self, screen: &Screen<'_>) -> Option<Verdict> {
        let mut cache: HashMap<Region, (String, String)> = HashMap::new();
        for rule in &self.rules {
            let (hay, lower) = cache.entry(rule.region.clone()).or_insert_with(|| {
                let h = rule.region.slice(screen);
                let l = h.to_lowercase();
                (h, l)
            });
            if rule.pred.matches(hay, lower) {
                if rule.skip_state_update {
                    return None;
                }
                return Some(Verdict { state: rule.state, reason: rule.id.clone() });
            }
        }
        Some(Verdict { state: AgentState::Idle, reason: "no rule matched".into() })
    }
}

fn compile(patterns: &[String], ctx: &str) -> Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| {
            RegexBuilder::new(p)
                // Patterns describe lines of a terminal, so `^`/`$` mean line boundaries and
                // case rarely carries meaning in a TUI.
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

pub fn parse(text: &str) -> Result<Manifest> {
    let raw: RawManifest = toml::from_str(text)?;
    let mut rules = Vec::new();
    for r in &raw.rules {
        let ctx = format!("{}.{}", raw.name, r.id);
        let pred = Pred::compile(&r.pred, &ctx)?;
        if pred.is_empty() {
            return Err(anyhow!("{ctx}: rule has no conditions, so it would never fire"));
        }
        rules.push(Rule {
            state: parse_state(&r.state).with_context(|| format!("rule {ctx}"))?,
            region: Region::parse(&r.region).with_context(|| format!("rule {ctx}"))?,
            priority: r.priority,
            skip_state_update: r.skip_state_update,
            id: r.id.clone(),
            pred,
        });
    }
    // Highest priority first. `sort_by` is stable, so declaration order breaks ties and a
    // manifest stays predictable when two rules share a priority.
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));

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
            Err(e) => warnings.push(format!("bundled manifest {name} is invalid: {e:#}")),
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
            Err(e) => warnings.push(format!("{}: {e:#}", path.display())),
        }
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen<'a>(lines: &'a [String], title: &'a str) -> Screen<'a> {
        Screen { lines, osc_title: title }
    }

    fn to_lines(s: &str) -> Vec<String> {
        s.lines().map(|l| l.to_string()).collect()
    }

    /// Real screens and titles captured from a live Claude Code session.
    struct Fixture {
        label: &'static str,
        body: &'static str,
        title: &'static str,
        want: AgentState,
    }

    const CLAUDE: &[Fixture] = &[
        Fixture {
            label: "idle",
            body: include_str!("../../tests/fixtures/claude-idle.txt"),
            title: "✳ Claude Code",
            want: AgentState::Idle,
        },
        Fixture {
            label: "working",
            body: include_str!("../../tests/fixtures/claude-working.txt"),
            title: "◐ count from 1 to 25",
            want: AgentState::Working,
        },
        Fixture {
            // The case screen scraping cannot see at all: the pane is too narrow for the
            // status marker to be rendered. The title is unaffected.
            label: "working-narrow",
            body: include_str!("../../tests/fixtures/claude-working-narrow.txt"),
            title: "◑ do a thing",
            want: AgentState::Working,
        },
        Fixture {
            label: "blocked",
            body: include_str!("../../tests/fixtures/claude-blocked.txt"),
            title: "✳ Claude Code",
            want: AgentState::Blocked,
        },
        Fixture {
            label: "finished-after-thinking",
            body: include_str!("../../tests/fixtures/claude-finished-after-thinking.txt"),
            title: "✳ count from 1 to 25",
            want: AgentState::Idle,
        },
    ];

    #[test]
    fn claude_reads_every_real_screen_correctly() {
        let (all, w) = load_all(Path::new("/nonexistent"));
        assert!(w.is_empty(), "{w:?}");
        let claude = &all["claude"];
        for f in CLAUDE {
            let lines = to_lines(f.body);
            let v = claude
                .evaluate(&screen(&lines, f.title))
                .unwrap_or_else(|| panic!("{}: a rule suppressed the state", f.label));
            assert_eq!(v.state, f.want, "{}: decided by {}", f.label, v.reason);
        }
    }

    /// The whole point of the `osc_title` region: a pane too narrow to render the status
    /// marker still reports the right state.
    #[test]
    fn a_narrow_pane_is_read_from_the_title_not_the_grid() {
        let (all, _) = load_all(Path::new("/nonexistent"));
        let claude = &all["claude"];
        let lines = to_lines(include_str!("../../tests/fixtures/claude-working-narrow.txt"));

        let v = claude.evaluate(&screen(&lines, "◐ doing a thing")).unwrap();
        assert_eq!(v.state, AgentState::Working);
        assert!(v.reason.contains("title"), "should be decided by the title: {}", v.reason);

        // Both spinner families Claude has shipped are recognised.
        for glyph in ['◐', '◑', '◒', '◓', '⠋', '⣾'] {
            let t = format!("{glyph} something");
            let v = claude.evaluate(&screen(&lines, &t)).unwrap();
            assert_eq!(v.state, AgentState::Working, "title glyph {glyph:?}");
        }
        // The resting glyph is not mistaken for a spinner. Tested against an idle body,
        // because the narrow *working* body genuinely contains the status marker and would
        // rightly win — contradictory inputs are not what this is checking.
        let idle_body = to_lines(include_str!("../../tests/fixtures/claude-idle.txt"));
        let v = claude.evaluate(&screen(&idle_body, "✳ Claude Code")).unwrap();
        assert_eq!(v.state, AgentState::Idle, "the idle glyph must not read as working");
    }

    /// The idle title is a *weak* signal: it only means "not generating", and an agent
    /// waiting on a permission prompt is not generating either. So every blocker outranks it,
    /// while the spinner outranks everything.
    #[test]
    fn the_idle_title_never_masks_a_blocked_prompt() {
        let (all, _) = load_all(Path::new("/nonexistent"));
        let claude = &all["claude"];
        let blocked = to_lines(include_str!("../../tests/fixtures/claude-blocked.txt"));

        // This is the real pairing: Claude shows `✳` while waiting for you to answer.
        let v = claude.evaluate(&screen(&blocked, "✳ Claude Code")).unwrap();
        assert_eq!(v.state, AgentState::Blocked, "decided by {}", v.reason);

        let by_priority: Vec<(&str, i32)> =
            claude.rules.iter().map(|r| (r.id.as_str(), r.priority)).collect();
        let idle_title = by_priority.iter().find(|(id, _)| *id == "osc_title_idle").unwrap().1;
        let working_title =
            by_priority.iter().find(|(id, _)| *id == "osc_title_working").unwrap().1;
        for (id, prio) in &by_priority {
            if id.contains("permission") || id.contains("trust") || id.contains("proceed") {
                assert!(*prio > idle_title, "{id} must outrank the idle title");
                assert!(*prio < working_title, "the spinner must outrank {id}");
            }
        }
    }

    #[test]
    fn no_other_manifest_claims_a_claude_screen() {
        let (all, _) = load_all(Path::new("/nonexistent"));
        for f in CLAUDE {
            let claimants: Vec<&str> = all
                .values()
                .filter(|m| m.matches_screen(f.body))
                .map(|m| m.name.as_str())
                .collect();
            assert_eq!(claimants, vec!["claude"], "claude {} claimed by {claimants:?}", f.label);
        }
    }

    #[test]
    fn manifests_do_not_match_phrases_several_agents_share() {
        let (all, _) = load_all(Path::new("/nonexistent"));
        for m in all.values() {
            for g in ["esc to interrupt", "esc to cancel", "Thinking", "Working"] {
                assert!(!m.matches_screen(g), "{}'s detect matches generic {g:?}", m.name);
            }
        }
    }

    #[test]
    fn region_parsing_accepts_the_documented_forms_and_rejects_others() {
        assert_eq!(Region::parse("osc_title").unwrap(), Region::OscTitle);
        assert_eq!(Region::parse("whole_recent").unwrap(), Region::WholeRecent);
        assert_eq!(Region::parse("bottom_non_empty_lines(4)").unwrap(), Region::BottomLines(4));
        assert_eq!(Region::parse("after_last_horizontal_rule").unwrap(), Region::AfterLastRule);
        assert_eq!(Region::parse("prompt_box_body").unwrap(), Region::PromptBoxBody);
        assert!(Region::parse("nonsense").is_err());
        assert!(Region::parse("bottom_non_empty_lines(x)").is_err());
        assert!(Region::parse("bottom_non_empty_lines(3").is_err());
    }

    #[test]
    fn bottom_lines_skips_blanks_so_a_trailing_gap_cannot_hide_the_status_area() {
        let lines = to_lines("keep\nBUSY\n\n\n\n\n");
        let got = Region::BottomLines(2).slice(&screen(&lines, ""));
        assert_eq!(got, "keep\nBUSY", "blank lines must not consume the budget");
    }

    #[test]
    fn after_last_rule_and_prompt_box_body_split_a_composer_correctly() {
        let lines = to_lines(
            "transcript line\n──────────────\n❯ typed text\n──────────────\n  mode line here",
        );
        let s = screen(&lines, "");
        assert_eq!(Region::AfterLastRule.slice(&s), "  mode line here");
        assert_eq!(Region::PromptBoxBody.slice(&s), "❯ typed text");
        // With no rules at all, both degrade to the whole snapshot rather than to nothing.
        let plain = to_lines("just text");
        assert_eq!(Region::AfterLastRule.slice(&screen(&plain, "")), "just text");
        assert_eq!(Region::PromptBoxBody.slice(&screen(&plain, "")), "just text");
    }

    #[test]
    fn rule_lines_are_recognised_but_ordinary_text_is_not() {
        assert!(is_rule_line("────────"));
        assert!(is_rule_line("  ---- "));
        assert!(is_rule_line("════════"));
        assert!(!is_rule_line("─ text ─"));
        assert!(!is_rule_line("---"), "too short to be a rule");
        assert!(!is_rule_line(""));
    }

    fn pred(toml_src: &str) -> Pred {
        let raw: RawPred = toml::from_str(toml_src).unwrap();
        Pred::compile(&raw, "t").unwrap()
    }

    fn m(p: &Pred, hay: &str) -> bool {
        p.matches(hay, &hay.to_lowercase())
    }

    #[test]
    fn contains_is_conjunctive_and_case_insensitive() {
        let p = pred(r#"contains = ["do you want", "proceed"]"#);
        assert!(m(&p, "Do You Want to proceed?"));
        assert!(!m(&p, "do you want to continue"), "every substring must be present");
    }

    #[test]
    fn pattern_lists_are_disjunctive() {
        let p = pred(r#"line_regex = ['^yes', '^no']"#);
        assert!(m(&p, "other\nyes please"));
        assert!(m(&p, "no thanks"));
        assert!(!m(&p, "maybe"));
    }

    #[test]
    fn nested_any_all_not_compose() {
        let p = pred(
            r#"
contains = ["prompt"]
any = [{ contains = ["yes"] }, { contains = ["no"] }]
not = [{ contains = ["cancelled"] }]
"#,
        );
        assert!(m(&p, "prompt: yes"));
        assert!(m(&p, "prompt: no"));
        assert!(!m(&p, "prompt only"), "the any branch is required");
        assert!(!m(&p, "prompt: yes, cancelled"), "not must veto");
    }

    #[test]
    fn an_empty_predicate_never_matches() {
        // A rule that fired on everything would be worse than a missing rule.
        let p = Pred::default();
        assert!(!m(&p, "anything at all"));
        assert!(!m(&p, ""));
    }

    #[test]
    fn a_rule_with_no_conditions_is_rejected_at_parse_time() {
        let err = format!(
            "{:#}",
            parse("name=\"t\"\n[[rules]]\nid=\"empty\"\nstate=\"idle\"").unwrap_err()
        );
        assert!(err.contains("no conditions"), "{err}");
    }

    #[test]
    fn highest_priority_wins_regardless_of_declaration_order() {
        let man = parse(
            r#"
name = "t"
[[rules]]
id = "low"
state = "idle"
priority = 10
contains = ["x"]

[[rules]]
id = "high"
state = "blocked"
priority = 900
contains = ["x"]
"#,
        )
        .unwrap();
        let lines = to_lines("x");
        let v = man.evaluate(&screen(&lines, "")).unwrap();
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.reason, "high");
    }

    #[test]
    fn skip_state_update_suppresses_rather_than_deciding() {
        // A transcript viewer or model picker looks like a permission prompt. Matching one
        // must leave the previous state alone, not flip the pane to blocked.
        let man = parse(
            r#"
name = "t"
[[rules]]
id = "viewer"
state = "unknown"
priority = 1000
skip_state_update = true
contains = ["showing transcript"]

[[rules]]
id = "looks-blocked"
state = "blocked"
priority = 500
contains = ["do you want"]
"#,
        )
        .unwrap();
        let lines = to_lines("showing transcript — do you want to scroll?");
        assert!(man.evaluate(&screen(&lines, "")).is_none(), "should suppress");

        let lines = to_lines("do you want to proceed?");
        assert_eq!(man.evaluate(&screen(&lines, "")).unwrap().state, AgentState::Blocked);
    }

    #[test]
    fn an_unmatched_screen_falls_back_to_idle_with_a_reason() {
        let man = parse(
            "name=\"t\"\n[[rules]]\nid=\"r\"\nstate=\"working\"\ncontains=[\"nope\"]",
        )
        .unwrap();
        let lines = to_lines("something else");
        let v = man.evaluate(&screen(&lines, "")).unwrap();
        assert_eq!(v.state, AgentState::Idle);
        assert_eq!(v.reason, "no rule matched");
    }

    #[test]
    fn every_bundled_manifest_parses_and_is_sorted_by_priority() {
        for (name, text) in BUNDLED {
            let man = parse(text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(&man.name, name);
            assert!(!man.rules.is_empty(), "{name} has no rules");
            let mut prev = i32::MAX;
            for r in &man.rules {
                assert!(r.priority <= prev, "{name} rules are not sorted");
                prev = r.priority;
            }
        }
    }

    #[test]
    fn load_all_returns_bundled_manifests_when_the_override_dir_is_missing() {
        let (all, warnings) = load_all(Path::new("/nonexistent/horde/agents"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(all.len(), BUNDLED.len());
        assert!(all.contains_key("claude"));
    }

    #[test]
    fn a_user_override_replaces_the_bundled_manifest() {
        let dir = std::env::temp_dir().join("horde-manifest-override-v2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("claude.toml"),
            "name=\"claude\"\nprocesses=[\"claude\"]\n[[rules]]\nid=\"only\"\nstate=\"working\"\ncontains=[\"ZZZ\"]\n",
        )
        .unwrap();
        let (all, warnings) = load_all(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(all["claude"].rules.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_override_warns_and_keeps_the_bundled_version() {
        let dir = std::env::temp_dir().join("horde-manifest-broken-v2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("claude.toml"), "name = \"claude\"\n[[rules]]\nbroken").unwrap();
        let (all, warnings) = load_all(&dir);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(all["claude"].rules.len() > 1, "bundled rules should survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_pattern_names_the_rule_it_came_from() {
        let err = format!(
            "{:#}",
            parse("name=\"t\"\n[[rules]]\nid=\"bad\"\nstate=\"idle\"\nregex=[\"([unclosed\"]")
                .unwrap_err()
        );
        assert!(err.contains("t.bad"), "{err}");
    }

    #[test]
    fn an_unknown_state_or_region_is_rejected_with_context() {
        let e = format!(
            "{:#}",
            parse("name=\"t\"\n[[rules]]\nid=\"r\"\nstate=\"confused\"\ncontains=[\"x\"]")
                .unwrap_err()
        );
        assert!(e.contains("confused"), "{e}");
        let e = format!(
            "{:#}",
            parse(
                "name=\"t\"\n[[rules]]\nid=\"r\"\nstate=\"idle\"\nregion=\"nope\"\ncontains=[\"x\"]"
            )
            .unwrap_err()
        );
        assert!(e.contains("nope"), "{e}");
    }

    #[test]
    fn presence_prefers_process_but_falls_back_to_screen() {
        let man = parse(
            "name=\"t\"\nprocesses=[\"claude\"]\ndetect=[\"the unique ui string\"]\n\
             [[rules]]\nid=\"r\"\nstate=\"idle\"\ncontains=[\"x\"]",
        )
        .unwrap();
        assert!(man.matches_process(Some("claude")));
        assert!(man.matches_process(Some("/usr/local/bin/claude")));
        // A versioned binary basename does not match, which is exactly why detect exists.
        assert!(!man.matches_process(Some("/x/claude/versions/2.1.227")));
        assert!(!man.matches_process(Some("zsh")));
        assert!(man.matches_screen("here is The Unique UI String"));
        assert!(!man.matches_screen("something else"));
    }
}
