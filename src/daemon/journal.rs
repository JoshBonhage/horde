//! A record of the things you would want to know you missed.
//!
//! The bus log and the task board already keep their own history, so this journal
//! deliberately holds only what nothing else records: agents changing state, panes going
//! away, and warnings. Each fact has exactly one home, which is what keeps the digest from
//! reporting the same event twice in two different sections.
//!
//! Entries are appended as jsonl so the record survives a daemon restart — the point of a
//! digest is telling you about the hour you were not watching, and a restart is exactly the
//! kind of thing that happens during that hour.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::proto::{AgentState, Event, NoticeLevel, PaneId};

/// Entries kept in memory. A digest only ever looks at a recent window, and the file keeps
/// the rest.
const CAP: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// An agent started a turn.
    Started,
    /// An agent stopped and wants a human — a permission prompt, usually.
    Blocked,
    /// An agent finished a turn while you were not looking at it.
    Finished,
    /// A pane exited.
    Gone,
    /// Something went wrong that a human should know about.
    Warned,
    /// horde reached out to you — a system notification, or your own notify command.
    ///
    /// The only record that an alert was ever sent. Everything else here is something horde
    /// observed; this is something horde *did*, which is why it is worth keeping even though no
    /// digest section reads it yet: "was I told, and when" has to be answerable after the fact.
    Notified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Unix millis.
    pub ts: u64,
    pub kind: Kind,
    /// Agent or pane name, so the entry still reads sensibly after the pane is gone.
    pub subject: String,
    pub pane: Option<PaneId>,
}

pub struct Journal {
    log: super::logfile::AppendLog,
    entries: Vec<Entry>,
}

impl Journal {
    pub fn new(path: PathBuf) -> Journal {
        let mut entries = read_log(&path);
        // Trust the file's order but not its length.
        if entries.len() > CAP {
            entries.drain(..entries.len() - CAP);
        }
        Journal { log: super::logfile::AppendLog::new(path), entries }
    }

    pub fn since(&self, ts: u64) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.ts >= ts)
    }

    /// Translate an outgoing event into a journal entry, if it is one worth keeping.
    ///
    /// Returns quietly for everything else: bus messages belong to the bus log and task
    /// notices to the board, so recording them here would double-count them in a digest.
    pub fn record(&mut self, ev: &Event, name_of: impl Fn(PaneId) -> String) {
        let entry = match ev {
            Event::AgentStateChanged { pane, name, to, .. } => {
                let kind = match to {
                    AgentState::Blocked => Kind::Blocked,
                    AgentState::Done => Kind::Finished,
                    AgentState::Working => Kind::Started,
                    // Idle and unknown are the absence of news.
                    _ => return,
                };
                Entry { ts: super::now_millis(), kind, subject: name.clone(), pane: Some(*pane) }
            }
            Event::PaneExited { pane, .. } => Entry {
                ts: super::now_millis(),
                kind: Kind::Gone,
                subject: name_of(*pane),
                pane: Some(*pane),
            },
            // An info notice is chatter — the interesting ones are the failures.
            Event::Notice { level, text } if *level != NoticeLevel::Info => Entry {
                ts: super::now_millis(),
                kind: Kind::Warned,
                subject: text.clone(),
                pane: None,
            },
            _ => return,
        };
        self.append(&entry);
        self.entries.push(entry);
        while self.entries.len() > CAP {
            self.entries.remove(0);
        }
    }

    /// Record something horde did, rather than something it saw.
    ///
    /// `record` translates outgoing events, which covers everything horde observes. An alert is
    /// not an event — it never reaches a client, and while one is being sent there may be no
    /// client to reach — so it needs a way in of its own.
    pub fn note(&mut self, kind: Kind, subject: impl Into<String>) {
        let entry = Entry { ts: super::now_millis(), kind, subject: subject.into(), pane: None };
        self.append(&entry);
        self.entries.push(entry);
        while self.entries.len() > CAP {
            self.entries.remove(0);
        }
    }

    fn append(&mut self, e: &Entry) {
        if let Ok(line) = serde_json::to_string(e) {
            self.log.append_line(&line);
        }
        if self.log.rotation_due() {
            let carry: Vec<String> =
                self.entries.iter().filter_map(|x| serde_json::to_string(x).ok()).collect();
            self.log.rotate(&carry);
        }
    }
}

/// Read the log back, skipping anything unparseable rather than failing.
///
/// A journal is a convenience, never a correctness requirement, so a corrupt line costs one
/// entry and nothing more.
fn read_log(path: &PathBuf) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter_map(|l| serde_json::from_str::<Entry>(l).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(name: &str) -> Journal {
        let p = std::env::temp_dir().join(format!("horde-journal-{name}.jsonl"));
        let _ = std::fs::remove_file(&p);
        Journal::new(p)
    }

    fn state_change(name: &str, to: AgentState) -> Event {
        Event::AgentStateChanged {
            pane: 1,
            name: name.to_string(),
            from: AgentState::Idle,
            to,
        }
    }

    #[test]
    fn agent_states_worth_reporting_are_kept() {
        let mut j = journal("states");
        j.record(&state_change("builder", AgentState::Blocked), |_| "x".into());
        j.record(&state_change("builder", AgentState::Done), |_| "x".into());
        j.record(&state_change("builder", AgentState::Working), |_| "x".into());
        let kinds: Vec<Kind> = j.since(0).map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::Blocked, Kind::Finished, Kind::Started]);
    }

    #[test]
    fn idle_is_not_news() {
        let mut j = journal("idle");
        j.record(&state_change("builder", AgentState::Idle), |_| "x".into());
        j.record(&state_change("builder", AgentState::Unknown), |_| "x".into());
        assert_eq!(j.since(0).count(), 0);
    }

    /// The bus and the board keep their own logs. Recording them here too would make a
    /// digest report every message twice.
    #[test]
    fn bus_traffic_and_info_notices_are_left_to_their_own_logs() {
        let mut j = journal("skip");
        j.record(
            &Event::Notice { level: NoticeLevel::Info, text: "task #1 is open again".into() },
            |_| "x".into(),
        );
        assert_eq!(j.since(0).count(), 0);
        j.record(
            &Event::Notice { level: NoticeLevel::Error, text: "pty write failed".into() },
            |_| "x".into(),
        );
        assert_eq!(j.since(0).count(), 1);
    }

    #[test]
    fn an_exited_pane_is_named_while_the_name_is_still_available() {
        let mut j = journal("gone");
        j.record(&Event::PaneExited { pane: 7, status: 0 }, |p| format!("worker{p}"));
        let e = j.since(0).next().unwrap();
        assert_eq!(e.kind, Kind::Gone);
        assert_eq!(e.subject, "worker7", "the name must be resolved before the pane is dropped");
    }

    #[test]
    fn since_excludes_older_entries() {
        let mut j = journal("since");
        j.record(&state_change("a", AgentState::Blocked), |_| "x".into());
        let cut = j.since(0).next().unwrap().ts + 1;
        assert_eq!(j.since(cut).count(), 0);
    }

    #[test]
    fn the_journal_survives_a_restart() {
        let p = std::env::temp_dir().join("horde-journal-restart.jsonl");
        let _ = std::fs::remove_file(&p);
        {
            let mut j = Journal::new(p.clone());
            j.record(&state_change("builder", AgentState::Blocked), |_| "x".into());
        }
        let j = Journal::new(p.clone());
        assert_eq!(j.since(0).count(), 1, "a digest has to survive the restart it reports on");
        let _ = std::fs::remove_file(&p);
    }
}
