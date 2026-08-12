//! The part that acts when nobody is watching.
//!
//! Everything else in horde waits to be asked. The bus routes a message someone sent, the board
//! holds work someone added, an agent takes a task because it was told to look. That makes horde
//! a workshop: capable, and completely inert until you are in it. A trigger is the thing that
//! pulls when the room is empty.
//!
//! Which is a much larger promise than it sounds, so almost all of this file is about *not*
//! firing. The mechanism is a timestamp comparison; the engineering is six guards:
//!
//! - **A master switch, off by default.** A fresh install never acts on its own.
//! - **One piece of work in flight per trigger.** A daily task still sitting on the board is
//!   the reason not to add a second one.
//! - **A floor on the interval**, so `every 1s` cannot be asked for.
//! - **A ceiling on firings per hour**, across all triggers — because agents can create these,
//!   so the failure mode is not one bad rule but fifty.
//! - **A failed action still counts as a firing**, or a broken trigger retries every tick
//!   forever.
//! - **Everything is journaled**, because a machine that acts while you are away is only
//!   trustworthy if you can read back what it decided to do.
//!
//! The action to reach for is [`What::Task`]. It puts work on the board and lets the nudge that
//! already exists find a free agent, which means a trigger never has to know who is idle and the
//! exclusivity guarantee stays where it already is — in `Board::claim`'s compare-and-set.

use std::path::PathBuf;

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
    /// Daily, at a local wall-clock time.
    At { hour: u32, min: u32 },
}

/// What a trigger does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum What {
    /// Put work on the board. The one to reach for: it composes with the nudge and the claim.
    Task { text: String },
    /// Push a line at one named agent. Bypasses the board, so it also bypasses everything the
    /// board guarantees — worth it only when the work belongs to a specific agent.
    Send { to: String, body: String },
}

impl When {
    pub fn describe(&self) -> String {
        match self {
            When::Every { secs } => format!("every {}", secs_words(*secs)),
            When::At { hour, min } => format!("at {hour:02}:{min:02}"),
        }
    }
}

