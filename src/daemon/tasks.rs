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

use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Tasks kept in memory. Beyond this the oldest done ones are forgotten.
const CAP: usize = 2000;

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
}

pub struct Board {
    path: PathBuf,
    tasks: Vec<Task>,
    next_id: u64,
}

impl Board {
    pub fn new(path: PathBuf) -> Board {
        let tasks = read_log(&path);
        let next_id = tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        Board { path, tasks, next_id }
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

    pub fn add(&mut self, text: &str, by: &str) -> Result<Task> {
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
            owner: None,
            claimed_at: None,
            done_at: None,
            result: None,
        };
        self.next_id += 1;
        self.record(task.clone());
        Ok(task)
    }

    /// Take the oldest open task, or a specific one.
    ///
    /// Returning `None` for "nothing to do" rather than an error is deliberate: an agent
    /// looping on the board should be able to tell "empty" from "broken".
    pub fn claim(&mut self, owner: &str, id: Option<u64>) -> Result<Option<Task>> {
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
            None => match self.tasks.iter().position(|t| t.is_open()) {
                Some(i) => i,
                None => return Ok(None),
            },
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
        if let Some(p) = self.path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            if let Ok(line) = serde_json::to_string(task) {
                let _ = writeln!(f, "{line}");
            }
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

    fn board(name: &str) -> Board {
        let p = std::env::temp_dir().join(format!("horde-tasks-{name}.jsonl"));
        let _ = std::fs::remove_file(&p);
        Board::new(p)
    }

    #[test]
    fn tasks_are_added_open_and_numbered() {
        let mut b = board("add");
        let a = b.add("write the tests", "user").unwrap();
        let c = b.add("review the diff", "builder").unwrap();
        assert_eq!((a.id, c.id), (1, 2));
        assert!(a.is_open());
        assert_eq!(c.by, "builder");
        assert_eq!(b.open_count(), 2);
        assert!(b.add("   ", "user").is_err(), "an empty task is not a task");
    }

    #[test]
    fn claiming_takes_the_oldest_open_task() {
        let mut b = board("fifo");
        b.add("first", "user").unwrap();
        b.add("second", "user").unwrap();
        let got = b.claim("worker", None).unwrap().unwrap();
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
        b.add("only one", "user").unwrap();
        let first = b.claim("a", Some(1)).unwrap().unwrap();
        assert_eq!(first.owner.as_deref(), Some("a"));

        let err = b.claim("b", Some(1)).unwrap_err().to_string();
        assert!(err.contains("already"), "{err}");
        assert!(err.contains("by a"), "the error should name the holder: {err}");

        // And a blind claim finds nothing rather than stealing it.
        assert!(b.claim("b", None).unwrap().is_none());
    }

    #[test]
    fn an_empty_board_reports_nothing_to_do_rather_than_an_error() {
        // An agent looping on the board has to tell "empty" from "broken".
        let mut b = board("empty");
        assert!(b.claim("worker", None).unwrap().is_none());
    }

    #[test]
    fn finishing_defaults_to_your_own_claimed_task() {
        let mut b = board("done");
        b.add("x", "user").unwrap();
        b.add("y", "user").unwrap();
        b.claim("worker", None).unwrap();
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
        b.add("x", "user").unwrap();
        b.claim("worker", None).unwrap();
        let back = b.release(1, false).unwrap();
        assert!(back.is_open());
        assert!(back.owner.is_none());

        b.claim("worker", None).unwrap();
        let dropped = b.release(1, true).unwrap();
        assert_eq!(dropped.state, TaskState::Dropped);
        assert_eq!(b.open_count(), 0, "a dropped task is not back on the board");
    }

    /// A crashed agent must not take its work with it.
    #[test]
    fn tasks_held_by_a_departed_agent_return_to_the_board() {
        let mut b = board("reclaim");
        b.add("held", "user").unwrap();
        b.add("also held", "user").unwrap();
        b.claim("gone", Some(1)).unwrap();
        b.claim("still-here", Some(2)).unwrap();

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
        b.add("mine", "user").unwrap();
        b.claim("user", Some(1)).unwrap();
        assert!(b.reclaim_absent(&[]).is_empty());
        assert_eq!(b.get(1).unwrap().state, TaskState::Claimed);
    }

    #[test]
    fn the_board_survives_a_restart() {
        let p = std::env::temp_dir().join("horde-tasks-persist.jsonl");
        let _ = std::fs::remove_file(&p);
        {
            let mut b = Board::new(p.clone());
            b.add("first", "user").unwrap();
            b.add("second", "user").unwrap();
            b.claim("worker", None).unwrap();
            b.done("worker", None, Some("finished")).unwrap();
        }
        let b = Board::new(p.clone());
        assert_eq!(b.all().len(), 2);
        assert_eq!(b.get(1).unwrap().state, TaskState::Done);
        assert_eq!(b.get(1).unwrap().result.as_deref(), Some("finished"));
        assert_eq!(b.get(2).unwrap().state, TaskState::Open);
        // Ids do not restart, so a replayed log cannot collide with new work.
        let mut b = Board::new(p.clone());
        assert_eq!(b.add("third", "user").unwrap().id, 3);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_malformed_log_line_is_skipped_rather_than_fatal() {
        let p = std::env::temp_dir().join("horde-tasks-broken.jsonl");
        std::fs::write(&p, "not json\n{\"partial\":true}\n").unwrap();
        assert!(Board::new(p.clone()).all().is_empty());
        let _ = std::fs::remove_file(&p);
    }
}
