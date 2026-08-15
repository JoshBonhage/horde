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
    let mut fired = Vec::new();
    let mut turns = 0usize;
    for e in eng.journal.since(since) {
        match e.kind {
            Kind::Gone => gone.push(e.subject.clone()),
            Kind::Warned => warnings.push(e.subject.clone()),
            Kind::Fired => fired.push(e.subject.clone()),
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
                done: true,
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
        fired,
        tasks_done,
        tasks_added,
        tasks_open: eng.board.open_count(),
        tasks_claimed: eng.board.claimed_count(),
        messages,
        turns,
    }
}

/// A digest as a note.
///
/// The same content the overlay shows, written so it can be read a month later by someone who
/// was not there — which means naming the window rather than saying "since you last looked",
/// and linking the agents and tasks it mentions so the note joins the graph instead of sitting
/// in it alone.
///
/// Written as a section rather than a whole note, because several digests land on the same
/// dated note over a day and each is one entry in it.
pub fn markdown(d: &Digest) -> String {
    let mut out = String::new();
    out.push_str(&format!("## {}\n\n", super::triggers::local_clock(d.now)));

    if d.is_empty() {
        out.push_str("Nothing happened.\n");
        return out;
    }

    let window = (d.now.saturating_sub(d.since)) / 1000;
    out.push_str(&format!(
        "Covering {}{}.\n\n",
        secs_words(window),
        if d.fresh { ", since the daemon started" } else { "" }
    ));

    // Ordered by what a person reads first, same as the overlay: what wants you, then what
    // happened, then what is still going.
    let agents = |title: &str, list: &[AgentLine], out: &mut String| {
        if list.is_empty() {
            return;
        }
        out.push_str(&format!("**{title}**\n\n"));
        for a in list {
            let what = a.activity.as_deref().unwrap_or(&a.reason);
            let what = if what.trim().is_empty() { String::new() } else { format!(" — {what}") };
            out.push_str(&format!("- [[{}]]{what}\n", a.name));
        }
        out.push('\n');
    };
    agents("Needs you", &d.needs_you, &mut out);
    agents("Finished", &d.finished, &mut out);
    agents("Working", &d.working, &mut out);

    if !d.tasks_done.is_empty() {
        out.push_str("**Tasks**\n\n");
        for t in &d.tasks_done {
            let verb = if t.dropped { "dropped" } else { "done" };
            let who = t.owner.as_deref().map(|o| format!(" by [[{o}]]")).unwrap_or_default();
            out.push_str(&format!("- {verb}: {}{who}\n", t.text));
        }
        out.push('\n');
    }

    if !d.fired.is_empty() {
        out.push_str("**Triggers fired**\n\n");
        for f in &d.fired {
            out.push_str(&format!("- {f}\n"));
        }
        out.push('\n');
    }

    if !d.warnings.is_empty() {
        out.push_str("**Warnings**\n\n");
        for w in &d.warnings {
            out.push_str(&format!("- {w}\n"));
        }
        out.push('\n');
    }

    if !d.gone.is_empty() {
        out.push_str(&format!("**Exited:** {}\n\n", d.gone.join(", ")));
    }

    // Both forms carried rather than an `s` appended, because "1 tasks added" is the kind of
    // thing that makes a generated note read as generated.
    let counts = [
        (d.turns, "turn", "turns"),
        (d.tasks_added, "task added", "tasks added"),
        (d.tasks_open, "open", "open"),
        (d.messages.len(), "message", "messages"),
    ];
    let counts: Vec<String> = counts
        .iter()
        .filter(|(n, _, _)| *n > 0)
        .map(|(n, one, many)| format!("{n} {}", if *n == 1 { one } else { many }))
        .collect();
    if !counts.is_empty() {
        out.push_str(&format!("{}\n", counts.join(" · ")));
    }
    out
}

/// `30m`, `2h` — how long a window was, in the words a person would use for it.
fn secs_words(secs: u64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3600 => format!("{}h", s / 3600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, state: AgentState, activity: Option<&str>) -> AgentLine {
        AgentLine {
            name: name.into(),
            state,
            elapsed: 90,
            activity: activity.map(String::from),
            reason: "rule".into(),
        }
    }

    fn empty() -> Digest {
        Digest {
            since: 1_000_000,
            now: 1_000_000 + 3_600_000,
            fresh: false,
            needs_you: Vec::new(),
            finished: Vec::new(),
            working: Vec::new(),
            gone: Vec::new(),
            warnings: Vec::new(),
            fired: Vec::new(),
            tasks_done: Vec::new(),
            tasks_added: 0,
            tasks_open: 0,
            tasks_claimed: 0,
            messages: Vec::new(),
            turns: 0,
        }
    }

    /// A digest in the vault has to be readable a month later by somebody who was not there.
    /// That means naming the window rather than saying "since you last looked", and linking
    /// what it mentions so the note joins the graph instead of sitting in it alone.
    #[test]
    fn a_digest_note_names_its_window_and_links_what_it_mentions() {
        let mut d = empty();
        d.needs_you = vec![line("reviewer", AgentState::Blocked, Some("waiting on approval"))];
        d.finished = vec![line("builder", AgentState::Done, None)];
        d.turns = 4;

        let md = markdown(&d);
        assert!(md.contains("Covering 1h"), "the window, in words: {md}");
        assert!(md.contains("[[reviewer]]"), "agents are links: {md}");
        assert!(md.contains("[[builder]]"));
        assert!(md.contains("waiting on approval"), "and what it was doing");
        assert!(md.contains("4 turns"), "{md}");

        d.turns = 1;
        d.tasks_added = 1;
        assert!(markdown(&d).contains("1 turn ·"), "and one of a thing is not one things");
        assert!(markdown(&d).contains("1 task added"));
        assert!(md.starts_with("## "), "a section, because a day holds several");
    }

    /// Nothing happening is a fact worth filing. A dated note with gaps in it reads as a
    /// daemon that stopped rather than a day that was quiet.
    #[test]
    fn a_quiet_window_still_says_so() {
        let md = markdown(&empty());
        assert!(md.contains("Nothing happened"), "{md}");
        assert!(md.starts_with("## "));
    }

    #[test]
    fn the_window_is_described_in_the_units_a_person_would_use() {
        assert_eq!(secs_words(45), "45s");
        assert_eq!(secs_words(90), "1m");
        assert_eq!(secs_words(7200), "2h");
        assert_eq!(secs_words(180_000), "2d");
    }
}
