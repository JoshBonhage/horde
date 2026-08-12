//! "What happened while I was away."
//!
//! Detaching from a multiplexer full of agents means coming back to five panes of scrollback
//! and no idea which of them matters. Every other tool leaves you to scroll. horde already
//! has the records — routed messages, the task board, agent state history — so it can just
//! tell you.
//!
//! The digest is assembled from three logs that each own their own facts, and it answers in
//! the order you care about: what needs you now, then what got finished, then what was said.

use super::journal::Kind;
use super::tasks::TaskState;
use super::Engine;
use crate::proto::{AgentLine, AgentState, Digest, Message, TaskLine};

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
