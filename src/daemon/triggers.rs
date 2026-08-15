//! The part that acts when nobody is watching.
//!
//! Everything else in horde waits to be asked. The bus routes a message someone sent, the board
//! holds work someone added, an agent takes a task because it was told to look. That makes horde
//! a workshop: capable, and completely inert until you are in it. A trigger is the thing that
//! pulls when the room is empty.
//!
//! Which is a much larger promise than it sounds, so almost all of this file is about *not*
//! firing. The mechanism is a timestamp comparison; the engineering is the guards:
//!
//! - **A master switch, off by default.** A fresh install never acts on its own.
//! - **One piece of work in flight per trigger.** A daily task still sitting on the board is
//!   the reason not to add a second one.
//! - **A floor on the interval**, so `every 1s` cannot be asked for.
//! - **A ceiling on firings per hour**, across all triggers — because agents can create these,
//!   so the failure mode is not one bad rule but fifty.
//! - **A cap on agents horde started**, counted live so a finished one frees its slot. This is
//!   the number of full-permission agents that can be working with nobody present.
//! - **No rule-making by machine-started agents** (enforced in [`super::rpc`], where the calling
//!   pane is known). Agents creating rules is useful; rules creating agents that create rules has
//!   no human anywhere in it.
//! - **An unmet `--when` condition spends the interval**, or the probe re-runs every tick.
//! - **A failed action still counts as a firing**, or a broken trigger retries every tick
//!   forever.
//! - **Everything is journaled**, because a machine that acts while you are away is only
//!   trustworthy if you can read back what it decided to do.
//!
//! The action to reach for is [`What::Task`]. It puts work on the board and lets the nudge that
//! already exists find a free agent, which means a trigger never has to know who is idle and the
//! exclusivity guarantee stays where it already is — in `Board::claim`'s compare-and-set.
//!
//! A `--task` rule is scoped to the space it was created in, so scheduled work lands on the
//! right project's board rather than being offered to whichever agent happens to be idle.

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::journal::Kind;
use super::Engine;
use crate::proto::{Event, NoticeLevel};

/// Triggers kept in memory. Far above any plausible number of rules; a backstop, not a budget.
const CAP: usize = 500;

/// Most firings allowed in any rolling hour, counted across every trigger together.
///
/// A global ceiling rather than a per-trigger one on purpose: a per-trigger limit does nothing
/// about fifty triggers each behaving impeccably. Twelve an hour is more than a person would set
/// up deliberately and far less than a loop would produce.
const MAX_PER_HOUR: usize = 12;

/// The shortest interval a trigger may be given. Anything faster is a busy-wait with extra steps.
const MIN_INTERVAL_SECS: u64 = 60;

/// A rolling hour, in millis.
const HOUR: u64 = 3_600_000;

/// When a trigger fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum When {
    /// Every `secs` seconds, counted from the last firing rather than from a fixed grid, so a
    /// slow action cannot cause the next one to be due the moment it finishes.
    Every { secs: u64 },
    /// At a local wall-clock time, on the days named by `days`.
    ///
    /// `days` is a bitmask, bit 0 Sunday through bit 6 Saturday, matching `tm_wday` so the
    /// check is a shift rather than a table. Defaulted to every day, so rules written before
    /// this field existed replay unchanged.
    At {
        hour: u32,
        min: u32,
        #[serde(default = "every_day")]
        days: u8,
    },
}

/// All seven bits set — what `--at` means without `--days`.
pub const EVERY_DAY: u8 = 0b0111_1111;

fn every_day() -> u8 {
    EVERY_DAY
}

/// Sunday first, to match `tm_wday`.
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// What a trigger does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum What {
    /// Put work on the board. The one to reach for: it composes with the nudge and the claim.
    Task { text: String },
    /// Push a line at one named agent. Bypasses the board, so it also bypasses everything the
    /// board guarantees — worth it only when the work belongs to a specific agent.
    Send { to: String, body: String },
    /// Start an agent.
    ///
    /// The one action that changes what horde is rather than what it does: an agent started
    /// with nobody present runs its tool calls with nobody to approve them. horde does not
    /// choose the posture — `cmd` is yours, flags and all — it only bounds how many of these
    /// can exist at once and records which ones it started.
    ///
    /// Usually paired with a board task rather than used alone: a spawned agent sitting at a
    /// prompt does nothing until the nudge tells it there is work waiting.
    Spawn { cmd: String, name: Option<String> },
}

impl When {
    pub fn describe(&self) -> String {
        match self {
            When::Every { secs } => format!("every {}", secs_words(*secs)),
            When::At { hour, min, days } => match describe_days(*days) {
                Some(d) => format!("at {hour:02}:{min:02} {d}"),
                None => format!("at {hour:02}:{min:02}"),
            },
        }
    }
}

/// `mon–fri`, `sat,sun`, or `None` for every day — which needs no saying.
fn describe_days(days: u8) -> Option<String> {
    if days & EVERY_DAY == EVERY_DAY {
        return None;
    }
    let set: Vec<usize> = (0..7).filter(|i| days & (1 << i) != 0).collect();
    if set.is_empty() {
        return Some("never".to_string());
    }
    // Contiguous runs read as a range, which is how they were almost certainly written.
    let contiguous = set.windows(2).all(|w| w[1] == w[0] + 1);
    if contiguous && set.len() > 2 {
        return Some(format!("{}–{}", DAY_NAMES[set[0]], DAY_NAMES[set[set.len() - 1]]));
    }
    Some(set.iter().map(|i| DAY_NAMES[*i]).collect::<Vec<_>>().join(","))
}