impl What {
    pub fn describe(&self) -> String {
        match self {
            What::Task { text } => format!("board: {text}"),
            What::Send { to, body } => format!("send {to}: {body}"),
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
    pub last_fired: Option<u64>,
    pub fire_count: u64,
    /// Removed. Kept in the log so the record of what once fired here survives, and hidden
    /// from every listing.
    #[serde(default)]
    pub deleted: bool,
}

impl Trigger {
    /// What the next firing is measured from: the last one, or creation for a trigger that has
    /// never fired.
    ///
    /// Creation rather than zero, so adding `every 30m` waits its thirty minutes instead of
    /// firing the instant you add it — and adding `at 09:00` in the afternoon waits for
    /// tomorrow rather than deciding it is nine hours late.
    fn baseline(&self) -> u64 {
        self.last_fired.unwrap_or(self.created)
    }

    fn is_due(&self, now: u64) -> bool {
        match &self.when {
            When::Every { secs } => now.saturating_sub(self.baseline()) >= secs * 1000,
            // Late rather than skipped: if the daemon was down at nine, a trigger that has not
            // run since before nine still runs when it comes back. Being told at eleven that
            // yesterday's diff wants reviewing beats never being told.
            When::At { hour, min } => {
                let occurrence = last_occurrence(now, *hour, *min);
                self.baseline() < occurrence
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
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        let triggers = read_log(&path);
        let next_id = triggers.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        Store {
            log: super::logfile::AppendLog::new(path),
            triggers,
            next_id,
            capped_notice_at: 0,
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

    pub fn add(&mut self, when: When, what: What, by: &str) -> Result<Trigger> {
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
            last_fired: None,
            fire_count: 0,
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
            t.fire_count += 1;
        });
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
            let task = eng.board.add(text, &owner_tag(t.id))?;
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
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// The most recent time today's — or yesterday's — `hour:min` came round, in unix millis.
fn last_occurrence(now: u64, hour: u32, min: u32) -> u64 {
    let (h, m, s) = local_hms(now);
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

/// Local wall-clock `(hour, minute, second)`. `at 09:00` has to mean nine where you are, and
/// the log's UTC clock is not good enough for that.
fn local_hms(ms: u64) -> (u32, u32, u32) {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes only the `tm` we hand it, which is the reentrant form's
    // whole point. On failure it returns null and leaves the zeroed struct, which reads as
    // midnight — wrong, but bounded, and it cannot fail for a value that came from the clock.
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
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
    Ok(When::At { hour, min })
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

    /// Pretend a trigger last fired `ms_ago` milliseconds ago.
    fn wind_back(e: &mut Engine, id: u64, ms_ago: u64) {
        let now = super::super::now_millis();
        let _ = e.triggers.mutate(id, |t| {
            t.created = now.saturating_sub(ms_ago);
            t.last_fired = t.last_fired.map(|_| now.saturating_sub(ms_ago));
        });
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
        let a = s.add(When::Every { secs: 1800 }, task_what(), "user").unwrap();
        let b = s.add(When::At { hour: 9, min: 0 }, task_what(), "builder").unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert!(a.enabled);
        assert_eq!(b.by, "builder");
        assert_eq!(s.count(), 2);
    }

    #[test]
    fn an_interval_below_the_floor_is_refused() {
        let mut s = store("floor");
        let err = s.add(When::Every { secs: 1 }, task_what(), "user").unwrap_err().to_string();
        assert!(err.contains("shortest interval"), "{err}");
        assert!(parse_every("5s").is_err(), "and at the parsing boundary too");
        assert!(parse_every("60s").is_ok());
    }

    #[test]
    fn removing_a_trigger_hides_it_without_losing_the_record() {
        let mut s = store("rm");
        s.add(When::Every { secs: 3600 }, task_what(), "user").unwrap();
        s.remove(1).unwrap();
        assert_eq!(s.count(), 0);
        assert!(s.get(1).is_none());
        assert!(s.remove(1).is_err(), "and it cannot be removed twice");
    }

    #[test]
    fn everything_can_be_turned_off_at_once() {
        let mut s = store("offall");
        s.add(When::Every { secs: 3600 }, task_what(), "user").unwrap();
        s.add(When::Every { secs: 7200 }, task_what(), "user").unwrap();
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
            s.add(When::Every { secs: 1800 }, task_what(), "user").unwrap();
            s.add(When::At { hour: 9, min: 30 }, task_what(), "user").unwrap();
            s.set_enabled(1, false).unwrap();
        }
        let mut s = Store::new(p.clone());
        assert_eq!(s.count(), 2);
        assert!(!s.get(1).unwrap().enabled, "off has to survive too, or a restart re-arms it");
        assert_eq!(s.get(2).unwrap().when, When::At { hour: 9, min: 30 });
        // Ids do not restart, so a replayed log cannot collide with a new rule.
        assert_eq!(s.add(When::Every { secs: 60 }, task_what(), "user").unwrap().id, 3);
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
            last_fired: None,
            fire_count: 0,
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
        let (h, _, _) = local_hms(now);
        // An hour that has already come round today, whatever time the test runs.
        let past = if h == 0 { 0 } else { h - 1 };
        let t = Trigger {
            id: 1,
            when: When::At { hour: past, min: 0 },
            what: task_what(),
            enabled: true,
            created: now,
            by: "user".into(),
            last_fired: None,
            fire_count: 0,
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
        let (h, _, _) = local_hms(now);
        let past = if h == 0 { 0 } else { h - 1 };
        let mut t = Trigger {
            id: 1,
            when: When::At { hour: past, min: 0 },
            what: task_what(),
            enabled: true,
            created: now - 86_400_000,
            by: "user".into(),
            last_fired: None,
            fire_count: 0,
            deleted: false,
        };
        assert!(t.is_due(now));
        t.last_fired = Some(now);
        assert!(!t.is_due(now), "already run for this occurrence");
        assert!(t.is_due(now + 86_400_000), "and due again tomorrow");
    }

    #[test]
    fn a_time_of_day_is_read_in_local_time() {
        assert_eq!(parse_at("09:00").unwrap(), When::At { hour: 9, min: 0 });
        assert_eq!(parse_at(" 21:30 ").unwrap(), When::At { hour: 21, min: 30 });
        assert!(parse_at("9am").is_err());
        assert!(parse_at("25:00").is_err());
        assert!(parse_at("09:71").is_err());

        // The occurrence lands on the requested local hour, which is the whole point of not
        // using the UTC clock the log lines use.
        let now = super::super::now_millis();
        let (h, m, _) = local_hms(last_occurrence(now, 9, 0));
        assert_eq!((h, m), (9, 0));
    }

    // -- guards ---------------------------------------------------------

    #[test]
    fn nothing_fires_until_unattended_is_turned_on() {
        let mut e = eng("switch");
        e.cfg.unattended = false;
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user").unwrap();
        wind_back(&mut e, 1, 120_000);
        assert!(fire_due(&mut e).is_empty());
        assert!(fired(&e).is_empty());
        assert_eq!(e.board.open_count(), 0, "and no work appeared");
    }

    #[test]
    fn a_due_trigger_puts_its_work_on_the_board() {
        let mut e = eng("board");
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user").unwrap();
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
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user").unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(e.board.open_count(), 1);

        // Due again, but its work is untouched.
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);
        assert_eq!(e.board.open_count(), 1, "one in flight, not two");

        // Claimed still counts as in flight — an agent is working on it.
        e.board.claim("worker0", None).unwrap();
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
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user").unwrap();
        wind_back(&mut e, 1, 120_000);
        fire_due(&mut e);

        // Captured after winding back, since winding back moves `last_fired` itself — reading
        // it before could not tell a skipped firing from the rewind.
        wind_back(&mut e, 1, 120_000);
        let before = e.triggers.get(1).unwrap().last_fired;
        fire_due(&mut e); // skipped: still outstanding
        assert_eq!(e.triggers.get(1).unwrap().last_fired, before, "a skip is not a firing");

        // So the moment the work clears, it goes — without waiting out another interval.
        e.board.claim("worker0", None).unwrap();
        e.board.done("worker0", None, None).unwrap();
        fire_due(&mut e);
        assert_eq!(e.board.all().len(), 2);
    }

    #[test]
    fn a_disabled_trigger_stays_quiet() {
        let mut e = eng("disabled");
        e.triggers.add(When::Every { secs: 60 }, task_what(), "user").unwrap();
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
                .add(When::Every { secs: 60 }, What::Task { text: format!("job {i}") }, "user")
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
            )
            .unwrap();
        wind_back(&mut e, 1, 120_000);
        let events = fire_due(&mut e);
        let sent = events.iter().any(
            |ev| matches!(ev, Event::BusMessage(m) if m.to == "worker0" && m.body == "status?"),
        );
        assert!(sent, "{events:?}");
    }

    #[test]
    fn descriptions_read_back_the_way_they_were_written() {
        assert_eq!(When::Every { secs: 1800 }.describe(), "every 30m");
        assert_eq!(When::Every { secs: 7200 }.describe(), "every 2h");
        assert_eq!(When::Every { secs: 86_400 }.describe(), "every 1d");
        assert_eq!(When::Every { secs: 90 }.describe(), "every 90s");
        assert_eq!(When::At { hour: 9, min: 0 }.describe(), "at 09:00");
        assert_eq!(task_what().describe(), "board: review yesterday's diff");
    }
}
