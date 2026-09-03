//! Agent detection and the `done`/seen state machine.
//!
//! Two tiers, and only ever one authority per pane:
//!
//! 1. **Lifecycle hooks** (`horde integration install claude`). An agent reports its own
//!    state through `horde pane report-agent`. While such reports are fresh, they win
//!    outright and the screen manifest is not consulted.
//! 2. **Screen manifests**. horde reads the foreground process plus the live bottom of the
//!    pane buffer and matches TOML rules against it.
//!
//! `done` is derived rather than reported: an agent that finishes while you are not looking
//! at it is `done` until you do look, which is what makes the sidebar worth glancing at.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::proto::{AgentClass, AgentState, Event, PaneId};

use super::manifest::{self, Manifest, Screen, Verdict};
use super::state::{AgentRuntime, Session};

/// How long after a nudge the same agent may earn another.
///
/// Long enough to cover a compaction and the turn that follows it, which is the soonest a
/// genuinely fresh fill-up could arrive. Short enough that an agent left running all day and
/// ignoring the nudge is eventually told again.
pub const MEMORY_NUDGE_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// How long a hook report stays authoritative. If an integration is installed but the
/// agent stops reporting (crash, killed mid-run), horde falls back to the screen manifest
/// rather than showing a stale state forever.
const HOOK_TTL: Duration = Duration::from_secs(90);

/// How often to re-probe which process is in the foreground of each pane.
///
/// Deliberately slower than screen detection. Identifying the process means forking `ps`
/// once per pane, and what is running in a pane changes on the timescale of a person typing
/// a command — whereas what it is *doing* changes second to second. Coupling the two made a
/// detached daemon fork several times a second forever.
const PROCESS_INTERVAL: Duration = Duration::from_secs(2);

pub struct Detector {
    manifests: HashMap<String, Manifest>,
    pub warnings: Vec<String>,
    /// Cached foreground process name per pane, refreshed on each scan.
    processes: HashMap<PaneId, String>,
    /// Last hook report per pane, used to decide whether hooks still hold authority.
    hook_reports: HashMap<PaneId, Instant>,
    /// Agent kind last reported by a hook, so a scan can keep the agent even when neither
    /// the process name nor the screen identifies it.
    hook_kinds: HashMap<PaneId, String>,
    /// When the foreground process cache was last refreshed.
    last_process_probe: Option<Instant>,
}

impl Detector {
    pub fn new(cfg: &Config) -> Detector {
        let _ = cfg;
        let (manifests, warnings) = manifest::load_all(&crate::config::config_dir().join("agents"));
        Detector {
            manifests,
            warnings,
            processes: HashMap::new(),
            hook_reports: HashMap::new(),
            hook_kinds: HashMap::new(),
            last_process_probe: None,
        }
    }

    pub fn reload(&mut self) {
        let (m, w) = manifest::load_all(&crate::config::config_dir().join("agents"));
        self.manifests = m;
        self.warnings = w;
    }