impl What {
    pub fn describe(&self) -> String {
        match self {
            What::Task { text } => format!("board: {text}"),
            What::Send { to, body } => format!("send {to}: {body}"),
            What::Spawn { cmd, name } => match name {
                Some(n) => format!("spawn {cmd} as {n}"),
                None => format!("spawn {cmd}"),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    pub id: u64,
    pub when: When,
    pub what: What,
    pub enabled: bool,
    /// Unix millis.
    pub created: u64,
    /// Who added it — `user`, or the agent that did.
    pub by: String,
    /// Which project the rule belongs to, by space name.
    ///
    /// Carried so a `--task` rule puts its work on the right project's board. A rule with no
    /// space predates this and scopes nothing, which is how it already behaved.
    #[serde(default)]
    pub space: Option<String>,
    pub last_fired: Option<u64>,
    pub fire_count: u64,
    /// Shell condition. The rule acts only when this exits 0.
    ///
    /// A gate on the schedule rather than a source of its own, which is why there is no file
    /// watcher here: "has anything changed" is a command (`git diff --quiet`), "are the tests
    /// broken" is a command, and one gate composes with both `every` and `at` instead of adding
    /// a variant per question.
    #[serde(default)]
    pub only_if: Option<String>,
    /// When the schedule last came round, whether or not it fired.
    ///
    /// Distinct from `last_fired`, which stays the record of what actually happened. A condition
    /// that comes back false has to spend the interval — otherwise a rule with a `--when` re-runs
    /// its probe on every tick, which for `cargo test` is a fork bomb with good intentions.
    /// Falls back to `last_fired` when absent, so rules written before this field replay
    /// unchanged rather than all coming due at once.
    #[serde(default)]
    pub last_eval: Option<u64>,
    /// Removed. Kept in the log so the record of what once fired here survives, and hidden
    /// from every listing.
    #[serde(default)]
    pub deleted: bool,
}

impl Trigger {
    /// The whole rule on one line: schedule, condition, action.
    pub fn describe(&self) -> String {
        match &self.only_if {
            Some(c) => format!("{} if `{c}` · {}", self.when.describe(), self.what.describe()),
            None => format!("{} · {}", self.when.describe(), self.what.describe()),
        }
    }

    /// What the next firing is measured from: the last one, or creation for a trigger that has
    /// never fired.
    ///
    /// Creation rather than zero, so adding `every 30m` waits its thirty minutes instead of
    /// firing the instant you add it — and adding `at 09:00` in the afternoon waits for
    /// tomorrow rather than deciding it is nine hours late.
    fn baseline(&self) -> u64 {
        self.last_eval.or(self.last_fired).unwrap_or(self.created)
    }

    fn is_due(&self, now: u64) -> bool {
        match &self.when {
            When::Every { secs } => now.saturating_sub(self.baseline()) >= secs * 1000,
            // Late rather than skipped: if the daemon was down at nine, a trigger that has not
            // run since before nine still runs when it comes back. Being told at eleven that
            // yesterday's diff wants reviewing beats never being told.
            //
            // With `days` set, the day filter wins over lateness. A weekday rule that was
            // missed on Friday does not fire on Saturday — running late is a courtesy, running
            // on a day you excluded is disobeying the rule.
            When::At { hour, min, days } => {
                let occurrence = last_occurrence(now, *hour, *min);
                day_allowed(occurrence, *days) && self.baseline() < occurrence
            }
        }
    }
}

pub struct Store {
    log: super::logfile::AppendLog,
    triggers: Vec<Trigger>,
    next_id: u64,
    /// When the hourly ceiling was last complained about, so a capped hour produces one
    /// warning rather than one per tick.
    capped_notice_at: u64,
    /// Conditions currently being evaluated, by trigger id. Runtime only — an in-flight probe
    /// is not worth persisting, and a daemon restart simply re-asks.
    probes: HashMap<u64, Probe>,
}

/// A `--when` condition running on its own thread.
///
/// On a thread because the engine is single-threaded and drives every pane: waiting here for
/// `cargo test` would freeze every agent's output for the duration. The result arrives on a later
/// tick, which is why a condition costs a tick or two rather than being decided inline.
struct Probe {
    started: u64,
    rx: std::sync::mpsc::Receiver<bool>,
}

/// How long a condition may take before it is abandoned.
///
/// A hung probe holds its trigger's slot forever otherwise. The thread is left to finish on its
/// own — one stuck child per rule is a bounded leak, and killing a shell pipeline from another
/// thread costs more machinery than the problem is worth.
const PROBE_TIMEOUT: u64 = 60_000;

impl Store {
    pub fn new(path: PathBuf) -> Store {
        let triggers = read_log(&path);
        let next_id = triggers.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        Store {
            log: super::logfile::AppendLog::new(path),
            triggers,
            next_id,
            capped_notice_at: 0,
            probes: HashMap::new(),
        }
    }

    /// Every trigger that still exists, deleted ones excluded.
    pub fn all(&self) -> impl Iterator<Item = &Trigger> {
        self.triggers.iter().filter(|t| !t.deleted)
    }

    pub fn get(&self, id: u64) -> Option<&Trigger> {
        self.triggers.iter().find(|t| t.id == id && !t.deleted)
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.all().count()
    }

    /// Rules that could fire, which is the number the sidebar shows.
    pub fn armed_count(&self) -> usize {
        self.all().filter(|t| t.enabled).count()
    }

    pub fn add(
        &mut self,
        when: When,
        what: What,
        by: &str,
        only_if: Option<String>,
        space: Option<String>,
    ) -> Result<Trigger> {
        if let When::Every { secs } = when {
            if secs < MIN_INTERVAL_SECS {
                return Err(anyhow!(
                    "the shortest interval is {MIN_INTERVAL_SECS}s — a faster trigger is a \
                     busy-wait, not a schedule"
                ));
            }
        }
        if let What::Task { text } = &what {
            if text.trim().is_empty() {
                return Err(anyhow!("a task needs a description"));
            }
        }
        let t = Trigger {
            id: self.next_id,
            when,
            what,
            enabled: true,
            created: super::now_millis(),
            by: by.to_string(),
            space,
            last_fired: None,
            fire_count: 0,
            only_if: only_if.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()),
            last_eval: None,
            deleted: false,
        };
        self.next_id += 1;
        self.append(&t);
        self.triggers.push(t.clone());
        while self.triggers.len() > CAP {
            match self.triggers.iter().position(|t| t.deleted) {
                Some(i) => {
                    self.triggers.remove(i);
                }
                None => break,
            }
        }
        Ok(t)
    }

    pub fn remove(&mut self, id: u64) -> Result<Trigger> {
        let t = self.mutate(id, |t| t.deleted = true)?;
        Ok(t)
    }

    pub fn set_enabled(&mut self, id: u64, on: bool) -> Result<Trigger> {
        self.mutate(id, |t| t.enabled = on)
    }

    /// Turn every trigger off at once — the one keystroke that has to exist.
    pub fn disable_all(&mut self) -> Vec<Trigger> {
        let ids: Vec<u64> = self.all().filter(|t| t.enabled).map(|t| t.id).collect();
        ids.into_iter().filter_map(|id| self.set_enabled(id, false).ok()).collect()
    }

    fn mark_fired(&mut self, id: u64, now: u64) {
        let _ = self.mutate(id, |t| {
            t.last_fired = Some(now);
            t.last_eval = Some(now);
            t.fire_count += 1;
        });
    }

    /// The schedule came round and the answer was no. Spends the interval without claiming a
    /// firing, so the condition is asked again next interval rather than next tick.
    fn mark_checked(&mut self, id: u64, now: u64) {
        let _ = self.mutate(id, |t| t.last_eval = Some(now));
    }

    fn mutate(&mut self, id: u64, f: impl FnOnce(&mut Trigger)) -> Result<Trigger> {
        let i = self
            .triggers
            .iter()
            .position(|t| t.id == id && !t.deleted)
            .ok_or_else(|| anyhow!("no trigger #{id}"))?;
        f(&mut self.triggers[i]);
        let out = self.triggers[i].clone();
        self.append(&out);
        Ok(out)
    }

    fn append(&mut self, t: &Trigger) {
        if let Ok(line) = serde_json::to_string(t) {
            self.log.append_line(&line);
        }
        // Carry the live set into the new file, exactly as the board does: the ordinary
        // "rename and start empty" would silently drop every rule you rely on.
        if self.log.rotation_due() {
            let carry: Vec<String> =
                self.triggers.iter().filter_map(|t| serde_json::to_string(t).ok()).collect();
            self.log.rotate(&carry);
        }
    }
}

/// Replay the log, later entries for an id superseding earlier ones.
fn read_log(path: &PathBuf) -> Vec<Trigger> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<Trigger> = Vec::new();
    for line in text.lines() {
        let Ok(t) = serde_json::from_str::<Trigger>(line) else { continue };
        match out.iter_mut().find(|x| x.id == t.id) {
            Some(slot) => *slot = t,
            None => out.push(t),
        }
    }
    out.sort_by_key(|t| t.id);
    out
}

// ---------------------------------------------------------------------------
// Firing
// ---------------------------------------------------------------------------

/// Fire everything due. Called every tick; returns events for attached clients.
pub fn fire_due(eng: &mut Engine) -> Vec<Event> {
    // The master switch, checked first and cheapest. Nothing below this line can happen to
    // someone who has not asked for it.
    if !eng.cfg.unattended {
        return Vec::new();
    }

    let now = super::now_millis();
    let due: Vec<u64> = eng
        .triggers
        .all()
        .filter(|t| t.enabled)
        .filter(|t| t.is_due(now))
        // A trigger whose last piece of work is still outstanding has already done its job.
        // Not marked as fired when skipped, so it goes as soon as the board clears rather than
        // waiting out another whole interval.
        .filter(|t| !outstanding(eng, t))
        .map(|t| t.id)
        .collect();
    if due.is_empty() {
        return Vec::new();
    }

    let mut events = Vec::new();
    let recent = eng.journal.since(now.saturating_sub(HOUR)).filter(|e| e.kind == Kind::Fired);
    let budget = MAX_PER_HOUR.saturating_sub(recent.count());
    if budget == 0 {
        // Refused, and visibly so: a trigger that quietly stopped working is worse than one
        // that failed loudly. Once an hour, because the condition persists.
        if now.saturating_sub(eng.triggers.capped_notice_at) >= HOUR {
            eng.triggers.capped_notice_at = now;
            events.push(Event::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "{MAX_PER_HOUR} trigger firings this hour is the ceiling — holding \
                     {} more. `horde trigger list` shows what is waiting.",
                    due.len()
                ),
            });
        }
        return events;
    }
    if due.len() > budget {
        events.push(Event::Notice {
            level: NoticeLevel::Warn,
            text: format!("{} triggers due, firing {budget} — hourly ceiling", due.len()),
        });
    }

    for id in due.into_iter().take(budget) {
        let Some(t) = eng.triggers.get(id).cloned() else { continue };

        // A condition is answered on a thread, so a due rule with a `--when` takes a tick or two
        // to decide. `Waiting` means come back next tick; `No` spends the interval so the probe
        // runs once per interval rather than once per tick.
        if t.only_if.is_some() {
            match check_condition(eng, &t, now) {
                Condition::Waiting => continue,
                Condition::No => {
                    eng.triggers.mark_checked(id, now);
                    continue;
                }
                Condition::Failed(why) => {
                    eng.triggers.mark_checked(id, now);
                    events.push(Event::Notice {
                        level: NoticeLevel::Warn,
                        text: format!("trigger #{id} condition {why}"),
                    });
                    continue;
                }
                Condition::Yes => {}
            }
        }

        match perform(eng, &t) {
            Ok((what, ev)) => {
                let line = format!("#{id} {what}");
                eng.journal.note(Kind::Fired, line.as_str());
                super::log_line(&format!("trigger {line}"));
                events.extend(ev);
            }
            Err(e) => {
                // Counted as a firing anyway. A trigger whose target agent no longer exists
                // would otherwise retry every 150ms for as long as you are away.
                let text = format!("trigger #{id} failed: {e}");
                super::log_line(&text);
                events.push(Event::Notice { level: NoticeLevel::Warn, text });
            }
        }
        eng.triggers.mark_fired(id, now);
    }
    events
}

