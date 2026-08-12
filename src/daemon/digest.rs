//! "What happened while I was away."
//!
//! Detaching from a multiplexer full of agents means coming back to five panes of scrollback
//! and no idea which of them matters. Every other tool leaves you to scroll. horde already
//! has the records — routed messages, the task board, agent state history — so it can just
//! tell you.
//!
//! The digest is assembled from three logs that each own their own facts, and it answers in
//! the order you care about: what needs you now, then what got finished, then what was said.

use serde::{Deserialize, Serialize};

use super::journal::Kind;
use super::tasks::TaskState;
use super::Engine;
use crate::proto::{AgentState, Message};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Digest {
    /// Window start, unix millis.
    pub since: u64,
    pub now: u64,
    /// True when the window is "since the daemon started" because no digest has been read
    /// yet. Without this a fresh session reads as "nothing new since you last looked", which
    /// claims a read that never happened.
    pub fresh: bool,
    /// Agents wanting a human right now, most urgent thing in the whole digest.
    pub needs_you: Vec<AgentLine>,
    /// Agents that finished a turn while you were not looking.
    pub finished: Vec<AgentLine>,
    /// Still going.
    pub working: Vec<AgentLine>,
    /// Panes that exited during the window.
    pub gone: Vec<String>,
    /// Warnings the daemon raised while nobody was watching them.
    pub warnings: Vec<String>,
    pub tasks_done: Vec<TaskLine>,
    pub tasks_added: usize,
    pub tasks_open: usize,
    pub tasks_claimed: usize,
    /// Messages routed in the window, newest last.
    pub messages: Vec<Message>,
    /// Turns any agent completed in the window, counted from the journal so it still counts
    /// the work of an agent that has since exited.
    pub turns: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLine {
    pub name: String,
    pub state: AgentState,
    /// Seconds in the current state.
    pub elapsed: u64,
    /// Whatever the hooks recorded for the current turn, when installed.
    pub activity: Option<String>,
    /// Matched rule or hook reason, so a surprising line can be explained.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLine {
    pub id: u64,
    pub text: String,
    pub owner: Option<String>,
    pub result: Option<String>,
    /// True for a task that was dropped rather than completed.
    pub dropped: bool,
}

impl Digest {
    /// True when there is genuinely nothing to report, so callers can stay quiet instead of
    /// printing an empty report.
    pub fn is_empty(&self) -> bool {
        self.needs_you.is_empty()
            && self.finished.is_empty()
            && self.gone.is_empty()
            && self.warnings.is_empty()
            && self.tasks_done.is_empty()
            && self.messages.is_empty()
            && self.tasks_added == 0
    }

    /// One line for a toast or status bar, or None when there is nothing worth saying.
    ///
    /// Ordered by what would make you act: a blocked agent first, then finished work, then
    /// traffic. Only the first two facts are shown — a toast nobody can read at a glance is
    /// no better than no toast.
    pub fn headline(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        match self.needs_you.len() {
            0 => {}
            1 => parts.push("1 agent needs you".into()),
            n => parts.push(format!("{n} agents need you")),
        }
        if !self.finished.is_empty() {
            parts.push(format!("{} finished", self.finished.len()));
        }
        if !self.tasks_done.is_empty() {
            parts.push(format!("{} done", plural(self.tasks_done.len(), "task")));
        }
        if !self.messages.is_empty() {
            parts.push(plural(self.messages.len(), "message"));
        }
        if !self.gone.is_empty() {
            parts.push(format!("{} exited", self.gone.len()));
        }
        if parts.is_empty() {
            return None;
        }
        parts.truncate(2);
        Some(format!("while you were away: {}", parts.join(", ")))
    }
}

/// `1 task` / `3 tasks`. A toast that says "1 messages" reads as a bug in the tool.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Build the digest for everything since `since`.
pub fn build(eng: &Engine, since: u64) -> Digest {
    let fresh = eng.last_seen == 0;
    let now = super::now_millis();

    let mut needs_you = Vec::new();
    let mut finished = Vec::new();
    let mut working = Vec::new();
    for p in eng.session.panes.values() {
        let Some(a) = &p.agent else { continue };
        let line = AgentLine {
            name: a.name.clone(),
            state: a.state,
            elapsed: a.since.elapsed().as_secs(),
            activity: a.activity.summary(),
            reason: a.reason.clone(),
        };
        match a.state {
            AgentState::Blocked => needs_you.push(line),
            AgentState::Done => finished.push(line),
            AgentState::Working => working.push(line),
            _ => {}
        }
    }
    // Longest-waiting first: that is the one that has been stuck the whole time.
    needs_you.sort_by(|a, b| b.elapsed.cmp(&a.elapsed));
    finished.sort_by(|a, b| b.elapsed.cmp(&a.elapsed));
    working.sort_by(|a, b| b.elapsed.cmp(&a.elapsed));

    let mut gone = Vec::new();
    let mut warnings = Vec::new();
    let mut turns = 0usize;
    for e in eng.journal.since(since) {
        match e.kind {
            Kind::Gone => gone.push(e.subject.clone()),
            Kind::Warned => warnings.push(e.subject.clone()),
            // A finished turn is the unit of agent work, and counting it from the journal
            // keeps the total honest when the agent itself is no longer around.
            Kind::Finished => turns += 1,
            _ => {}
        }
    }
    gone.dedup();

    let mut tasks_done = Vec::new();
    let mut tasks_added = 0usize;
    for t in eng.board.all() {
        if t.created >= since {
            tasks_added += 1;
        }
        let closed = t.state == TaskState::Done || t.state == TaskState::Dropped;
        if closed && t.done_at.unwrap_or(0) >= since {
            tasks_done.push(TaskLine {
                id: t.id,
                text: t.text.clone(),
                owner: t.owner.clone(),
                result: t.result.clone(),
                dropped: t.state == TaskState::Dropped,
            });
        }
    }

    let messages: Vec<Message> =
        eng.bus.recent(200).into_iter().filter(|m| m.ts >= since).collect();

    Digest {
        since,
        now,
        fresh,
        needs_you,
        finished,
        working,
        gone,
        warnings,
        tasks_done,
        tasks_added,
        tasks_open: eng.board.open_count(),
        tasks_claimed: eng.board.claimed_count(),
        messages,
        turns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, state: AgentState, elapsed: u64) -> AgentLine {
        AgentLine {
            name: name.into(),
            state,
            elapsed,
            activity: None,
            reason: "test".into(),
        }
    }

    fn empty() -> Digest {
        Digest {
            since: 0,
            now: 1000,
            fresh: false,
            needs_you: vec![],
            finished: vec![],
            working: vec![],
            gone: vec![],
            warnings: vec![],
            tasks_done: vec![],
            tasks_added: 0,
            tasks_open: 0,
            tasks_claimed: 0,
            messages: vec![],
            turns: 0,
        }
    }

    #[test]
    fn nothing_to_report_is_reported_as_nothing() {
        assert!(empty().is_empty());
        assert_eq!(empty().headline(), None);
    }

    /// An agent still working is not news you missed — it is the current state, visible in
    /// the sidebar. Only the digest's own findings make it non-empty.
    #[test]
    fn a_working_agent_alone_does_not_make_a_digest() {
        let mut d = empty();
        d.working.push(line("builder", AgentState::Working, 30));
        assert!(d.is_empty());
    }

    #[test]
    fn headline_leads_with_what_needs_a_human() {
        let mut d = empty();
        d.messages = vec![];
        d.tasks_done.push(TaskLine {
            id: 1,
            text: "t".into(),
            owner: None,
            result: None,
            dropped: false,
        });
        d.needs_you.push(line("reviewer", AgentState::Blocked, 90));
        let h = d.headline().unwrap();
        assert!(h.starts_with("while you were away: 1 agent needs you"), "{h}");
    }

    #[test]
    fn a_single_item_is_not_pluralised() {
        let mut d = empty();
        d.tasks_done.push(TaskLine {
            id: 1,
            text: "t".into(),
            owner: None,
            result: None,
            dropped: false,
        });
        assert_eq!(d.headline().unwrap(), "while you were away: 1 task done");
        d.needs_you.push(line("a", AgentState::Blocked, 1));
        let h = d.headline().unwrap();
        assert!(h.contains("1 agent needs you"), "{h}");
    }

    #[test]
    fn headline_shows_two_facts_not_five() {
        let mut d = empty();
        d.needs_you.push(line("a", AgentState::Blocked, 1));
        d.finished.push(line("b", AgentState::Done, 1));
        d.tasks_done.push(TaskLine {
            id: 1,
            text: "t".into(),
            owner: None,
            result: None,
            dropped: false,
        });
        d.gone.push("c".into());
        let h = d.headline().unwrap();
        assert_eq!(h.matches(',').count(), 1, "too much for a toast: {h}");
    }
}