    pub fn manifest_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.manifests.keys().cloned().collect();
        v.sort();
        v
    }

    /// An agent reported its own state. Hooks are authoritative, so this overrides whatever
    /// the screen says.
    pub fn report(
        &mut self,
        session: &mut Session,
        pane: PaneId,
        state: AgentState,
        session_id: Option<String>,
    ) -> Option<Event> {
        self.hook_reports.insert(pane, Instant::now());
        let focused = session.focused_pane() == Some(pane);
        let kind = self
            .processes
            .get(&pane)
            .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
            .unwrap_or_else(|| "agent".to_string());

        let names = self.taken_names(session, pane);
        let pane_ref = session.panes.get_mut(&pane)?;
        // Remember which agent reported, so a scan that cannot identify the pane by process
        // or screen still knows what is in it.
        self.hook_kinds.insert(pane, kind.clone());
        let cmd_kind =
            pane_ref.cmd.split_whitespace().next().unwrap_or("agent").rsplit('/').next()?.to_string();
        let kind = if self.manifests.contains_key(&cmd_kind) { cmd_kind } else { kind };

        let agent = pane_ref.agent.get_or_insert_with(|| AgentRuntime {
            name: unique_name(&kind, &names),
            kind: kind.clone(),
            // Only an agent has lifecycle hooks to report through, so a pane that reports is
            // one by definition.
            class: AgentClass::Agent,
            state,
            since: Instant::now(),
            authority: "hook".into(),
            reason: "reported by integration".into(),
            seen: focused,
            session_id: None,
            queued: Vec::new(),
            question: None,
            endpoint: None,
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                memory_nudged: None,
                context_low: false,
                alerted_since: None,
        });
        if session_id.is_some() {
            agent.session_id = session_id;
        }
        agent.authority = "hook".into();
        agent.reason = "reported by integration".into();
        agent.class = AgentClass::Agent;
        apply_transition(agent, state, focused)
            .map(|(from, to)| Event::AgentStateChanged { pane, name: agent.name.clone(), from, to })
    }

    /// Focusing or typing at a pane clears its `done` badge.
    pub fn mark_seen(&mut self, session: &mut Session, pane: PaneId) {
        if let Some(agent) = session.panes.get_mut(&pane).and_then(|p| p.agent.as_mut()) {
            agent.seen = true;
            if agent.state == AgentState::Done {
                agent.state = AgentState::Idle;
                agent.reason = "seen".into();
            }
        }
    }

    /// Full pass over every pane. Called on a slow cadence because probing the foreground
    /// process shells out.
    pub fn scan(&mut self, session: &mut Session, cfg: &Config) -> Vec<Event> {
        let mut events = Vec::new();
        let pane_ids: Vec<PaneId> = session.panes.keys().copied().collect();
        let focused = session.focused_pane();

        // Refresh the foreground process cache, on its own slower cadence. A pane horde has
        // never probed is always looked at, so a newly spawned agent is identified at once.
        let probe_due = self
            .last_process_probe
            .is_none_or(|t| t.elapsed() >= PROCESS_INTERVAL);
        for &id in &pane_ids {
            if !probe_due && self.processes.contains_key(&id) {
                continue;
            }
            match session.panes.get(&id).and_then(|p| p.foreground_pgid()) {
                Some(pgid) => {
                    if let Some(name) = process_name(pgid) {
                        self.processes.insert(id, name);
                    }
                }
                None => {
                    self.processes.remove(&id);
                }
            }
        }
        if probe_due {
            self.last_process_probe = Some(Instant::now());
        }
        self.processes.retain(|id, _| pane_ids.contains(id));
        self.hook_reports.retain(|id, _| pane_ids.contains(id));
        self.hook_kinds.retain(|id, _| pane_ids.contains(id));

        for &id in &pane_ids {
            let process = self.processes.get(&id).cloned();
            // The title is captured from OSC 0/2 rather than read off the grid, which is why
            // it survives a narrow pane and never lingers from scrollback.
            let (lines, title) = match session.panes.get(&id) {
                Some(p) => (p.detection_snapshot(cfg.detection_lines), p.osc_title.clone()),
                None => continue,
            };
            let screen = lines.join("\n");

            // Which agent, if any, is in this pane? A fresh hook report is itself proof,
            // and outranks guessing from the process name or the screen.
            let hooked = self
                .hook_reports
                .get(&id)
                .is_some_and(|t| t.elapsed() < HOOK_TTL)
                .then(|| self.hook_kinds.get(&id).cloned())
                .flatten();
            let cmd_kind = session
                .panes
                .get(&id)
                .and_then(|p| p.cmd.split_whitespace().next())
                .and_then(|w| w.rsplit('/').next())
                .map(|s| s.to_string());
            let found = hooked.or_else(|| self.identify(process.as_deref(), cmd_kind.as_deref(), &screen));

            // Hooks hold authority while their last report is fresh.
            let hook_fresh = self.hook_reports.get(&id).is_some_and(|t| t.elapsed() < HOOK_TTL);

            let Some(kind) = found else {
                // Nothing recognisable on screen. Drop the runtime so the sidebar stops
                // listing an agent that has exited back to a shell — unless hooks are still
                // reporting, in which case they are the authority and screen detection has
                // no business overruling them. Without this guard a hook-reported agent
                // whose process name horde does not recognise would flicker in and out.
                if !hook_fresh {
                    if let Some(p) = session.panes.get_mut(&id) {
                        if p.agent.is_some() {
                            p.agent = None;
                        }
                    }
                }
                continue;
            };

            // `evaluate` returning None means a rule matched that deliberately leaves the
            // state alone — a transcript viewer or model picker, which reads like a prompt.
            let verdict: Option<Verdict> = if hook_fresh {
                None
            } else {
                self.manifests
                    .get(&kind)
                    .and_then(|m| m.evaluate(&Screen { lines: &lines, osc_title: &title }))
            };

            let names = self.taken_names(session, id);
            let is_focused = focused == Some(id);
            let class = self.class_of(&kind);
            let Some(pane) = session.panes.get_mut(&id) else { continue };

            // An explicit pane name is what the user asked to address this agent by, so it
            // wins over the auto-generated one. `horde spawn --name reviewer` would
            // otherwise still be reachable only as `claude-2`.
            let explicit = pane.name.clone();
            let auto = unique_name(&kind, &names);
            let agent = pane.agent.get_or_insert_with(|| AgentRuntime {
                name: explicit.clone().unwrap_or(auto),
                kind: kind.clone(),
                class,
                state: AgentState::Unknown,
                since: Instant::now(),
                authority: if hook_fresh { "hook".into() } else { "screen".into() },
                reason: "detected".into(),
                seen: is_focused,
                session_id: None,
                queued: Vec::new(),
                question: None,
                endpoint: None,
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                memory_nudged: None,
                context_low: false,
                alerted_since: None,
            });

            // A pane can change hands, e.g. quitting one agent and starting another.
            if agent.kind != kind {
                agent.kind = kind.clone();
                agent.name = explicit.clone().unwrap_or_else(|| unique_name(&kind, &names));
            }
            agent.class = class;
            // A later `horde pane rename` re-addresses the agent too.
            if let Some(name) = explicit {
                if agent.name != name {
                    agent.name = name;
                }
            }

            if let Some(v) = verdict {
                agent.authority = "screen".into();
                agent.reason = v.reason;
                if let Some((from, to)) = apply_transition(agent, v.state, is_focused) {
                    events.push(Event::AgentStateChanged {
                        pane: id,
                        name: agent.name.clone(),
                        from,
                        to,
                    });
                }
            } else {
                agent.authority = "hook".into();
            }

            // What it is waiting on, while it is waiting. After the transition, not before:
            // the tick an agent *becomes* blocked is the tick its prompt appears, and reading
            // the old state here would leave the queue one scan behind the screen.
            //
            // Read here rather than on demand because this is the one place holding both the
            // screen and the state. By the time a snapshot is built the lines are gone, and
            // re-reading them per client would parse the same screen once per attached client
            // per frame.
            //
            // Cleared for every other state, so a question can never outlive the prompt that
            // asked it — answering a question the agent has moved past is the one failure
            // this feature could cause that reading the pane by hand could not.
            agent.question = match agent.state {
                AgentState::Blocked => super::question::extract(&lines),
                _ => None,
            };

            // Where a service is answering, for the same reason and in the same place.
            //
            // Sticky, unlike `question`, and that is the whole difference between them. A
            // question is cleared the moment the prompt goes because answering a stale one
            // is a real harm; an address is printed once at startup and then buried under
            // request logs, so re-reading a screen that has scrolled past it must not blank
            // a port that is still perfectly live. It is replaced when the screen says
            // something newer, and dropped only when the service itself does.
            if agent.class == AgentClass::Service {
                if let Some(found) = super::endpoint::extract(&lines) {
                    agent.endpoint = Some(found);
                }
            } else {
                // A pane that changed hands from a server to an agent must not keep the
                // server's port on its row.
                agent.endpoint = None;
            }

            // Whether it is about to lose this conversation. Read here for the third time for
            // the same reason as the two above: this is the one place holding the screen.
            //
            // The flag is cleared the moment the warning leaves the screen, which is what
            // makes the nudge fire once per fill-up rather than once per session: an agent
            // that compacts, works, and fills up again gets told again, and one staring at the
            // same warning for an hour does not.
            let pressure = super::compaction::pressure(&lines);
            agent.context_low = matches!(pressure, Some(super::compaction::Pressure::Low { .. }));
            // Re-arm only once the warning has been gone for the whole cooldown. Clearing the
            // moment it left the screen re-armed on a redraw, which is how one fill-up earned
            // two nudges.
            if !agent.context_low {
                if let Some(at) = agent.memory_nudged {
                    if at.elapsed() >= MEMORY_NUDGE_COOLDOWN {
                        agent.memory_nudged = None;
                    }
                }
            }
        }

        events
    }

    /// Work out what is in a pane, in order of how much the signal can be trusted.
    ///
    /// 1. an agent's foreground process name — a definite answer from `ps`
    /// 2. the command the pane was started with — what we were asked to run
    /// 3. an agent's screen patterns — a guess, and the only ambiguous one
    /// 4. a service's process name, then its screen patterns
    ///
    /// The order matters and the iteration is sorted, because several agents share phrases
    /// in their UIs. Letting a screen guess outrank `ps`, or letting HashMap order pick
    /// between two matching manifests, is how a Claude pane ends up labelled `codex` on one
    /// scan and `gemini` on the next.
    ///
    /// Services come last, after even an agent's screen guess, because a service manifest
    /// names launchers rather than programs: `npm` and `bun` run whatever you ask them to,
    /// including an agent. "A service is what a pane is when it is not an agent" costs
    /// nothing — a dev server has never matched an agent's `detect` patterns — and it means
    /// a broad process list can never quietly relabel something you were talking to.
    fn identify(&self, process: Option<&str>, cmd: Option<&str>, screen: &str) -> Option<String> {
        let mut names: Vec<&String> = self.manifests.keys().collect();
        names.sort();
        let of_class = |c: AgentClass| -> Vec<&String> {
            names.iter().copied().filter(|n| self.manifests[*n].class == c).collect()
        };
        let agents = of_class(AgentClass::Agent);

        for n in &agents {
            if self.manifests[*n].matches_process(process) {
                return Some((*n).clone());
            }
        }
        // Before the shell guard, because this tier is not a guess: it is the command horde
        // was asked to run in this pane. A dev script that is itself a shell script reports
        // as `sh` in the foreground, and `horde spawn --cmd "npm run dev"` should still be a
        // dev server rather than nothing at all.
        if let Some(cmd) = cmd {
            if let Some(n) = names.iter().find(|n| n.as_str() == cmd) {
                return Some((*n).clone());
            }
            for n in &names {
                if self.manifests[*n].processes.iter().any(|p| p == cmd) {
                    return Some((*n).clone());
                }
            }
        }
        // Everything below this line is inference from what is on screen, and a shell prompt
        // is where that inference goes wrong: the pane is showing you scrollback, not a
        // running program. See `manifest::is_shell`.
        if manifest::is_shell(process) {
            return None;
        }
        for n in &agents {
            if self.manifests[*n].matches_screen(screen) {
                return Some((*n).clone());
            }
        }
        let services = of_class(AgentClass::Service);
        for n in &services {
            if self.manifests[*n].matches_process(process) {
                return Some((*n).clone());
            }
        }
        for n in &services {
            if self.manifests[*n].matches_screen(screen) {
                return Some((*n).clone());
            }
        }
        None
    }

    /// What class of thing a manifest name describes. An unknown name is an agent: that is
    /// what every manifest was before services existed.
    fn class_of(&self, kind: &str) -> AgentClass {
        self.manifests.get(kind).map(|m| m.class).unwrap_or_default()
    }

    /// Names already in use, excluding `except` so a pane does not collide with itself.
    fn taken_names(&self, session: &Session, except: PaneId) -> Vec<String> {
        session
            .panes
            .values()
            .filter(|p| p.id != except)
            .filter_map(|p| p.agent.as_ref().map(|a| a.name.clone()))
            .chain(session.panes.values().filter_map(|p| p.name.clone()))
            .collect()
    }

    /// Explain how a pane's state was decided, for `horde agent explain`.
    pub fn explain(&self, session: &Session, pane: PaneId, cfg: &Config) -> serde_json::Value {
        let process = self.processes.get(&pane).cloned();
        let Some(p) = session.panes.get(&pane) else {
            return serde_json::json!({ "error": "no such pane" });
        };
        let lines = p.detection_snapshot(cfg.detection_lines);
        let screen = lines.join("\n");
        let title = p.osc_title.clone();
        let mut present: Vec<String> = self
            .manifests
            .values()
            .filter(|m| m.present(process.as_deref(), &screen))
            .map(|m| m.name.clone())
            .collect();
        present.sort();
        let cmd_kind = p
            .cmd
            .split_whitespace()
            .next()
            .and_then(|w| w.rsplit('/').next())
            .map(|s| s.to_string());
        let chosen = self.identify(process.as_deref(), cmd_kind.as_deref(), &screen);

        let hook_fresh =
            self.hook_reports.get(&pane).is_some_and(|t| t.elapsed() < HOOK_TTL);

        let evaluated = chosen.as_ref().and_then(|k| self.manifests.get(k)).map(|m| {
            let v = m.evaluate(&Screen { lines: &lines, osc_title: &title });
            serde_json::json!({
                "manifest": m.name,
                "state": v.as_ref().map(|v| v.state.label()),
                "matched_rule": v.as_ref().map(|v| v.reason.clone()),
                "suppressed": v.is_none(),
                "rules": m
                    .rules
                    .iter()
                    .map(|r| serde_json::json!({
                        "id": r.id,
                        "state": r.state.label(),
                        "priority": r.priority,
                        "region": format!("{:?}", r.region),
                        "skip_state_update": r.skip_state_update,
                    }))
                    .collect::<Vec<_>>(),
            })
        });

        serde_json::json!({
            "pane": pane,
            "foreground_process": process,
            "spawn_command": p.cmd,
            "authority": if hook_fresh { "hook" } else { "screen" },
            "hook_authoritative": hook_fresh,
            "manifests_matching": present,
            "chosen": chosen,
            "screen_evaluation": evaluated,
            "current": p.agent.as_ref().map(|a| serde_json::json!({
                "kind": a.kind,
                "name": a.name,
                "class": a.class,
                "state": a.state.label(),
                "reason": a.reason,
                "seen": a.seen,
                "queued_messages": a.queued.len(),
            })),
            "osc_title": title,
            "snapshot_lines": lines,
        })
    }
}

