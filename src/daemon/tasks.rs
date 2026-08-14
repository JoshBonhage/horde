//! A shared task board agents pull work from.
//!
//! The bus lets you push work at a named agent. This is the other direction: put work on a
//! board and let whoever is free take it. That turns "spawn three agents and dispatch to each"
//! into "spawn three agents and add ten tasks", which is the difference between you being the
//! scheduler and you not being needed.
//!
//! Claiming is the only operation that has to be exactly right. Two agents claiming the same
//! task would duplicate work silently, so a claim is a compare-and-set: it succeeds only from
//! `Open`, and the daemon's single-threaded engine serialises the attempts.
//!
//! # Scope, which is what made this unusable the first time
//!
//! A task belongs to a **project**, and is only ever offered to agents in that project. The
//! board shipped without this, and with more than one project open it does not degrade, it
//! inverts: work added in one repository is handed to an idle agent sitting in another, which
//! claims it and starts editing the wrong tree. With a single project the flaw is invisible,
//! which is exactly why it survived.
//!
//! The scope is the space's **name**, not its id. Ids are persisted by position and are not
//! stable across a restart, so a task holding one would point at a different project the next
//! morning. A rename orphans a task, which is rarer and far more visible than a restart.
//!
//! # Staleness
//!
//! An open task is replayed from the log forever. That is right for a board you are working
//! and wrong for one you walked away from: a week-old task is not work waiting for an agent,
//! it is something you forgot about, and offering it to a fleet on the next restart is how a
//! quiet morning turns into three agents doing archaeology. Past [`STALE_AFTER`] a task stops
//! being offered and says so, rather than vanishing — the record is still the record.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Tasks kept in memory. Beyond this the oldest done ones are forgotten.
const CAP: usize = 2000;

/// How long an open task goes on being offered.
///
/// A day, because that is the span of "I was working on this" — anything older outlived the
/// session that created it, and the person who added it is not expecting an agent to pick it
/// up unannounced. It is a threshold on *offering*, not on the record: a stale task still
/// lists, still reads, and can still be claimed by name.
pub const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Open,
    Claimed,
    Done,
    /// Given up on. Kept rather than deleted so the record of the attempt survives.
    Dropped,
}

impl TaskState {
    /// For error text: "already claimed by builder" reads better than a Debug dump.
    pub fn word(&self) -> &'static str {
        match self {
            TaskState::Open => "open",
            TaskState::Claimed => "claimed",
            TaskState::Done => "done",
            TaskState::Dropped => "dropped",
        }
    }

    pub fn glyph(&self) -> &'static str {
        match self {
            TaskState::Open => "○",
            TaskState::Claimed => "◐",
            TaskState::Done => "●",
            TaskState::Dropped => "✕",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub text: String,
    pub state: TaskState,
    /// Unix millis.
    pub created: u64,
    /// Who added it. `user` when it came from outside a pane.
    pub by: String,
    /// Which project it belongs to, by space *name*.
    ///
    /// `None` only for tasks written before the board carried scope; those are claimable by
    /// anyone, which is the behaviour they were added under. Everything new is scoped.
    #[serde(default)]
    pub space: Option<String>,
    /// Agent holding it, once claimed.
    pub owner: Option<String>,
    pub claimed_at: Option<u64>,
    pub done_at: Option<u64>,
    /// Whatever the agent said when finishing.
    pub result: Option<String>,
}

impl Task {
    pub fn is_open(&self) -> bool {
        self.state == TaskState::Open
    }

    /// Open and claimed together are "outstanding" — what the board is for.
    pub fn is_claimed(&self) -> bool {
        self.state == TaskState::Claimed
    }

    /// Too old to go on being offered. See [`STALE_AFTER`].
    pub fn is_stale(&self, now: u64) -> bool {
        self.is_open() && now.saturating_sub(self.created) > STALE_AFTER.as_millis() as u64
    }

