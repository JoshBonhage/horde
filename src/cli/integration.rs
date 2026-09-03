//! Lifecycle-hook integration.
//!
//! With hooks installed, an agent reports its own state and horde stops guessing from the
//! screen. That is the difference between "probably working" and "definitely working".
//!
//! Installing is **merge-safe**: it only ever adds or replaces horde's own hook entries and
//! leaves every other tool's hooks (and the user's) untouched.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;

use crate::proto::AgentState;

/// The skill that teaches an agent to use the bus.
///
/// Hooks tell horde what an agent is doing; this tells the agent what horde can do. Without
/// it an agent receiving `[horde] request #7 … run: horde reply 7 "…"` may treat the command
/// as something to investigate rather than run — observed in testing.
const SKILL: &str = include_str!("../../skills/horde/SKILL.md");

fn skill_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot find your home directory"))?;
    Ok(home.join(".claude").join("skills").join("horde").join("SKILL.md"))
}

/// Write the skill where Claude Code will find it. Idempotent, and overwrites in place so an
/// upgrade ships the current text.
fn install_skill() -> Result<PathBuf> {
    let path = skill_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, SKILL).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Claude Code hook events horde cares about, and the state each implies.
///
/// `SessionStart` reports identity rather than state — it is where the session id for
/// `--resume` comes from.
const CLAUDE_EVENTS: &[(&str, Option<AgentState>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", Some(AgentState::Working)),
    ("PreToolUse", Some(AgentState::Working)),
    ("PostToolUse", Some(AgentState::Working)),
    ("Notification", Some(AgentState::Blocked)),
    ("Stop", Some(AgentState::Idle)),
];

fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot find your home directory"))?;
    Ok(home.join(".claude").join("settings.json"))
}

fn horde_binary() -> Result<String> {
    // Absolute path, so hooks work regardless of the agent's PATH.
    let exe = std::env::current_exe().context("cannot determine the horde binary path")?;
    Ok(exe.to_string_lossy().to_string())
}

/// True when a hook command belongs to horde.
///
/// This decides what install replaces and what uninstall deletes, in a file horde does not own,
/// so it has to be exact. It used to accept any command containing `" hook "` — which is a hook
/// *convention*, not horde's signature. `pre-commit hook run`, `husky hook`, or any other tool
/// following the same convention was claimed by that, and uninstall would have removed it while
/// printing that other tools were left alone.
///
/// So the first token has to be a program called `horde`, and `hook` has to be its subcommand.
/// Quoting is handled because the installed command embeds an absolute path, and a home
/// directory with a space in it would otherwise be unrecognisable to the uninstaller that wrote
/// it.
fn is_horde_command(cmd: &str) -> bool {
    let (program, rest) = match cmd.trim().strip_prefix(['"', '\'']) {
        // A quoted path: the program is everything up to the closing quote.
        Some(quoted) => match quoted.find(['"', '\'']) {
            Some(end) => (&quoted[..end], &quoted[end + 1..]),
            None => return false,
        },
        None => match cmd.trim().split_once(char::is_whitespace) {
            Some((p, rest)) => (p, rest),
            None => return false,
        },
    };
    let is_horde = std::path::Path::new(program)
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("horde"));
    is_horde && rest.split_whitespace().next() == Some("hook")
}

/// Install the hooks and the skill, printing what happened.
pub fn install(agent: &str) -> Result<()> {
    for line in install_reporting(agent)? {
        println!("{line}");
    }
    Ok(())
}