/// Fire one trigger now, whatever its schedule says.
///
/// Every guard is deliberately skipped — the master switch, the schedule, the one-in-flight
/// check, the hourly ceiling. All of them exist to bound what horde does *unasked*, and this is
/// asked. Without it a rule set for nine in the morning can only be tested at nine in the
/// morning, which costs a day per iteration.
///
/// The firing is journaled exactly as an automatic one is, so it does count against the hour's
/// ceiling. That way the record cannot disagree with the guard that reads it.
pub fn fire_now(eng: &mut Engine, id: u64) -> Result<(String, Vec<Event>)> {
    let t = eng.triggers.get(id).cloned().ok_or_else(|| anyhow!("no trigger #{id}"))?;
    let (what, events) = perform(eng, &t)?;
    let line = format!("#{id} {what}");
    eng.journal.note(Kind::Fired, line.as_str());
    super::log_line(&format!("trigger {line} (by hand)"));
    eng.triggers.mark_fired(id, super::now_millis());
    Ok((what, events))
}

/// Where a trigger's `--when` condition stands this tick.
enum Condition {
    /// Exited 0: go.
    Yes,
    /// Exited non-zero: the condition is simply not met, which is ordinary operation.
    No,
    /// Still running. Nothing is spent — ask again next tick.
    Waiting,
    /// Could not be answered: it never started, or it ran too long. Carries the phrase for the
    /// warning.
    Failed(String),
}

/// Start, poll, or time out a trigger's condition.
///
/// One probe in flight per rule, which is what stops a slow condition from being launched again
/// every tick while the first is still thinking.
fn check_condition(eng: &mut Engine, t: &Trigger, now: u64) -> Condition {
    let Some(cmd) = t.only_if.clone() else { return Condition::Yes };

    if let Some(probe) = eng.triggers.probes.get(&t.id) {
        match probe.rx.try_recv() {
            Ok(ok) => {
                eng.triggers.probes.remove(&t.id);
                super::log_line(&format!(
                    "trigger #{} condition {}: {cmd}",
                    t.id,
                    if ok { "met" } else { "not met" }
                ));
                return if ok { Condition::Yes } else { Condition::No };
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if now.saturating_sub(probe.started) < PROBE_TIMEOUT {
                    return Condition::Waiting;
                }
                eng.triggers.probes.remove(&t.id);
                return Condition::Failed(format!(
                    "took longer than {}s and was abandoned: {cmd}",
                    PROBE_TIMEOUT / 1000
                ));
            }
            // The thread died without answering, which a panic in `Command` could do.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                eng.triggers.probes.remove(&t.id);
                return Condition::Failed(format!("could not be run: {cmd}"));
            }
        }
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let spec = cmd.clone();
    // Through `sh -c` for the same reason the notify command is: a condition is usually a
    // pipeline or a negation (`! cargo test -q`), not a bare path.
    let started = std::thread::Builder::new().name(format!("horde-probe-{}", t.id)).spawn(
        move || {
            let ok = std::process::Command::new("sh")
                .arg("-c")
                .arg(&spec)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let _ = tx.send(ok);
        },
    );
    match started {
        Ok(_) => {
            eng.triggers.probes.insert(t.id, Probe { started: now, rx });
            Condition::Waiting
        }
        Err(e) => Condition::Failed(format!("could not start: {e}")),
    }
}

/// Whether this trigger's own last piece of work is still open or claimed.
///
/// Read off the board's existing `by` field rather than a new link table: a triggered task is
/// added as `trigger:<id>`, which the board already records, already persists, and already shows
/// in `horde task list`.
fn outstanding(eng: &Engine, t: &Trigger) -> bool {
    if !matches!(t.what, What::Task { .. }) {
        return false;
    }
    let by = owner_tag(t.id);
    eng.board.all().iter().any(|x| x.by == by && (x.is_open() || x.is_claimed()))
}

/// How a triggered task records who added it, and therefore how [`outstanding`] finds it again.
pub fn owner_tag(id: u64) -> String {
    format!("trigger:{id}")
}