    /// Work this task is available for: its own project, or any when it has no scope.
    pub fn open_to(&self, space: Option<&str>) -> bool {
        match (&self.space, space) {
            (None, _) => true,
            (Some(mine), Some(theirs)) => mine == theirs,
            (Some(_), None) => false,
        }
    }
}

pub struct Board {
    log: super::logfile::AppendLog,
    tasks: Vec<Task>,
    next_id: u64,
}

impl Board {
    pub fn new(path: PathBuf) -> Board {
        let tasks = read_log(&path);
        let next_id = tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        Board { log: super::logfile::AppendLog::new(path), tasks, next_id }
    }

    pub fn all(&self) -> &[Task] {
        &self.tasks
    }

    #[cfg(test)]
    pub fn get(&self, id: u64) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn open_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.is_open()).count()
    }

    pub fn claimed_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.state == TaskState::Claimed).count()
    }

    pub fn add(&mut self, text: &str, by: &str, space: Option<&str>) -> Result<Task> {
        let text = text.trim();
        if text.is_empty() {
            return Err(anyhow!("a task needs a description"));
        }
        let task = Task {
            id: self.next_id,
            text: text.to_string(),
            state: TaskState::Open,
            created: super::now_millis(),
            by: by.to_string(),
            space: space.map(|s| s.to_string()),
            owner: None,
            claimed_at: None,
            done_at: None,
            result: None,
        };
        self.next_id += 1;
        self.record(task.clone());
        Ok(task)
    }

    /// Take the oldest open task in `space`, or a specific one by id.
    ///
    /// Returning `None` for "nothing to do" rather than an error is deliberate: an agent
    /// looping on the board should be able to tell "empty" from "broken".
    ///
    /// Scope binds the unnamed claim only. Naming an id is you pointing at one task, and
    /// refusing that because of which pane you happened to run it from would be obstruction
    /// rather than safety — the mistake this guards against is the *automatic* pickup.
    /// Staleness works the same way: too old to be offered, never too old to be asked for.
    pub fn claim(&mut self, owner: &str, id: Option<u64>, space: Option<&str>) -> Result<Option<Task>> {
        let now = super::now_millis();
        let idx = match id {
            Some(id) => {
                let i = self
                    .tasks
                    .iter()
                    .position(|t| t.id == id)
                    .ok_or_else(|| anyhow!("no task #{id}"))?;
                // A compare-and-set, not a write: whoever got here first keeps it.
                if !self.tasks[i].is_open() {
                    let t = &self.tasks[i];
                    return Err(anyhow!(
                        "task #{id} is already {}{}",
                        t.state.word(),
                        t.owner.as_ref().map(|o| format!(" by {o}")).unwrap_or_default()
                    ));
                }
                i
            }
            None => {
                match self
                    .tasks
                    .iter()
                    .position(|t| t.is_open() && t.open_to(space) && !t.is_stale(now))
                {
                    Some(i) => i,
                    None => return Ok(None),
                }
            }
        };

        let t = &mut self.tasks[idx];
        t.state = TaskState::Claimed;
        t.owner = Some(owner.to_string());
        t.claimed_at = Some(super::now_millis());
        let out = t.clone();
        self.append(&out);
        Ok(Some(out))
    }

    /// Finish a task. `id` defaults to the caller's own claimed task.
    pub fn done(&mut self, owner: &str, id: Option<u64>, result: Option<&str>) -> Result<Task> {
        let idx = match id {
            Some(id) => self
                .tasks
                .iter()
                .position(|t| t.id == id)
                .ok_or_else(|| anyhow!("no task #{id}"))?,
            None => self
                .tasks
                .iter()
                .position(|t| t.state == TaskState::Claimed && t.owner.as_deref() == Some(owner))
                .ok_or_else(|| {
                    anyhow!("you have no claimed task — claim one first, or name an id")
                })?,
        };
        let t = &mut self.tasks[idx];
        t.state = TaskState::Done;
        t.done_at = Some(super::now_millis());
        t.result = result.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
        // Record who finished it even if they never claimed it, so the log is honest.
        if t.owner.is_none() {
            t.owner = Some(owner.to_string());
        }
        let out = t.clone();
        self.append(&out);
        Ok(out)
    }

    /// Put a claimed task back, or abandon one entirely.
    pub fn release(&mut self, id: u64, drop_it: bool) -> Result<Task> {
        let idx = self
            .tasks
            .iter()
            .position(|t| t.id == id)
            .ok_or_else(|| anyhow!("no task #{id}"))?;
        let t = &mut self.tasks[idx];
        if drop_it {
            t.state = TaskState::Dropped;
            t.done_at = Some(super::now_millis());
        } else {
            t.state = TaskState::Open;
            t.owner = None;
            t.claimed_at = None;
        }
        let out = t.clone();
        self.append(&out);
        Ok(out)
    }

    /// Drop every open task, optionally only one project's.
    ///
    /// The thing the board was missing. `release --drop` is one task at a time, which is no
    /// use against a board that accumulated forty of them over a week, and "I have stopped
    /// caring about all of this" is a real and frequent intention. Recorded as `dropped`
    /// rather than deleted, because the log is the record and a task that quietly vanished
    /// would be indistinguishable from one that was never added.
    /// `space` of `None` means *every* project, which is the opposite of what `None` means to
    /// [`Task::open_to`]. The two readings are genuinely different questions: claiming asks
    /// "may this task come to me", where an unknown scope must not sweep up other projects'
    /// work, and clearing asks "which of these am I throwing away", where naming no project is
    /// how you say all of them. Sharing one predicate between them made `clear --everywhere`
    /// drop nothing at all, which is worse than either reading.
    pub fn clear(&mut self, space: Option<&str>, claimed_too: bool) -> Vec<Task> {
        let mut dropped = Vec::new();
        for t in self.tasks.iter_mut() {
            let matches_state = t.is_open() || (claimed_too && t.is_claimed());
            let matches_space = match space {
                None => true,
                // An unscoped task is nobody's project and everybody's mess, so it clears
                // with whichever project you happen to be standing in.
                Some(want) => t.space.as_deref() == Some(want) || t.space.is_none(),
            };
            if matches_state && matches_space {
                t.state = TaskState::Dropped;
                t.done_at = Some(super::now_millis());
                dropped.push(t.clone());
            }
        }
        for t in &dropped {
            self.append(t);
        }
        dropped
    }

    /// Open, fresh tasks belonging to `space` and to no other reading of the word.
    ///
    /// Stricter than [`Task::open_to`] on purpose. Claiming is permissive — an unscoped task
    /// can be taken by anyone, which is how unscoped tasks already behaved — but *offering* is
    /// the operation that goes and interrupts an agent, so it demands the project be named.
    /// An unscoped task therefore sits there until somebody asks for it, which is exactly what
    /// should happen to work nobody said where to do.
    pub fn offered_to(&self, space: &str) -> usize {
        let now = super::now_millis();
        self.tasks
            .iter()
            .filter(|t| t.is_open() && !t.is_stale(now))
            .filter(|t| t.space.as_deref() == Some(space))
            .count()
    }

    /// Hand back any task whose owner is no longer among the live claimants.
    ///
    /// Without this a crashed agent's work would sit claimed forever and the board would
    /// quietly stall. Phrased as "who is still here" rather than "who left" on purpose: a
    /// pane that closes before detection ever named it leaves no departure to notice, but it
    /// is just as absent from the live list.
    ///
    /// `user` is exempt — the human at the keyboard owns no pane and never goes away.
    pub fn reclaim_absent(&mut self, live: &[String]) -> Vec<Task> {
        let mut released = Vec::new();
        for t in self.tasks.iter_mut() {
            if t.state == TaskState::Claimed
                && t.owner.as_ref().is_some_and(|o| o != "user" && !live.contains(o))
            {
                t.state = TaskState::Open;
                t.owner = None;
                t.claimed_at = None;
                released.push(t.clone());
            }
        }
        for t in &released {
            self.append(t);
        }
        released
    }

    /// Age a task, so staleness can be tested without waiting a day.
    #[cfg(test)]
    pub fn backdate_for_test(&mut self, id: u64, by_millis: u64) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.created = t.created.saturating_sub(by_millis);
        }
    }

    fn record(&mut self, task: Task) {
        self.append(&task);
        self.tasks.push(task);
        // Forget finished work first; anything open or claimed still matters.
        while self.tasks.len() > CAP {
            match self.tasks.iter().position(|t| !t.is_open() && t.state != TaskState::Claimed) {
                Some(i) => {
                    self.tasks.remove(i);
                }
                None => break,
            }
        }
    }

    fn append(&mut self, task: &Task) {
        if let Ok(line) = serde_json::to_string(task) {
            self.log.append_line(&line);
        }
        // Carry every task still in memory, not just the open ones: `task list --all` reads
        // finished work and its results, and losing that on rotation would be a surprise.
        if self.log.rotation_due() {
            let carry: Vec<String> =
                self.tasks.iter().filter_map(|t| serde_json::to_string(t).ok()).collect();
            self.log.rotate(&carry);
        }
    }
}