/// The same, returning what happened instead of printing it.
///
/// Two callers are inside the TUI — the settings page and the setup walkthrough — and a
/// `println!` from there lands on top of the frame. ratatui paints only the cells that changed,
/// so the stray line stays on screen until something happens to overwrite it. Printing is the
/// CLI's job; this is the part that does the work.
pub fn install_reporting(agent: &str) -> Result<Vec<String>> {
    if agent != "claude" {
        return Err(anyhow!(
            "only `claude` has a hook integration so far; other agents use screen detection"
        ));
    }
    let path = claude_settings_path()?;
    let bin = horde_binary()?;

    let mut settings: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        _ => json!({}),
    };
    if !settings.is_object() {
        return Err(anyhow!("{} does not contain a JSON object", path.display()));
    }

    // Back up before touching a file horde does not own.
    if path.exists() {
        let backup = path.with_extension("json.horde-backup");
        std::fs::copy(&path, &backup)
            .with_context(|| format!("could not back up to {}", backup.display()))?;
    }

    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        return Err(anyhow!("the `hooks` key in {} is not an object", path.display()));
    }
    let hooks = hooks.as_object_mut().unwrap();

    let mut added = 0;
    for (event, _) in CLAUDE_EVENTS {
        let entry = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let Some(arr) = entry.as_array_mut() else {
            return Err(anyhow!("hooks.{event} in {} is not an array", path.display()));
        };

        // Drop any previous horde entry for this event so reinstalling is idempotent and
        // never accumulates duplicates.
        arr.retain(|group| {
            !group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|inner| {
                    inner.iter().any(|h| {
                        h.get("command").and_then(|c| c.as_str()).is_some_and(is_horde_command)
                    })
                })
                .unwrap_or(false)
        });

        arr.push(json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": format!("{bin} hook claude {event}"),
                "timeout": 5
            }]
        }));
        added += 1;
    }

    let text = serde_json::to_string_pretty(&settings)?;
    // A machine that has never run Claude Code has no ~/.claude yet — and being installed
    // before the agent is exactly what a new horde user does first.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Write via a temp file then rename, so an interrupted write cannot corrupt settings.
    let tmp = path.with_extension("json.horde-tmp");
    std::fs::write(&tmp, text + "\n").with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("could not write {}", path.display()))?;

    let mut report = vec![
        format!("installed {added} hooks into {}", path.display()),
        "existing hooks from other tools were left untouched".to_string(),
    ];
    match install_skill() {
        Ok(p) => report.push(format!("installed the horde skill at {}", p.display())),
        // The hooks are the important half; a missing skill is worth reporting, not fatal.
        Err(e) => report.push(format!("warning: could not install the skill: {e:#}")),
    }
    report.push("restart any running Claude Code sessions for both to take effect".to_string());
    Ok(report)
}

pub fn uninstall(agent: &str) -> Result<()> {
    if agent != "claude" {
        return Err(anyhow!("unknown integration {agent:?}"));
    }
    let path = claude_settings_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            println!("nothing to remove");
            return Ok(());
        }
    };
    let mut settings: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    // The same care install takes. Removing is still editing a file horde does not own, and
    // "it only deleted things" is no comfort if it deleted the wrong ones.
    let backup = path.with_extension("json.horde-backup");
    std::fs::copy(&path, &backup)
        .with_context(|| format!("could not back up to {}", backup.display()))?;

    let mut removed = 0;
    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_event, entry) in hooks.iter_mut() {
            if let Some(arr) = entry.as_array_mut() {
                let before = arr.len();
                arr.retain(|group| {
                    !group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|inner| {
                            inner.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .is_some_and(is_horde_command)
                            })
                        })
                        .unwrap_or(false)
                });
                removed += before - arr.len();
            }
        }
        // Leave no empty arrays behind.
        hooks.retain(|_, v| !v.as_array().map(|a| a.is_empty()).unwrap_or(false));
    }

    // Temp file then rename, so an interrupted uninstall cannot leave a truncated settings.json
    // — the same reason install does it this way.
    let tmp = path.with_extension("json.horde-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&settings)? + "\n")?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("could not write {}", path.display()))?;
    println!("removed {removed} horde hooks from {}", path.display());
    println!("your previous settings are at {}", backup.display());

    if let Ok(p) = skill_path() {
        if p.exists() {
            match std::fs::remove_file(&p) {
                Ok(()) => println!("removed the skill at {}", p.display()),
                Err(e) => eprintln!("warning: could not remove {}: {e}", p.display()),
            }
        }
    }
    Ok(())
}

/// Payload a Claude Code hook delivers on stdin. Only the fields horde reads are named.
#[derive(Debug, Default)]
struct HookInput {
    event: String,
    session_id: Option<String>,
    /// Present when the event came from a subagent rather than the main conversation.
    agent_id: Option<String>,
    /// Which tool is running, on the tool events.
    tool: Option<String>,
    /// The path the tool was given, when it had one. This is what makes "3 files" possible.
    file: Option<String>,
    /// Set when a tool reported failure.
    failed: bool,
}