/// Do the thing. Returns a phrase for the record, plus any event clients should see.
fn perform(eng: &mut Engine, t: &Trigger) -> Result<(String, Vec<Event>)> {
    match &t.what {
        What::Task { text } => {
            // A trigger reaches the board directly rather than through the socket, so the RPC
            // gate does not cover it. Without this check a closed board would still fill up on
            // a schedule — which is the exact combination someone turning it off is avoiding.
            if !eng.cfg.board {
                return Err(anyhow!(
                    "the task board is off (agents.board), so this rule cannot place work"
                ));
            }
            // Scoped to the space the rule was created in, so a scheduled task lands in the
            // project it was written for rather than being offered to whoever is idle.
            let space = t.space.clone();
            let task = eng.board.add(text, &owner_tag(t.id), space.as_deref())?;
            // The sidebar carries the open count, so it has to be told.
            eng.touch();
            Ok((format!("put task #{} on the board: {}", task.id, task.text), Vec::new()))
        }
        What::Send { to, body } => {
            let cfg = eng.cfg.clone();
            let Engine { bus, session, .. } = eng;
            let m = bus.send(session, &cfg, None, to, body, false, false, None)?;
            Ok((format!("sent to {}", m.to), vec![Event::BusMessage(m)]))
        }
        What::Spawn { cmd, name } => {
            // The cap counts what horde is *currently* running, not what it has ever run, so a
            // spawned agent that finishes and exits gives its slot back.
            let live = live_spawned(eng);
            let cap = eng.cfg.max_spawned;
            if live >= cap {
                return Err(anyhow!(
                    "already running {live} agent{} horde started, and the cap is {cap} \
                     (triggers.max_spawned)",
                    if live == 1 { "" } else { "s" }
                ));
            }
            let cfg = eng.cfg.clone();
            let pane = eng.session.split(&cfg, None, crate::proto::Dir::Right, Some(cmd))?;
            if let Some(p) = eng.session.panes.get_mut(&pane) {
                p.name = name.clone();
                // Stamped before anything else can look: the cap and the depth guard both read
                // this, and a pane that exists without it is a pane horde thinks you started.
                p.spawned_by = Some(t.id);
            }
            eng.touch();
            eng.detect_now();
            let who = name.clone().unwrap_or_else(|| format!("pane{pane}"));
            Ok((format!("spawned {who} running {cmd}"), Vec::new()))
        }
    }
}

/// Agents horde started that are still running.
pub fn live_spawned(eng: &Engine) -> usize {
    eng.session.panes.values().filter(|p| p.spawned_by.is_some() && p.exited.is_none()).count()
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Whether `when` lands on a day the rule allows.
fn day_allowed(when: u64, days: u8) -> bool {
    let (_, _, _, wday) = local_parts(when);
    days & (1 << wday) != 0
}

/// The most recent time today's — or yesterday's — `hour:min` came round, in unix millis.
fn last_occurrence(now: u64, hour: u32, min: u32) -> u64 {
    let (h, m, s, _) = local_parts(now);
    let since_midnight = (h as u64 * 3600 + m as u64 * 60 + s as u64) * 1000;
    let midnight = now.saturating_sub(since_midnight);
    let target = midnight + (hour as u64 * 3600 + min as u64 * 60) * 1000;
    if target <= now {
        target
    } else {
        // Yesterday's. A day is not always 86_400s — twice a year a DST shift makes this an
        // hour out, which for "review the diff each morning" is not worth a date library.
        target.saturating_sub(86_400_000)
    }
}

/// The local clock as a person reads it, with the offset that produced it.
///
/// Exists so `horde status` can show the time triggers will actually fire on. The offset is the
/// half that matters: `09:00` alone looks right whatever the timezone, and `09:00 UTC+00` on a
/// machine whose owner is in London in June is the tell that the distro's timezone was never
/// set — the failure this is here to make visible.
pub fn local_clock(ms: u64) -> String {
    let (h, m, _, _) = local_parts(ms);
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: as `local_parts` — `localtime_r` writes only the `tm` passed to it.
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format_clock(h, m, tm.tm_gmtoff as i64)
}

/// The local date, as `2026-08-15`.
///
/// Local rather than UTC, because a note called "today" has to mean the day the person
/// writing it is having. A digest written at eleven at night belongs to that evening, not to
/// tomorrow morning in London.
pub fn local_date(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: as `local_parts` — `localtime_r` writes only the `tm` passed to it.
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// The formatting half of [`local_clock`], split out because the other half is the machine's
/// timezone and cannot be varied from a test without racing every other test that reads it.
///
/// `gmtoff` is seconds east of UTC, as `tm_gmtoff` gives it.
fn format_clock(h: u32, m: u32, gmtoff: i64) -> String {
    let off_min = gmtoff / 60;
    // Sign taken before the magnitude, so `-03:30` does not come out as `-03:-30`.
    let (sign, off_min) = if off_min < 0 { ('-', -off_min) } else { ('+', off_min) };
    match off_min % 60 {
        0 => format!("{h:02}:{m:02} UTC{sign}{:02}", off_min / 60),
        r => format!("{h:02}:{m:02} UTC{sign}{:02}:{r:02}", off_min / 60),
    }
}

/// Local `(hour, minute, second, weekday)`, weekday Sunday-zero.
///
/// `at 09:00 mon–fri` has to mean nine where you are, on the days you mean; the log's UTC clock
/// is not good enough for either half.
fn local_parts(ms: u64) -> (u32, u32, u32, u32) {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes only the `tm` we hand it, which is the reentrant form's
    // whole point. On failure it returns null and leaves the zeroed struct, which reads as
    // midnight on a Sunday — wrong, but bounded, and it cannot fail for a value that came from
    // the clock.
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32, tm.tm_wday as u32)
}

/// `30m`, `2h`, `1d` — how a schedule reads back to the person who set it.
fn secs_words(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// `30m`, `2h`, `1d`, or bare seconds.
pub fn parse_every(spec: &str) -> Result<When> {
    let secs = crate::cli::parse_duration(spec)?;
    if secs < MIN_INTERVAL_SECS {
        return Err(anyhow!(
            "the shortest interval is {MIN_INTERVAL_SECS}s — a faster trigger is a busy-wait, \
             not a schedule"
        ));
    }
    Ok(When::Every { secs })
}

/// `mon-fri`, `mon,wed,fri`, `sat`, `daily`. Returns the day bitmask.
pub fn parse_days(spec: &str) -> Result<u8> {
    let spec = spec.trim().to_lowercase();
    if spec.is_empty() || spec == "daily" || spec == "all" {
        return Ok(EVERY_DAY);
    }
    let day = |name: &str| -> Result<usize> {
        // Three letters is the whole vocabulary, but accept longer forms by their prefix so
        // `monday` and `mon` mean the same thing.
        let n = name.trim();
        DAY_NAMES
            .iter()
            .position(|d| n == *d || (n.len() > 3 && n.starts_with(*d)))
            .ok_or_else(|| anyhow!("cannot read {n:?} as a day — try mon-fri, or sat,sun"))
    };

    let mut mask = 0u8;
    for part in spec.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (from, to) = (day(a)?, day(b)?);
                // Wrapping ranges are what `fri-mon` has to mean; walking forward modulo seven
                // handles both directions without a special case.
                let span = (to + 7 - from) % 7;
                for i in 0..=span {
                    mask |= 1 << ((from + i) % 7);
                }
            }
            None => mask |= 1 << day(part)?,
        }
    }
    if mask == 0 {
        return Err(anyhow!("{spec:?} names no days"));
    }
    Ok(mask)
}