/// Replay the log. Later entries for an id supersede earlier ones, so the final state of each
/// task is whatever was written last.
fn read_log(path: &PathBuf) -> Vec<Task> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<Task> = Vec::new();
    for line in text.lines() {
        let Ok(t) = serde_json::from_str::<Task>(line) else { continue };
        match out.iter_mut().find(|x| x.id == t.id) {
            Some(slot) => *slot = t,
            None => out.push(t),
        }
    }
    out.sort_by_key(|t| t.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;


    /// The failure that made the board unusable: work is offered to one project only.
    #[test]
    fn a_task_is_only_ever_offered_to_its_own_project() {
        let mut b = board("scope");
        b.add("api work", "user", Some("api")).unwrap();
        b.add("docs work", "user", Some("docs")).unwrap();
        assert_eq!(b.offered_to("api"), 1);
        assert_eq!(b.offered_to("docs"), 1);
        assert_eq!(b.offered_to("something-else"), 0);
        // And an unnamed claim takes only its own project's.
        let got = b.claim("worker", None, Some("docs")).unwrap().unwrap();
        assert_eq!(got.text, "docs work");
    }

    /// An unscoped task predates scoping. It may be claimed by anyone, which is how it already
    /// behaved, but it is never *offered* — nothing should be interrupted for work that never
    /// said where it belongs.
    #[test]
    fn an_unscoped_task_is_claimable_but_never_offered() {
        let mut b = board("unscoped");
        b.add("from before", "user", None).unwrap();
        assert_eq!(b.offered_to("anywhere"), 0);
        assert!(b.claim("worker", None, Some("anywhere")).unwrap().is_some());
    }

    /// `None` means "every project" here and "no project named" to `open_to`. Sharing one
    /// predicate between them made `--everywhere` drop nothing at all.
    #[test]
    fn clearing_everywhere_really_does_clear_everywhere() {
        let mut b = board("clear-all");
        b.add("a", "user", Some("api")).unwrap();
        b.add("b", "user", Some("docs")).unwrap();
        b.add("c", "user", None).unwrap();
        assert_eq!(b.clear(None, false).len(), 3);
        assert_eq!(b.open_count(), 0);
    }

    #[test]
    fn clearing_one_project_leaves_the_others_alone() {
        let mut b = board("clear-one");
        b.add("a", "user", Some("api")).unwrap();
        b.add("b", "user", Some("docs")).unwrap();
        assert_eq!(b.clear(Some("api"), false).len(), 1);
        assert_eq!(b.offered_to("docs"), 1);
        assert_eq!(b.open_count(), 1);
    }

    /// Clearing is for open work. An agent is holding a claimed task right now, and dropping
    /// it out from under them is a different and louder intention.
    #[test]
    fn clearing_leaves_claimed_work_alone_unless_asked() {
        let mut b = board("clear-claimed");
        b.add("a", "user", Some("api")).unwrap();
        b.claim("worker", None, Some("api")).unwrap();
        assert_eq!(b.clear(Some("api"), false).len(), 0);
        assert_eq!(b.clear(Some("api"), true).len(), 1);
    }

    fn board(name: &str) -> Board {
        let p = std::env::temp_dir().join(format!("horde-tasks-{name}.jsonl"));
        let _ = std::fs::remove_file(&p);
        Board::new(p)
    }

    #[test]
    fn tasks_are_added_open_and_numbered() {
        let mut b = board("add");
        let a = b.add("write the tests", "user", None).unwrap();
        let c = b.add("review the diff", "builder", None).unwrap();
        assert_eq!((a.id, c.id), (1, 2));
        assert!(a.is_open());
        assert_eq!(c.by, "builder");
        assert_eq!(b.open_count(), 2);
        assert!(b.add("   ", "user", None).is_err(), "an empty task is not a task");
    }

    #[test]
    fn claiming_takes_the_oldest_open_task() {
        let mut b = board("fifo");
        b.add("first", "user", None).unwrap();
        b.add("second", "user", None).unwrap();
        let got = b.claim("worker", None, None).unwrap().unwrap();
        assert_eq!(got.text, "first");
        assert_eq!(got.owner.as_deref(), Some("worker"));
        assert_eq!(b.open_count(), 1);
        assert_eq!(b.claimed_count(), 1);
    }

    /// The one operation that must be exactly right: two agents cannot hold one task, or the
    /// work is silently done twice.
    #[test]
    fn a_task_cannot_be_claimed_twice() {
        let mut b = board("race");
        b.add("only one", "user", None).unwrap();
        let first = b.claim("a", Some(1), None).unwrap().unwrap();
        assert_eq!(first.owner.as_deref(), Some("a"));

        let err = b.claim("b", Some(1), None).unwrap_err().to_string();
        assert!(err.contains("already"), "{err}");
        assert!(err.contains("by a"), "the error should name the holder: {err}");

        // And a blind claim finds nothing rather than stealing it.
        assert!(b.claim("b", None, None).unwrap().is_none());
    }

    #[test]
    fn an_empty_board_reports_nothing_to_do_rather_than_an_error() {
        // An agent looping on the board has to tell "empty" from "broken".
        let mut b = board("empty");
        assert!(b.claim("worker", None, None).unwrap().is_none());
    }

    #[test]
    fn finishing_defaults_to_your_own_claimed_task() {
        let mut b = board("done");
        b.add("x", "user", None).unwrap();
        b.add("y", "user", None).unwrap();
        b.claim("worker", None, None).unwrap();
        let done = b.done("worker", None, Some("all green")).unwrap();
        assert_eq!(done.id, 1);
        assert_eq!(done.state, TaskState::Done);
        assert_eq!(done.result.as_deref(), Some("all green"));

        // With nothing claimed, it says so instead of guessing.
        let err = b.done("someone-else", None, None).unwrap_err().to_string();
        assert!(err.contains("no claimed task"), "{err}");
    }

    #[test]
    fn releasing_returns_a_task_to_the_board_and_dropping_retires_it() {
        let mut b = board("release");
        b.add("x", "user", None).unwrap();
        b.claim("worker", None, None).unwrap();
        let back = b.release(1, false).unwrap();
        assert!(back.is_open());
        assert!(back.owner.is_none());

        b.claim("worker", None, None).unwrap();
        let dropped = b.release(1, true).unwrap();
        assert_eq!(dropped.state, TaskState::Dropped);
        assert_eq!(b.open_count(), 0, "a dropped task is not back on the board");
    }

    /// A crashed agent must not take its work with it.
    #[test]
    fn tasks_held_by_a_departed_agent_return_to_the_board() {
        let mut b = board("reclaim");
        b.add("held", "user", None).unwrap();
        b.add("also held", "user", None).unwrap();
        b.claim("gone", Some(1), None).unwrap();
        b.claim("still-here", Some(2), None).unwrap();

        let released = b.reclaim_absent(&["still-here".to_string()]);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].id, 1);
        assert!(b.get(1).unwrap().is_open());
        assert_eq!(b.get(2).unwrap().state, TaskState::Claimed, "others are untouched");
    }

    /// The human at the keyboard owns no pane, so an empty roster must not strip their work.
    #[test]
    fn a_task_the_user_claimed_is_never_reclaimed() {
        let mut b = board("reclaim-user");
        b.add("mine", "user", None).unwrap();
        b.claim("user", Some(1), None).unwrap();
        assert!(b.reclaim_absent(&[]).is_empty());
        assert_eq!(b.get(1).unwrap().state, TaskState::Claimed);
    }

    #[test]
    fn the_board_survives_a_restart() {
        let p = std::env::temp_dir().join("horde-tasks-persist.jsonl");
        let _ = std::fs::remove_file(&p);
        {
            let mut b = Board::new(p.clone());
            b.add("first", "user", None).unwrap();
            b.add("second", "user", None).unwrap();
            b.claim("worker", None, None).unwrap();
            b.done("worker", None, Some("finished")).unwrap();
        }
        let b = Board::new(p.clone());
        assert_eq!(b.all().len(), 2);
        assert_eq!(b.get(1).unwrap().state, TaskState::Done);
        assert_eq!(b.get(1).unwrap().result.as_deref(), Some("finished"));
        assert_eq!(b.get(2).unwrap().state, TaskState::Open);
        // Ids do not restart, so a replayed log cannot collide with new work.
        let mut b = Board::new(p.clone());
        assert_eq!(b.add("third", "user", None).unwrap().id, 3);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_malformed_log_line_is_skipped_rather_than_fatal() {
        let p = std::env::temp_dir().join("horde-tasks-broken.jsonl");
        std::fs::write(&p, "not json\n{\"partial\":true}\n").unwrap();
        assert!(Board::new(p.clone()).all().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    /// Rotation must not cost the board its state.
    ///
    /// This is the trap in bounding a log that gets replayed: the ordinary "rename and start
    /// empty" would drop every open task on the floor. The live set is carried into the new
    /// file, so a restart after rotation rebuilds the same board.
    #[test]
    fn rotating_the_log_keeps_open_tasks_and_finished_results() {
        let p = std::env::temp_dir().join("horde-tasks-rotate.jsonl");
        let archive = std::env::temp_dir().join("horde-tasks-rotate.jsonl.1");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&archive);

        {
            let mut b = Board::new(p.clone());
            // A tiny limit, so the next size check rotates.
            b.log = crate::daemon::logfile::AppendLog::with_max(p.clone(), 1);
            b.add("still open", "user", None).unwrap();
            b.add("will finish", "user", None).unwrap();
            b.claim("worker", Some(2), None).unwrap();
            b.done("worker", Some(2), Some("all green")).unwrap();
            // Enough appends to trigger the periodic check.
            for i in 0..300 {
                b.add(&format!("filler {i}"), "user", None).unwrap();
            }
        }
        assert!(archive.exists(), "history should have been archived");

        // Reopen: the board must look exactly as it did.
        let b = Board::new(p.clone());
        let open = b.get(1).expect("the open task survived rotation");
        assert!(open.is_open());
        assert_eq!(open.text, "still open");
        let done = b.get(2).expect("the finished task survived rotation");
        assert_eq!(done.state, TaskState::Done);
        assert_eq!(done.result.as_deref(), Some("all green"), "its result too");
        // And ids do not restart, which would collide with the archived history.
        assert!(b.next_id > 300);

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&archive);
    }
}