fn parse_hook_input(text: &str) -> HookInput {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return HookInput::default() };
    let non_empty = |x: Option<&Value>| {
        x.and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string())
    };
    // Tools name their target under different keys depending on the tool, so try the ones
    // that carry a path and ignore the rest.
    let input = v.get("tool_input");
    let file = ["file_path", "path", "notebook_path"]
        .iter()
        .find_map(|k| non_empty(input.and_then(|i| i.get(k))));
    let failed = v
        .get("tool_response")
        .and_then(|r| r.get("success"))
        .and_then(|s| s.as_bool())
        .map(|ok| !ok)
        .unwrap_or(false)
        || v.get("hook_event_name").and_then(|x| x.as_str()) == Some("PostToolUseFailure");

    HookInput {
        event: v.get("hook_event_name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        session_id: non_empty(v.get("session_id")),
        agent_id: non_empty(v.get("agent_id")),
        tool: non_empty(v.get("tool_name")),
        file,
        failed,
    }
}

/// What a hook firing should do.
#[derive(Debug, PartialEq)]
enum HookAction {
    Report { state: Option<AgentState>, session: bool },
    /// Deliberately do nothing.
    Ignore(&'static str),
}

/// Which event this firing really is, preferring Claude's own report over the argument.
fn event_is(payload: &HookInput, want: &str) -> bool {
    payload.event == want
}

/// Decide what to do about one hook firing.
///
/// `event_arg` is what the hook config passed; `payload.event` is what Claude reported. The
/// payload wins when present, since it is the authority on what actually happened.
fn decide(event_arg: &str, payload: &HookInput) -> HookAction {
    let event = if payload.event.is_empty() { event_arg } else { payload.event.as_str() };

    // A subagent's lifecycle says nothing about whether the pane needs you.
    if payload.agent_id.is_some() {
        return HookAction::Ignore("subagent event");
    }
    // SubagentStop can arrive after the main turn has already stopped (recap and
    // away-summary both do this), so treating it as activity would revive an idle pane.
    if event == "SubagentStop" {
        return HookAction::Ignore("SubagentStop cannot revive an idle pane");
    }

    match CLAUDE_EVENTS.iter().find(|(e, _)| *e == event) {
        Some((_, state)) => HookAction::Report { state: *state, session: event == "SessionStart" },
        None => HookAction::Ignore("event not mapped"),
    }
}

/// Called by an installed hook. Reads the payload on stdin and reports to the daemon.
///
/// This must never fail loudly: a hook that errors would surface inside the agent's own
/// output. Every path exits 0.
pub fn run_hook(agent: &str, event: &str) -> Result<()> {
    if agent != "claude" {
        return Ok(());
    }
    let mut text = String::new();
    let _ = std::io::stdin().read_to_string(&mut text);
    let payload = parse_hook_input(&text);

    // Outside a horde pane there is nothing to report to.
    let Ok(pane) = std::env::var("HORDE_PANE") else { return Ok(()) };

    match decide(event, &payload) {
        HookAction::Ignore(_) => Ok(()),
        HookAction::Report { state, session } => {
            let mut params = json!({ "pane": pane.parse::<u32>().unwrap_or(0) });
            if let Some(s) = state {
                params["state"] = Value::from(s.label());
            } else {
                // SessionStart carries no state, but the daemon requires one; `idle` is
                // accurate at the moment a session opens.
                params["state"] = Value::from("idle");
            }
            if session || payload.session_id.is_some() {
                if let Some(sid) = payload.session_id.clone() {
                    params["session"] = Value::from(sid);
                }
            }
            // Activity travels with the state report, so one hook firing is one call.
            if let Some(t) = &payload.tool {
                params["tool"] = Value::from(t.clone());
            }
            if let Some(f) = &payload.file {
                params["file"] = Value::from(f.clone());
            }
            if payload.failed {
                params["tool_failed"] = Value::from(true);
            }
            // A tool starting is the countable moment; PostToolUse only reports the outcome.
            params["counts_tool"] = Value::from(event_is(&payload, "PreToolUse"));
            params["new_turn"] = Value::from(event_is(&payload, "UserPromptSubmit"));

            // Failure here is not the agent's problem; stay silent.
            let _ = super::call("pane.report_agent", params);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What counts as horde's own hook, which is what install replaces and uninstall deletes.
    ///
    /// The bug this pins: the test used to be `contains(" hook ")`, which is the shape of half
    /// the hook commands in the world. Any other tool whose hook read `<tool> hook <event>` was
    /// claimed by horde and removed by `horde integration uninstall claude` — one line below a
    /// promise that other tools were left untouched.
    #[test]
    fn only_hordes_own_hook_commands_are_claimed() {
        for ours in [
            "/Users/j/.local/bin/horde hook claude Stop",
            "horde hook claude PreToolUse",
            "\"/Users/j with space/bin/horde\" hook claude Stop",
            "/opt/horde hook claude Notification",
        ] {
            assert!(is_horde_command(ours), "{ours} is ours");
        }
        for theirs in [
            "pre-commit hook run --all",
            "husky hook pre-push",
            "/usr/local/bin/othertool hook claude Stop",
            "npx some-linter hook",
            "hordelike hook claude Stop",
            // Names horde but does something else: not a hook entry of ours to remove.
            "horde send reviewer \"done\"",
            "",
            "horde",
        ] {
            assert!(!is_horde_command(theirs), "{theirs} is not ours to touch");
        }
    }

    #[test]
    fn the_skill_is_present_and_teaches_the_commands_it_needs_to() {
        // A skill that omits a command an agent is told to run is worse than none.
        assert!(SKILL.len() > 500);
        assert!(SKILL.starts_with("---"), "needs frontmatter to be discovered");
        for needed in [
            "name: horde",
            "description:",
            "HORDE_PANE",
            "horde roster",
            "horde wait",
            "horde spawn",
            "horde pane read",
            "horde digest",
        ] {
            assert!(SKILL.contains(needed), "the skill never mentions {needed}");
        }
    }

    /// The skill is how an agent learns what horde offers, so it has to track what horde
    /// actually answers. A skill that describes a command the daemon refuses spends the
    /// agent's turn on it and then invites it to go looking for the bug, which is exactly what
    /// went wrong while the bus and the board were switched off.
    ///
    /// Now that both are back, the thing worth pinning is the opposite: that the skill teaches
    /// the two rules an agent cannot infer from the command names, because getting either
    /// wrong is what made the board unusable the first time.
    #[test]
    fn the_skill_teaches_the_rules_an_agent_cannot_infer() {
        let lower = SKILL.to_lowercase();
        // Enlistment. An agent that does not know it has to opt in concludes the board is
        // broken when nothing ever arrives.
        assert!(lower.contains("horde task work"), "the skill must teach enlisting");
        assert!(
            lower.contains("enlist"),
            "the skill must say that work is not offered until you enlist"
        );
        // Isolation. Several agents in one repository without worktrees is the failure that
        // loses work silently, and it is not visible from any command's name.
        assert!(
            lower.contains("--worktree"),
            "the skill must teach worktrees before it teaches spawning a fleet"
        );
        // Giving work back. An agent that only knows `claim` and `done` holds a task it cannot
        // finish until it exits, and a claimed task nobody is working looks handled.
        assert!(
            lower.contains("horde task release"),
            "the skill must teach putting work back, not only taking and finishing it"
        );
        // Roles gate what a claim returns. An agent that does not know this reads an empty
        // claim as an empty board, concludes there is nothing to do, and stops — while work
        // for somebody else sits there and work for *it* may arrive a second later.
        assert!(
            lower.contains("--role"),
            "the skill must teach that work can name the role that takes it"
        );
        assert!(
            lower.contains("cannot change your own role"),
            "the skill must say a role is not self-assigned, or an agent will try"
        );
        // Nothing may claim the pause is still on.
        assert!(!lower.contains("paused"), "the bus and the board are back on");
    }

    #[test]
    fn maps_events_to_states() {
        let p = HookInput::default();
        assert_eq!(
            decide("UserPromptSubmit", &p),
            HookAction::Report { state: Some(AgentState::Working), session: false }
        );
        assert_eq!(
            decide("Notification", &p),
            HookAction::Report { state: Some(AgentState::Blocked), session: false }
        );
        assert_eq!(
            decide("Stop", &p),
            HookAction::Report { state: Some(AgentState::Idle), session: false }
        );
        assert_eq!(
            decide("SessionStart", &p),
            HookAction::Report { state: None, session: true }
        );
    }

    #[test]
    fn subagent_stop_never_revives_an_idle_pane() {
        // Claude's recap and away-summary can emit SubagentStop after the turn ended.
        // Treating it as activity would flip a finished pane back to working.
        let p = HookInput { event: "SubagentStop".into(), ..Default::default() };
        assert!(matches!(decide("Stop", &p), HookAction::Ignore(_)));
    }

    #[test]
    fn subagent_events_are_ignored() {
        let p = HookInput {
            event: "Stop".into(),
            agent_id: Some("sub-1".into()),
            ..Default::default()
        };
        assert!(matches!(decide("Stop", &p), HookAction::Ignore(_)));
    }

    #[test]
    fn the_payload_event_wins_over_the_argument() {
        // The hook config could be stale; Claude's own report is authoritative.
        let p = HookInput { event: "Notification".into(), ..Default::default() };
        assert_eq!(
            decide("Stop", &p),
            HookAction::Report { state: Some(AgentState::Blocked), session: false }
        );
    }

    #[test]
    fn unmapped_events_are_ignored() {
        // PostToolUse is mapped now (it is how tool failures are counted), so use an event
        // horde genuinely does not care about.
        let p = HookInput { event: "PreCompact".into(), ..Default::default() };
        assert!(matches!(decide("PreCompact", &p), HookAction::Ignore(_)));
    }

    #[test]
    fn tool_activity_is_extracted_from_a_real_payload() {
        let p = parse_hook_input(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Edit",
                "tool_input":{"file_path":"/x/src/bus.rs","old_string":"a"}}"#,
        );
        assert_eq!(p.tool.as_deref(), Some("Edit"));
        assert_eq!(p.file.as_deref(), Some("/x/src/bus.rs"));
        assert!(!p.failed);

        // Tools that touch no file still count as a tool call.
        let p = parse_hook_input(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash",
                "tool_input":{"command":"cargo test"}}"#,
        );
        assert_eq!(p.tool.as_deref(), Some("Bash"));
        assert!(p.file.is_none());

        // A failed tool is recognised from the response.
        let p = parse_hook_input(
            r#"{"hook_event_name":"PostToolUse","tool_name":"Edit",
                "tool_response":{"success":false}}"#,
        );
        assert!(p.failed, "a failed tool must be counted as one");
    }