/// Move an agent to a new state, deriving `done` on the way.
///
/// Returns the transition when the state actually changed, so callers only emit events for
/// real changes rather than every scan.
fn apply_transition(
    agent: &mut AgentRuntime,
    next: AgentState,
    focused: bool,
) -> Option<(AgentState, AgentState)> {
    // Finishing while unobserved is what `done` means. If you are already looking at the
    // pane there is nothing to flag, so it goes straight to idle. A service has no finish to
    // flag — it stops when you stop it, and that is a pane exiting, not a result to read.
    let resolved = if next == AgentState::Idle
        && agent.state == AgentState::Working
        && !focused
        && agent.class == AgentClass::Agent
    {
        AgentState::Done
    } else {
        next
    };

    // Do not let a plain `idle` report erase a `done` badge the user has not seen yet;
    // otherwise a finished agent would quietly stop asking for attention.
    if resolved == AgentState::Idle && agent.state == AgentState::Done && !agent.seen {
        return None;
    }

    if resolved == agent.state {
        return None;
    }
    let from = agent.state;
    agent.state = resolved;
    agent.since = Instant::now();
    if resolved != AgentState::Done {
        agent.seen = focused;
    } else {
        agent.seen = false;
    }
    Some((from, resolved))
}

/// `claude`, then `claude-2`, `claude-3`, ...
fn unique_name(kind: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == kind) {
        return kind.to_string();
    }
    for n in 2.. {
        let cand = format!("{kind}-{n}");
        if !taken.iter().any(|t| *t == cand) {
            return cand;
        }
    }
    unreachable!()
}