/// `9:00`, `09:00`, `21:30`. Local time.
pub fn parse_at(spec: &str) -> Result<When> {
    let spec = spec.trim();
    let (h, m) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("cannot read {spec:?} as a time of day — try 09:00 or 21:30"))?;
    let hour: u32 = h
        .trim()
        .parse()
        .map_err(|_| anyhow!("cannot read {h:?} as an hour — try 09:00 or 21:30"))?;
    let min: u32 = m
        .trim()
        .parse()
        .map_err(|_| anyhow!("cannot read {m:?} as a minute — try 09:00 or 21:30"))?;
    if hour > 23 || min > 59 {
        return Err(anyhow!("{spec:?} is not a time of day — hours are 0–23, minutes 0–59"));
    }
    Ok(When::At { hour, min, days: EVERY_DAY })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::journal::Journal;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("horde-trig-{tag}.jsonl"));
        let _ = std::fs::remove_file(&p);
        Store::new(p)
    }

    fn task_what() -> What {
        What::Task { text: "review yesterday's diff".into() }
    }

    /// An engine with the master switch on, a trigger store, and its own logs.
    ///
    /// The journal is emptied *before* it is opened, not after: `Journal::new` reads its file at
    /// construction, so deleting afterwards leaves the previous run's entries loaded in memory.
    /// That is worth stating because the hourly ceiling is counted from journal entries, and a
    /// leaked `Fired` from an earlier run makes this suite pass or fail depending on what ran
    /// before it.
    fn eng(tag: &str) -> Engine {
        let jp = std::env::temp_dir().join(format!("horde-trig-{tag}-journal.jsonl"));
        let _ = std::fs::remove_file(&jp);
        let mut e = super::super::tests::engine_with_idle_agents(&format!("trig-{tag}"), 1);
        e.triggers = store(&format!("eng-{tag}"));
        e.journal = Journal::new(jp);
        e.cfg.unattended = true;
        e.clients.clear();
        e
    }

    /// Pretend a trigger's last activity was `ms_ago` milliseconds ago.
    ///
    /// Has to move every marker `baseline()` consults, not just the obvious one — `last_eval`
    /// was added later and winding only `last_fired` leaves the rule permanently not-due, which
    /// looks exactly like a broken guard.
    fn wind_back(e: &mut Engine, id: u64, ms_ago: u64) {
        let then = super::super::now_millis().saturating_sub(ms_ago);
        let _ = e.triggers.mutate(id, |t| {
            t.created = then;
            t.last_fired = t.last_fired.map(|_| then);
            t.last_eval = t.last_eval.map(|_| then);
        });
    }

    /// Stop every pane, so a test that spawned one leaves no process behind.
    fn kill_panes(e: &mut Engine) {
        for p in e.session.panes.values_mut() {
            p.kill();
        }
    }

    /// Whether any event is a warning mentioning `word`.
    fn warned_about(events: &[Event], word: &str) -> bool {
        events
            .iter()
            .any(|ev| matches!(ev, Event::Notice { text, .. } if text.contains(word)))
    }

    fn fired(e: &Engine) -> Vec<String> {
        e.journal
            .since(0)
            .filter(|x| x.kind == Kind::Fired)
            .map(|x| x.subject.clone())
            .collect()
    }

    // -- the store ------------------------------------------------------

    #[test]
    fn triggers_are_numbered_and_enabled_when_added() {
        let mut s = store("add");
        let a = s.add(When::Every { secs: 1800 }, task_what(), "user", None, None).unwrap();
        let at9 = When::At { hour: 9, min: 0, days: EVERY_DAY };
        let b = s.add(at9, task_what(), "builder", None, None).unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert!(a.enabled);
        assert_eq!(b.by, "builder");
        assert_eq!(s.count(), 2);
    }

    #[test]
    fn an_interval_below_the_floor_is_refused() {
        let mut s = store("floor");
        let err =
            s.add(When::Every { secs: 1 }, task_what(), "user", None, None).unwrap_err().to_string();
        assert!(err.contains("shortest interval"), "{err}");
        assert!(parse_every("5s").is_err(), "and at the parsing boundary too");
        assert!(parse_every("60s").is_ok());
    }

    #[test]
    fn removing_a_trigger_hides_it_without_losing_the_record() {
        let mut s = store("rm");
        s.add(When::Every { secs: 3600 }, task_what(), "user", None, None).unwrap();
        s.remove(1).unwrap();
        assert_eq!(s.count(), 0);
        assert!(s.get(1).is_none());
        assert!(s.remove(1).is_err(), "and it cannot be removed twice");
    }

    #[test]
    fn everything_can_be_turned_off_at_once() {
        let mut s = store("offall");
        s.add(When::Every { secs: 3600 }, task_what(), "user", None, None).unwrap();
        s.add(When::Every { secs: 7200 }, task_what(), "user", None, None).unwrap();
        assert_eq!(s.armed_count(), 2);
        assert_eq!(s.disable_all().len(), 2);
        assert_eq!(s.armed_count(), 0);
        assert_eq!(s.count(), 2, "off is not gone");
    }

    #[test]
    fn the_set_survives_a_restart() {
        let p = std::env::temp_dir().join("horde-trig-persist.jsonl");
        let _ = std::fs::remove_file(&p);
        {
            let mut s = Store::new(p.clone());
            s.add(When::Every { secs: 1800 }, task_what(), "user", None, None).unwrap();
            s.add(When::At { hour: 9, min: 30, days: EVERY_DAY }, task_what(), "user", None, None)
                .unwrap();
            s.set_enabled(1, false).unwrap();
        }
        let mut s = Store::new(p.clone());
        assert_eq!(s.count(), 2);
        assert!(!s.get(1).unwrap().enabled, "off has to survive too, or a restart re-arms it");
        assert_eq!(s.get(2).unwrap().when, When::At { hour: 9, min: 30, days: EVERY_DAY });
        // Ids do not restart, so a replayed log cannot collide with a new rule.
        assert_eq!(s.add(When::Every { secs: 60 }, task_what(), "user", None, None).unwrap().id, 3);
        let _ = std::fs::remove_file(&p);
    }

    // -- when it is due -------------------------------------------------

    /// Measured from creation, so adding a rule does not immediately run it. Getting this
    /// backwards means `horde trigger add` doubles as `horde trigger fire`.
    #[test]
    fn a_new_interval_trigger_waits_its_interval() {
        let t = Trigger {
            id: 1,
            when: When::Every { secs: 1800 },
            what: task_what(),
            enabled: true,
            created: super::super::now_millis(),
            by: "user".into(),
            space: None,
            last_fired: None,
            fire_count: 0,
            only_if: None,
            last_eval: None,
            deleted: false,
        };
        let now = super::super::now_millis();
        assert!(!t.is_due(now));
        assert!(t.is_due(now + 1800 * 1000));
    }

    /// Same for a time of day: adding `at 09:00` in the afternoon must wait for tomorrow, not
    /// decide it is nine hours late.
    #[test]
    fn a_new_daily_trigger_does_not_fire_for_a_time_already_past() {
        let now = super::super::now_millis();
        let (h, _, _, _) = local_parts(now);
        // An hour that has already come round today, whatever time the test runs.
        let past = if h == 0 { 0 } else { h - 1 };
        let t = Trigger {
            id: 1,
            when: When::At { hour: past, min: 0, days: EVERY_DAY },
            what: task_what(),
            enabled: true,
            created: now,
            by: "user".into(),
            space: None,
            last_fired: None,
            fire_count: 0,
            only_if: None,
            last_eval: None,
            deleted: false,
        };
        assert!(!t.is_due(now), "created after today's occurrence, so it waits for tomorrow");

        // But one created yesterday is owed today's run.
        let t = Trigger { created: now - 86_400_000, ..t };
        assert!(t.is_due(now));
    }

    /// A daily trigger fires once for one occurrence, however many ticks pass.
    #[test]
    fn a_daily_trigger_fires_once_per_day() {
        let now = super::super::now_millis();
        let (h, _, _, _) = local_parts(now);
        let past = if h == 0 { 0 } else { h - 1 };
        let mut t = Trigger {
            id: 1,
            when: When::At { hour: past, min: 0, days: EVERY_DAY },
            what: task_what(),
            enabled: true,
            created: now - 86_400_000,
            by: "user".into(),
            space: None,
            last_fired: None,
            fire_count: 0,
            only_if: None,
            last_eval: None,
            deleted: false,
        };
        assert!(t.is_due(now));
        t.last_fired = Some(now);
        assert!(!t.is_due(now), "already run for this occurrence");
        assert!(t.is_due(now + 86_400_000), "and due again tomorrow");
    }

    #[test]
    fn days_parse_as_lists_ranges_and_wrapping_ranges() {
        let bit = |i: u32| 1u8 << i;
        assert_eq!(parse_days("mon-fri").unwrap(), bit(1) | bit(2) | bit(3) | bit(4) | bit(5));
        assert_eq!(parse_days("sat,sun").unwrap(), bit(6) | bit(0));
        assert_eq!(parse_days("mon,wed,fri").unwrap(), bit(1) | bit(3) | bit(5));
        assert_eq!(parse_days("daily").unwrap(), EVERY_DAY);
        assert_eq!(parse_days("monday").unwrap(), bit(1), "longer forms match by prefix");
        // `fri-mon` has to mean the weekend, not nothing.
        assert_eq!(parse_days("fri-mon").unwrap(), bit(5) | bit(6) | bit(0) | bit(1));
        assert!(parse_days("funday").is_err());
    }

    /// A weekday rule must not fire at the weekend, which is the whole point of asking.
    #[test]
    fn a_day_filter_keeps_a_rule_off_the_days_it_excludes() {
        let now = super::super::now_millis();
        let (h, _, _, wday) = local_parts(now);
        let past = if h == 0 { 0 } else { h - 1 };
        let today = 1u8 << wday;

        let base = Trigger {
            id: 1,
            when: When::At { hour: past, min: 0, days: today },
            what: task_what(),
            enabled: true,
            created: now - 86_400_000,
            by: "user".into(),
            space: None,
            last_fired: None,
            fire_count: 0,
            only_if: None,
            last_eval: None,
            deleted: false,
        };
        assert!(base.is_due(now), "today is allowed, and the hour has passed");

        // Every day except today.
        let t = Trigger {
            when: When::At { hour: past, min: 0, days: EVERY_DAY & !today },
            ..base.clone()
        };
        assert!(!t.is_due(now), "the excluded day wins over the elapsed hour");
    }

    #[test]
    fn a_day_filter_reads_back_the_way_it_was_written() {
        let days = parse_days("mon-fri").unwrap();
        assert_eq!(When::At { hour: 9, min: 0, days }.describe(), "at 09:00 mon–fri");
        let days = parse_days("sat,sun").unwrap();
        assert_eq!(When::At { hour: 9, min: 0, days }.describe(), "at 09:00 sun,sat");
        // Every day needs no saying.
        assert_eq!(When::At { hour: 9, min: 0, days: EVERY_DAY }.describe(), "at 09:00");
    }

    /// Rules written before the day filter existed have to keep working.
    #[test]
    fn a_daily_rule_from_before_days_existed_replays_as_every_day() {
        let line = r#"{"id":1,"when":{"kind":"at","hour":9,"min":0},
            "what":{"kind":"task","text":"x"},"enabled":true,"created":0,"by":"user",
            "last_fired":null,"fire_count":0}"#
            .replace('\n', "");
        let t: Trigger = serde_json::from_str(&line).expect("an old entry must still parse");
        assert_eq!(t.when, When::At { hour: 9, min: 0, days: EVERY_DAY });
    }

    #[test]
    fn a_time_of_day_is_read_in_local_time() {
        assert_eq!(parse_at("09:00").unwrap(), When::At { hour: 9, min: 0, days: EVERY_DAY });
        assert_eq!(parse_at(" 21:30 ").unwrap(), When::At { hour: 21, min: 30, days: EVERY_DAY });
        assert!(parse_at("9am").is_err());
        assert!(parse_at("25:00").is_err());
        assert!(parse_at("09:71").is_err());

        // The occurrence lands on the requested local hour, which is the whole point of not
        // using the UTC clock the log lines use.
        let now = super::super::now_millis();
        let (h, m, _, _) = local_parts(last_occurrence(now, 9, 0));
        assert_eq!((h, m), (9, 0));
    }

    // -- guards ---------------------------------------------------------

    #[test]
    fn nothing_fires_until_unattended_is_turned_on() {
        let mut e = eng("switch");
        e.cfg.unattended = false;
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user", None, None).unwrap();
        wind_back(&mut e, 1, 120_000);
        assert!(fire_due(&mut e).is_empty());
        assert!(fired(&e).is_empty());
        assert_eq!(e.board.open_count(), 0, "and no work appeared");
    }

    #[test]
    fn a_due_trigger_puts_its_work_on_the_board() {
        let mut e = eng("board");
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user", None, None).unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);

        assert_eq!(e.board.open_count(), 1);
        let t = &e.board.all()[0];
        assert_eq!(t.text, "review yesterday's diff");
        // Tagged with its origin, which is both the audit trail and how the dedupe finds it.
        assert_eq!(t.by, owner_tag(1));
        assert_eq!(e.triggers.get(1).unwrap().fire_count, 1);
        assert_eq!(fired(&e).len(), 1, "and the journal says what happened");
        assert!(fired(&e)[0].contains("review yesterday's diff"), "{:?}", fired(&e));
    }

    /// The guard that keeps a schedule from becoming a pile: yesterday's task still sitting on
    /// the board is the reason not to add today's.
    #[test]
    fn a_trigger_does_not_stack_work_it_has_already_queued() {
        let mut e = eng("stack");
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user", None, None).unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(e.board.open_count(), 1);

        // Due again, but its work is untouched.
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(e.board.open_count(), 1, "one in flight, not two");

        // Claimed still counts as in flight — an agent is working on it.
        e.board.claim("worker0", None, None).unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(e.board.all().len(), 1);

        // Finished, so the next one is welcome.
        e.board.done("worker0", None, Some("nothing broken")).unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(e.board.all().len(), 2, "the schedule resumes once the work clears");
    }

    /// Skipping must not spend the interval, or clearing the board would start a fresh wait.
    #[test]
    fn work_still_outstanding_delays_a_firing_without_consuming_it() {
        let mut e = eng("nospend");
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user", None, None).unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);

        // Captured after winding back, since winding back moves `last_fired` itself — reading
        // it before could not tell a skipped firing from the rewind.
        wind_back(&mut e, 1, 120_000);
        let before = e.triggers.get(1).unwrap().last_fired;
        fire_due(&mut e); // skipped: still outstanding
        assert_eq!(e.triggers.get(1).unwrap().last_fired, before, "a skip is not a firing");

        // So the moment the work clears, it goes — without waiting out another interval.
        e.board.claim("worker0", None, None).unwrap();
        e.board.done("worker0", None, None).unwrap();
        fire_due(&mut e);
        assert_eq!(e.board.all().len(), 2);
    }

    #[test]
    fn a_disabled_trigger_stays_quiet() {
        let mut e = eng("disabled");
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user", None, None).unwrap();
        e.triggers.set_enabled(1, false).unwrap();
        wind_back(&mut e, 1, 120_000);
        assert!(fire_due(&mut e).is_empty());
        assert_eq!(e.board.open_count(), 0);
    }

    /// The ceiling exists because agents can create triggers, so the thing to survive is not
    /// one misconfigured rule but a pile of them.
    #[test]
    fn the_hourly_ceiling_holds_the_rest_and_says_so() {
        let mut e = eng("ceiling");
        for i in 0..MAX_PER_HOUR + 3 {
            e.triggers
                .add(
                    When::Every { secs: 60 },
                    What::Task { text: format!("job {i}") },
                    "user",
                    None,
                    None,
                )
                .unwrap();
            wind_back(&mut e, i as u64 + 1, 120_000);
        }
        // The pass that fills the budget says how many it is holding back.
        let events = fire_due(&mut e);
        assert_eq!(e.board.open_count(), MAX_PER_HOUR, "no more than the ceiling");
        assert!(warned_about(&events, "ceiling"), "a refusal has to be visible: {events:?}");

        // The next pass has no budget at all, and says so once.
        let events = fire_due(&mut e);
        assert_eq!(e.board.open_count(), MAX_PER_HOUR);
        assert!(
            warned_about(&events, "ceiling"),
            "hitting the wall is not a silent no-op: {events:?}"
        );

        // And then stays quiet: the condition persists for an hour, the complaint does not.
        assert!(fire_due(&mut e).is_empty(), "one warning an hour, not one per tick");
        assert_eq!(e.board.open_count(), MAX_PER_HOUR);
    }

    /// A trigger pointing at an agent that no longer exists would otherwise retry every tick
    /// for as long as you are away.
    #[test]
    fn an_action_that_fails_still_counts_as_a_firing() {
        let mut e = eng("failure");
        e.triggers
            .add(
                When::Every { secs: 60 },
                What::Send { to: "nobody".into(), body: "hello".into() },
                "user",
                None,
                    None,
            )
            .unwrap();
        wind_back(&mut e, 1, 120_000);
        let events = fire_due(&mut e);
        assert!(warned_about(&events, "failed"), "{events:?}");
        assert_eq!(e.triggers.get(1).unwrap().fire_count, 1, "spent, so it will not spin");
    }

    #[test]
    fn a_send_trigger_reaches_a_named_agent() {
        let mut e = eng("send");
        e.triggers
            .add(
                When::Every { secs: 60 },
                What::Send { to: "worker0".into(), body: "status?".into() },
                "user",
                None,
                    None,
            )
            .unwrap();
        wind_back(&mut e, 1, 120_000);
        let events = fire_due(&mut e);
        let sent = events.iter().any(
            |ev| matches!(ev, Event::BusMessage(m) if m.to == "worker0" && m.body == "status?"),
        );
        assert!(sent, "{events:?}");
    }

    // -- spawning -------------------------------------------------------
    // The action that changes what horde is rather than what it does, so what is pinned here is
    // the bound on it and the record of it.

    fn spawn_what() -> What {
        // A shell, not a real agent: this exercises the pane and the provenance, and starting
        // `claude` in a unit test would be neither fast nor polite.
        What::Spawn { cmd: "cat".into(), name: Some("nightly".into()) }
    }

    #[test]
    fn a_spawn_trigger_starts_an_agent_and_stamps_where_it_came_from() {
        let mut e = eng("spawn");
        let before = e.session.panes.len();
        e.triggers.add(When::Every { secs: 60 }, spawn_what(), "user", None, None).unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);

        assert_eq!(e.session.panes.len(), before + 1, "a pane should have appeared");
        let spawned: Vec<_> =
            e.session.panes.values().filter(|p| p.spawned_by == Some(1)).collect();
        assert_eq!(spawned.len(), 1, "and it must carry the trigger that started it");
        assert_eq!(spawned[0].name.as_deref(), Some("nightly"));
        assert_eq!(live_spawned(&e), 1);
        assert!(fired(&e)[0].contains("spawned nightly"), "{:?}", fired(&e));
        kill_panes(&mut e);
    }

    /// The bound on how many full-permission agents can be working with nobody present.
    #[test]
    fn the_spawn_cap_refuses_loudly_rather_than_quietly() {
        let mut e = eng("cap");
        e.cfg.max_spawned = 1;
        e.triggers.add(When::Every { secs: 60 }, spawn_what(), "user", None, None).unwrap();

        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(live_spawned(&e), 1);

        // Due again, cap full: a warning, and no second agent.
        wind_back(&mut e, 1, 120_000);
        let events = fire_due(&mut e);
        assert_eq!(live_spawned(&e), 1, "the cap holds");
        assert!(warned_about(&events, "cap is 1"), "{events:?}");
        // Spent, so it warns once per interval rather than once per tick.
        assert_eq!(e.triggers.get(1).unwrap().fire_count, 2);
        kill_panes(&mut e);
    }

    /// A spawned agent that finishes gives its slot back — the cap counts what is running, not
    /// what has ever run.
    #[test]
    fn a_departed_agent_frees_its_slot() {
        let mut e = eng("slot");
        e.cfg.max_spawned = 1;
        e.triggers.add(When::Every { secs: 60 }, spawn_what(), "user", None, None).unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(live_spawned(&e), 1);

        // Marked exited rather than removed from the map: dropping a pane without touching the
        // layout tree leaves the tree pointing at it, and the next split has no valid target.
        // This is also the state a real exit passes through before it is reaped.
        let id = *e
            .session
            .panes
            .iter()
            .find(|(_, p)| p.spawned_by.is_some())
            .map(|(id, _)| id)
            .unwrap();
        e.session.panes.get_mut(&id).unwrap().exited = Some(0);
        assert_eq!(live_spawned(&e), 0, "a finished agent holds no slot");

        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(live_spawned(&e), 1, "the schedule resumes once a slot is free");
        kill_panes(&mut e);
    }

    // -- conditions -----------------------------------------------------

    /// Drive `fire_due` until the trigger's probe has been answered, or give up.
    ///
    /// A condition is answered on a thread, so the decision lands a tick or two after the rule
    /// comes due. Polling is what the real tick loop does; this just does it faster.
    fn settle(e: &mut Engine) -> Vec<Event> {
        for _ in 0..200 {
            let events = fire_due(e);
            if e.triggers.probes.is_empty() {
                return events;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the condition never came back");
    }

    /// The offset is the half that carries the information — `09:00` looks correct in every
    /// timezone, and `UTC+00` on a machine whose owner is not on UTC is the tell.
    #[test]
    fn the_local_clock_shows_the_offset_that_produced_it() {
        assert_eq!(format_clock(9, 0, 0), "09:00 UTC+00");
        assert_eq!(format_clock(14, 30, 3600), "14:30 UTC+01");
        assert_eq!(format_clock(7, 5, -5 * 3600), "07:05 UTC-05");
        // Zones off the hour boundary keep their minutes.
        assert_eq!(format_clock(12, 0, 5 * 3600 + 1800), "12:00 UTC+05:30");
        // And a negative one takes its sign before its magnitude, or the minutes come out
        // negative too: `-03:-30`.
        assert_eq!(format_clock(12, 0, -(3 * 3600 + 1800)), "12:00 UTC-03:30");
    }

    /// Whatever this machine's timezone is, the shape has to be readable.
    #[test]
    fn the_local_clock_reads_as_a_time_and_an_offset() {
        let s = local_clock(super::super::now_millis());
        let (hm, off) = s.split_once(' ').expect("a time then an offset");
        assert_eq!(hm.len(), 5, "{s}");
        assert!(hm[..2].parse::<u32>().is_ok_and(|h| h < 24), "{s}");
        assert!(hm[3..].parse::<u32>().is_ok_and(|m| m < 60), "{s}");
        assert!(off.starts_with("UTC+") || off.starts_with("UTC-"), "{s}");
    }

    /// A trigger reaches the board directly, not through the socket, so closing the board has
    /// to be checked here too — otherwise "no board" still fills up overnight on a schedule.
    #[test]
    fn a_closed_board_stops_a_scheduled_task_from_landing() {
        let mut e = super::super::tests::engine();
        e.cfg.unattended = true;
        e.cfg.board = false;
        let t = Trigger {
            id: 1,
            when: When::Every { secs: 1800 },
            what: task_what(),
            enabled: true,
            created: super::super::now_millis(),
            by: "user".into(),
            space: None,
            last_fired: None,
            fire_count: 0,
            only_if: None,
            last_eval: None,
            deleted: false,
        };
        // Counted relative to where it started: the test board is a file in the temp dir shared
        // by every test in this binary, so an absolute count is really an assertion about what
        // else happened to be running.
        let before = e.board.open_count();
        let err = perform(&mut e, &t).unwrap_err().to_string();
        assert!(err.contains("agents.board"), "{err}");
        assert_eq!(e.board.open_count(), before, "nothing may have landed");

        // Open it again and the same rule works, so this is a switch rather than a removal.
        e.cfg.board = true;
        assert!(perform(&mut e, &t).is_ok());
        assert_eq!(e.board.open_count(), before + 1);
    }

    #[test]
    fn a_met_condition_lets_the_rule_act() {
        let mut e = eng("cond-yes");
        e.triggers
            .add(When::Every { secs: 60 }, task_what(), "user", Some("true".into()), None)
            .unwrap();
        wind_back(&mut e, 1, 120_000);
        settle(&mut e);
        assert_eq!(e.board.open_count(), 1, "the condition held, so the work went up");
        assert_eq!(e.triggers.get(1).unwrap().fire_count, 1);
    }

    /// The important half: an unmet condition spends the interval. Without that the probe re-runs
    /// on every tick, which for a real command is a fork bomb with good intentions.
    #[test]
    fn an_unmet_condition_holds_the_rule_and_spends_the_interval() {
        let mut e = eng("cond-no");
        e.triggers
            .add(When::Every { secs: 60 }, task_what(), "user", Some("false".into()), None)
            .unwrap();
        wind_back(&mut e, 1, 120_000);
        settle(&mut e);

        assert_eq!(e.board.open_count(), 0, "the condition said no");
        assert_eq!(e.triggers.get(1).unwrap().fire_count, 0, "and that is not a firing");
        let evaluated = e.triggers.get(1).unwrap().last_eval;
        assert!(evaluated.is_some(), "but the interval was spent");

        // So the next pass does not immediately probe again.
        fire_due(&mut e);
        assert!(e.triggers.probes.is_empty(), "no second probe until the interval comes round");
        assert_eq!(e.triggers.get(1).unwrap().last_eval, evaluated);
    }

    /// One probe per rule at a time, or a slow condition is launched again every 150ms.
    #[test]
    fn a_condition_still_running_is_not_launched_again() {
        let mut e = eng("cond-once");
        e.triggers
            .add(When::Every { secs: 60 }, task_what(), "user", Some("sleep 5".into()), None)
            .unwrap();
        wind_back(&mut e, 1, 120_000);

        for _ in 0..5 {
            fire_due(&mut e);
        }
        assert_eq!(e.triggers.probes.len(), 1, "five passes, one probe");
        assert_eq!(e.board.open_count(), 0, "and nothing acted while it was thinking");

        // Abandon it rather than waiting five seconds for the test.
        e.triggers.probes.clear();
    }

    /// A condition that hangs must not hold its rule forever.
    #[test]
    fn a_condition_that_never_answers_is_abandoned_with_a_warning() {
        let mut e = eng("cond-hang");
        e.triggers
            .add(When::Every { secs: 60 }, task_what(), "user", Some("sleep 60".into()), None)
            .unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(e.triggers.probes.len(), 1);

        // Backdate the probe past the timeout rather than waiting a minute for it.
        if let Some(p) = e.triggers.probes.get_mut(&1) {
            p.started = super::super::now_millis().saturating_sub(PROBE_TIMEOUT + 1);
        }
        let events = fire_due(&mut e);
        assert!(warned_about(&events, "abandoned"), "{events:?}");
        assert!(e.triggers.probes.is_empty(), "and the slot is free again");
        assert_eq!(e.board.open_count(), 0);
    }

    /// Rules written before conditions existed have to keep their schedule, not all come due at
    /// once because a new field defaulted to None.
    #[test]
    fn a_rule_from_before_conditions_existed_keeps_its_place_in_the_schedule() {
        let fired_at = super::super::now_millis() - 1000;
        let line = format!(
            r#"{{"id":1,"when":{{"kind":"every","secs":3600}},"what":{{"kind":"task","text":"x"}},
               "enabled":true,"created":0,"by":"user","last_fired":{fired_at},"fire_count":4}}"#
        )
        .replace('\n', "");
        let t: Trigger = serde_json::from_str(&line).expect("an old entry must still parse");
        assert!(t.only_if.is_none());
        assert!(t.last_eval.is_none());
        // Falls back to `last_fired`, so it is one second into its hour rather than overdue.
        assert!(!t.is_due(super::super::now_millis()));
    }

    #[test]
    fn descriptions_read_back_the_way_they_were_written() {
        assert_eq!(When::Every { secs: 1800 }.describe(), "every 30m");
        assert_eq!(When::Every { secs: 7200 }.describe(), "every 2h");
        assert_eq!(When::Every { secs: 86_400 }.describe(), "every 1d");
        assert_eq!(When::Every { secs: 90 }.describe(), "every 90s");
        assert_eq!(When::At { hour: 9, min: 0, days: EVERY_DAY }.describe(), "at 09:00");
        assert_eq!(task_what().describe(), "board: review yesterday's diff");
    }
}