    #[test]
    fn parses_a_realistic_payload() {
        let p = parse_hook_input(
            r#"{"hook_event_name":"SessionStart","session_id":"abc-123",
                "transcript_path":"/tmp/x.jsonl","source":"startup"}"#,
        );
        assert_eq!(p.event, "SessionStart");
        assert_eq!(p.session_id.as_deref(), Some("abc-123"));
        assert!(p.agent_id.is_none());
    }

    #[test]
    fn garbage_or_empty_stdin_does_not_panic() {
        assert_eq!(parse_hook_input("").event, "");
        assert_eq!(parse_hook_input("not json").event, "");
        assert_eq!(parse_hook_input("{}").event, "");
        // An empty session id is treated as absent rather than a valid id.
        assert!(parse_hook_input(r#"{"session_id":""}"#).session_id.is_none());
    }

    #[test]
    fn horde_commands_are_recognised_for_idempotent_reinstall() {
        assert!(is_horde_command("/usr/local/bin/horde hook claude Stop"));
        assert!(!is_horde_command("bash '/Users/josh/.claude/hooks/herdr-agent-state.sh' session"));
        assert!(!is_horde_command("echo hello"));
    }

    #[test]
    fn install_is_merge_safe_and_idempotent() {
        // Simulate a settings file that already has another tool's hook, mirroring the
        // real-world case of herdr being installed alongside.
        let mut settings = json!({
            "permissions": { "allow": ["Bash"] },
            "hooks": {
                "SessionStart": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "bash '/Users/josh/.claude/hooks/herdr-agent-state.sh' session"
                    }]
                }]
            }
        });

        let apply = |settings: &mut Value| {
            let hooks = settings.get_mut("hooks").unwrap().as_object_mut().unwrap();
            for (event, _) in CLAUDE_EVENTS {
                let entry = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
                let arr = entry.as_array_mut().unwrap();
                arr.retain(|g| {
                    !g.get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|inner| {
                            inner.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .is_some_and(is_horde_command)
                            })
                        })
                        .unwrap_or(false)
                });
                arr.push(json!({
                    "matcher": "*",
                    "hooks": [{ "type": "command", "command": format!("/bin/horde hook claude {event}") }]
                }));
            }
        };

        apply(&mut settings);
        let after_first = settings.clone();
        apply(&mut settings);

        assert_eq!(settings, after_first, "reinstalling must not accumulate duplicates");

        // Unrelated settings survive.
        assert!(settings.get("permissions").is_some());
        // And so does the other tool's hook.
        let session = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session.len(), 2, "herdr's hook must survive alongside horde's");
        assert!(session.iter().any(|g| g["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("herdr-agent-state.sh")));
    }
}