/// Name of the process leading a process group.
///
/// The routes differ per kernel and the choice between them is a platform question, so it lives
/// in [`crate::platform::process_name`] with the rest of them.
fn process_name(pgid: i32) -> Option<String> {
    crate::platform::process_name(pgid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(state: AgentState, seen: bool) -> AgentRuntime {
        AgentRuntime {
            kind: "claude".into(),
            name: "claude".into(),
            class: AgentClass::Agent,
            state,
            since: Instant::now(),
            authority: "screen".into(),
            reason: "t".into(),
            seen,
            session_id: None,
            queued: Vec::new(),
            question: None,
            endpoint: None,
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                memory_nudged: None,
                context_low: false,
                alerted_since: None,
        }
    }

    #[test]
    fn finishing_unobserved_becomes_done() {
        let mut a = agent(AgentState::Working, false);
        let t = apply_transition(&mut a, AgentState::Idle, false);
        assert_eq!(t, Some((AgentState::Working, AgentState::Done)));
        assert_eq!(a.state, AgentState::Done);
        assert!(!a.seen);
    }

    #[test]
    fn finishing_while_watched_goes_straight_to_idle() {
        let mut a = agent(AgentState::Working, true);
        let t = apply_transition(&mut a, AgentState::Idle, true);
        assert_eq!(t, Some((AgentState::Working, AgentState::Idle)));
        assert_eq!(a.state, AgentState::Idle);
    }

    #[test]
    fn done_survives_repeated_idle_reports_until_seen() {
        let mut a = agent(AgentState::Done, false);
        // The screen still says idle on every scan; the badge must not be erased.
        for _ in 0..5 {
            assert_eq!(apply_transition(&mut a, AgentState::Idle, false), None);
            assert_eq!(a.state, AgentState::Done);
        }
        // Once seen, idle takes effect.
        a.seen = true;
        assert_eq!(
            apply_transition(&mut a, AgentState::Idle, false),
            Some((AgentState::Done, AgentState::Idle))
        );
    }

    #[test]
    fn done_yields_to_a_real_state_change() {
        let mut a = agent(AgentState::Done, false);
        // Starting new work must clear the badge even though it was never seen.
        assert_eq!(
            apply_transition(&mut a, AgentState::Working, false),
            Some((AgentState::Done, AgentState::Working))
        );
    }

    #[test]
    fn unchanged_state_reports_no_transition() {
        let mut a = agent(AgentState::Working, false);
        assert_eq!(apply_transition(&mut a, AgentState::Working, false), None);
    }

    #[test]
    fn blocked_is_reachable_from_any_state() {
        for from in [AgentState::Idle, AgentState::Working, AgentState::Done] {
            let mut a = agent(from, false);
            let t = apply_transition(&mut a, AgentState::Blocked, false);
            assert_eq!(t, Some((from, AgentState::Blocked)), "from {from:?}");
        }
    }

    #[test]
    fn transition_resets_the_elapsed_clock() {
        let mut a = agent(AgentState::Idle, false);
        a.since = Instant::now() - Duration::from_secs(60);
        apply_transition(&mut a, AgentState::Working, false);
        assert!(a.since.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn names_are_uniquified_per_kind() {
        assert_eq!(unique_name("claude", &[]), "claude");
        assert_eq!(unique_name("claude", &["claude".into()]), "claude-2");
        assert_eq!(
            unique_name("claude", &["claude".into(), "claude-2".into()]),
            "claude-3"
        );
        // A gap is reused rather than skipped.
        assert_eq!(unique_name("claude", &["claude".into(), "claude-3".into()]), "claude-2");
    }

    #[test]
    fn detector_loads_bundled_manifests() {
        let cfg = Config::default();
        let d = Detector::new(&cfg);
        assert!(d.manifest_names().contains(&"claude".to_string()));
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    /// Bundled manifests only, so a stray override in the developer's own config directory
    /// cannot change what these tests mean.
    fn bundled() -> Detector {
        let (manifests, warnings) = manifest::load_all(std::path::Path::new("/nonexistent"));
        assert!(warnings.is_empty(), "{warnings:?}");
        Detector {
            manifests,
            warnings,
            processes: HashMap::new(),
            hook_reports: HashMap::new(),
            hook_kinds: HashMap::new(),
            last_process_probe: None,
        }
    }

    /// The bug this guard exists for: a shell that has merely *mentioned* an agent kept
    /// matching its `detect` patterns, so a plain terminal sat in the roster as a live agent
    /// forever — one that could then be handed board work it would never do.
    #[test]
    fn a_shell_prompt_is_not_an_agent_however_much_scrollback_mentions_one() {
        let d = bundled();
        let scrollback = "❯ gh pr view 41\n  🤖 Generated with Claude Code\n\
                          ❯ ";
        assert_eq!(d.identify(Some("/bin/zsh"), Some("zsh"), scrollback), None);
        // The same screen with the agent actually in the foreground is still the agent.
        assert_eq!(d.identify(Some("claude"), Some("zsh"), scrollback).as_deref(), Some("claude"));
    }

    /// The guard is on inference, not on everything: `--cmd "npm run dev"` is a fact about
    /// the pane, and a dev script that happens to be a shell script must not vanish because
    /// `ps` says `sh`.
    #[test]
    fn what_the_pane_was_asked_to_run_survives_the_shell_guard() {
        let d = bundled();
        assert_eq!(d.identify(Some("/bin/sh"), Some("npm"), "").as_deref(), Some("dev"));
    }

    #[test]
    fn a_dev_server_is_recognised_as_a_service() {
        let d = bundled();
        assert_eq!(d.identify(Some("npm run dev"), Some("zsh"), "").as_deref(), Some("dev"));
        assert_eq!(d.class_of("dev"), AgentClass::Service);
        assert_eq!(d.class_of("claude"), AgentClass::Agent);
        // And an unknown kind is an agent, which is what every manifest was before services.
        assert_eq!(d.class_of("something-else"), AgentClass::Agent);
    }

    /// A service manifest names launchers, and a launcher runs anything you ask it to. So a
    /// pane that looks like an agent stays that agent even when `ps` says `npm` — otherwise a
    /// broad process list could quietly relabel the thing you were talking to.
    #[test]
    fn an_agent_on_screen_outranks_a_launcher_in_the_foreground() {
        let d = bundled();
        let screen = "Claude Code\n? for shortcuts";
        assert_eq!(d.identify(Some("npm"), None, screen).as_deref(), Some("claude"));
    }

    /// A dev server has no finish to report: `done` is "you have not read this yet", and
    /// nobody is going to read a page-load log.
    #[test]
    fn a_service_never_derives_done() {
        let mut a = agent(AgentState::Working, false);
        a.class = AgentClass::Service;
        assert_eq!(
            apply_transition(&mut a, AgentState::Idle, false),
            Some((AgentState::Working, AgentState::Idle))
        );
    }
}
