//! The message bus: agents talking to each other.
//!
//! Delivery is pane injection — horde writes into the target agent's PTY. What makes this
//! more than "type at each other blindly" is that the daemon *routes and records* every
//! message, so there is an addressable name space, a durable log, and a visible queue.
//!
//! # Why injection is gated on state
//!
//! Writing into a pane mid-stream races whatever the agent is emitting. Worse, an agent
//! sitting at a permission prompt is waiting on a *decision*: text plus a newline would
//! answer that prompt, potentially approving something nobody agreed to. So:
//!
//! | Target state | Behaviour |
//! |---|---|
//! | `idle`, `done` | inject and submit — the agent is at its prompt |
//! | `blocked` | **queue**; injecting would answer the pending question |
//! | `working` | queue; flush when it goes idle |
//! | `unknown` | queue; be conservative when we cannot tell |
//!
//! A pane with no agent at all gets the text without a submitting newline, so a stray
//! message can never execute as a shell command.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::proto::{AgentState, Delivery, Event, Message, PaneId, SpaceId};

use super::state::Session;

/// Messages kept in memory for the bus drawer.
const RING: usize = 500;

pub struct Bus {
    log_path: PathBuf,
    messages: VecDeque<Message>,
    next_id: u64,
}

/// Whether a message can be written into a pane right now.
enum Gate {
    /// Agent is at its prompt; send the body and submit it.
    Submit,
    /// No agent in the pane; write the text but never a newline.
    TextOnly,
    /// Hold it, with the reason shown in the drawer.
    Hold(&'static str),
}

impl Bus {
    pub fn new(log_path: PathBuf) -> Bus {
        let messages = read_tail(&log_path, RING);
        let next_id = messages.iter().map(|m| m.id).max().unwrap_or(0) + 1;
        Bus { log_path, messages, next_id }
    }

    pub fn recent(&self, n: usize) -> Vec<Message> {
        let skip = self.messages.len().saturating_sub(n);
        self.messages.iter().skip(skip).cloned().collect()
    }

    /// Resolve a target string to a pane.
    ///
    /// Accepts an agent name (`reviewer`), a pane name, a bare pane id (`7`), or
    /// `space:pane` / `space:tab:pane` coordinates.
    pub fn resolve(session: &Session, target: &str) -> Option<PaneId> {
        let t = target.trim().trim_start_matches('@');

        // Agent name is the common case and takes precedence.
        for p in session.panes.values() {
            if p.agent.as_ref().is_some_and(|a| a.name == t) {
                return Some(p.id);
            }
        }
        for p in session.panes.values() {
            if p.name.as_deref() == Some(t) {
                return Some(p.id);
            }
        }
        if let Ok(id) = t.parse::<PaneId>() {
            if session.panes.contains_key(&id) {
                return Some(id);
            }
        }
        // `space:tab:pane` — the last component is a pane index within the tab.
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() >= 2 {
            let space = session.find_space_by_name(parts[0])?;
            let s = session.space(space)?;
            let (tab, idx) = if parts.len() == 2 {
                (s.focused_tab?, parts[1])
            } else {
                let tab_name = parts[1];
                let tab = s
                    .tabs
                    .iter()
                    .find(|&&t| session.tab(t).is_some_and(|t| t.name == tab_name))
                    .copied()?;
                (tab, parts[2])
            };
            let panes = session.tab(tab)?.layout.panes();
            if let Ok(n) = idx.parse::<usize>() {
                return panes.get(n.saturating_sub(1)).copied();
            }
        }
        None
    }

    /// Human-facing name for whoever a message is from.
    pub fn sender_name(session: &Session, from_pane: Option<PaneId>) -> String {
        match from_pane.and_then(|p| session.panes.get(&p)) {
            Some(p) => p
                .agent
                .as_ref()
                .map(|a| a.name.clone())
                .or_else(|| p.name.clone())
                .unwrap_or_else(|| format!("pane{}", p.id)),
            // Anything not originating in a pane is the human at the keyboard.
            None => "user".to_string(),
        }
    }

    fn gate(session: &Session, pane: PaneId, force: bool) -> Gate {
        let Some(p) = session.panes.get(&pane) else { return Gate::Hold("pane is gone") };
        let Some(agent) = p.agent.as_ref() else { return Gate::TextOnly };
        if force {
            return Gate::Submit;
        }
        match agent.state {
            AgentState::Idle | AgentState::Done => Gate::Submit,
            // Never type at a pending question — the newline would answer it.
            AgentState::Blocked => Gate::Hold("target is blocked on a prompt"),
            AgentState::Working => Gate::Hold("target is working"),
            AgentState::Unknown => Gate::Hold("target state is unknown"),
        }
    }

    /// Route one message. Delivers now if the target is at its prompt, else queues it.
    pub fn send(
        &mut self,
        session: &mut Session,
        cfg: &Config,
        from_pane: Option<PaneId>,
        to: &str,
        body: &str,
        force: bool,
    ) -> Result<Message> {
        let target = Self::resolve(session, to)
            .ok_or_else(|| anyhow!("no agent or pane called {to:?} (try `horde roster`)"))?;
        if Some(target) == from_pane {
            return Err(anyhow!("refusing to send a message to yourself"));
        }
        let from = Self::sender_name(session, from_pane);

        let mut msg = Message {
            id: self.next_id,
            ts: super::now_millis(),
            from,
            to: Self::sender_name(session, Some(target)),
            body: body.to_string(),
            delivery: Delivery::Queued,
            broadcast: false,
        };
        self.next_id += 1;

        let force = force || cfg.force_inject;
        match Self::gate(session, target, force) {
            Gate::Submit => self.deliver(session, target, &mut msg, true),
            Gate::TextOnly => self.deliver(session, target, &mut msg, false),
            Gate::Hold(reason) => {
                super::log_line(&format!(
                    "bus: holding message {} for {} ({reason})",
                    msg.id, msg.to
                ));
                match session.panes.get_mut(&target).and_then(|p| p.agent.as_mut()) {
                    Some(agent) => {
                        msg.delivery = Delivery::Queued;
                        agent.queued.push(msg.clone());
                    }
                    // Nowhere to park it — the pane vanished between resolve and gate.
                    None => msg.delivery = Delivery::Dropped,
                }
            }
        }

        self.record(msg.clone());
        Ok(msg)
    }

    /// Write a message into a pane. `submit` appends a carriage return.
    fn deliver(&self, session: &mut Session, pane: PaneId, msg: &mut Message, submit: bool) {
        let text = format!("[horde] message from {}: {}", msg.from, msg.body.trim());
        let mut bytes = text.into_bytes();
        if submit {
            // Carriage return, not newline: that is what a terminal sends for Enter.
            bytes.push(b'\r');
        }
        match session.panes.get_mut(&pane) {
            Some(p) => {
                msg.delivery = match p.write(&bytes) {
                    Ok(()) => Delivery::Delivered,
                    Err(_) => Delivery::Dropped,
                };
            }
            None => msg.delivery = Delivery::Dropped,
        }
    }

    /// Send to every agent in a space, or across the whole session when `space` is None.
    pub fn broadcast(
        &mut self,
        session: &mut Session,
        cfg: &Config,
        from_pane: Option<PaneId>,
        space: Option<SpaceId>,
        body: &str,
    ) -> Vec<Message> {
        let targets: Vec<PaneId> = session
            .panes
            .values()
            .filter(|p| Some(p.id) != from_pane)
            .filter(|p| p.agent.is_some())
            .filter(|p| space.is_none_or(|s| p.space == s))
            .map(|p| p.id)
            .collect();

        let mut out = Vec::new();
        for t in targets {
            let name = Self::sender_name(session, Some(t));
            match self.send(session, cfg, from_pane, &name, body, false) {
                Ok(mut m) => {
                    m.broadcast = true;
                    out.push(m);
                }
                Err(_) => continue,
            }
        }
        out
    }

    /// Deliver anything held for agents that have since reached their prompt.
    pub fn flush_queued(&mut self, session: &mut Session, cfg: &Config) -> Vec<Event> {
        let candidates: Vec<PaneId> = session
            .panes
            .values()
            .filter(|p| p.agent.as_ref().is_some_and(|a| !a.queued.is_empty()))
            .map(|p| p.id)
            .collect();

        let mut events = Vec::new();
        for pane in candidates {
            if !matches!(Self::gate(session, pane, cfg.force_inject), Gate::Submit) {
                continue;
            }
            // Take the whole queue: delivering one at a time would race the state change
            // back to `working` and strand the rest.
            let queued: Vec<Message> = session
                .panes
                .get_mut(&pane)
                .and_then(|p| p.agent.as_mut())
                .map(|a| std::mem::take(&mut a.queued))
                .unwrap_or_default();

            for mut msg in queued {
                self.deliver(session, pane, &mut msg, true);
                self.update_delivery(msg.id, msg.delivery);
                events.push(Event::BusMessage(msg));
            }
        }
        events
    }

    /// Mark a previously queued message as delivered, in the ring and in the log.
    fn update_delivery(&mut self, id: u64, delivery: Delivery) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
            m.delivery = delivery;
            let updated = m.clone();
            // The log is append-only, so a delivery change is recorded as a new line and
            // the latest entry for an id wins on replay.
            self.append_log(&updated);
        }
    }

    fn record(&mut self, msg: Message) {
        self.append_log(&msg);
        self.messages.push_back(msg);
        while self.messages.len() > RING {
            self.messages.pop_front();
        }
    }

    fn append_log(&self, msg: &Message) {
        if let Some(p) = self.log_path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.log_path)
        {
            if let Ok(line) = serde_json::to_string(msg) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// Replay the tail of the log. Later entries for the same id supersede earlier ones, which
/// is how a queued-then-delivered message reads back correctly.
fn read_tail(path: &PathBuf, n: usize) -> VecDeque<Message> {
    let Ok(text) = std::fs::read_to_string(path) else { return VecDeque::new() };
    let mut by_id: Vec<Message> = Vec::new();
    for line in text.lines() {
        let Ok(m) = serde_json::from_str::<Message>(line) else { continue };
        match by_id.iter_mut().find(|x| x.id == m.id) {
            Some(slot) => *slot = m,
            None => by_id.push(m),
        }
    }
    by_id.sort_by_key(|m| m.id);
    let skip = by_id.len().saturating_sub(n);
    by_id.into_iter().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tail_of_a_missing_log_is_empty() {
        assert!(read_tail(&PathBuf::from("/nonexistent/horde/bus.jsonl"), 10).is_empty());
    }

    #[test]
    fn later_log_entries_supersede_earlier_ones_for_the_same_id() {
        let p = std::env::temp_dir().join("horde-bus-supersede.jsonl");
        let queued = Message {
            id: 1,
            ts: 1,
            from: "a".into(),
            to: "b".into(),
            body: "hi".into(),
            delivery: Delivery::Queued,
            broadcast: false,
        };
        let delivered = Message { delivery: Delivery::Delivered, ..queued.clone() };
        let text = format!(
            "{}\n{}\n",
            serde_json::to_string(&queued).unwrap(),
            serde_json::to_string(&delivered).unwrap()
        );
        std::fs::write(&p, text).unwrap();

        let tail = read_tail(&p, 10);
        assert_eq!(tail.len(), 1, "the same id must not appear twice");
        assert_eq!(tail[0].delivery, Delivery::Delivered);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn read_tail_skips_malformed_lines() {
        let p = std::env::temp_dir().join("horde-bus-malformed.jsonl");
        std::fs::write(&p, "not json\n{\"partial\":true}\n").unwrap();
        assert!(read_tail(&p, 10).is_empty());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn ring_is_bounded_and_keeps_the_newest() {
        let mut bus = Bus::new(std::env::temp_dir().join("horde-bus-ring-unused.jsonl"));
        // Bypass the log by pushing straight into the ring.
        for i in 0..(RING + 50) {
            bus.messages.push_back(Message {
                id: i as u64,
                ts: 0,
                from: "a".into(),
                to: "b".into(),
                body: String::new(),
                delivery: Delivery::Delivered,
                broadcast: false,
            });
            while bus.messages.len() > RING {
                bus.messages.pop_front();
            }
        }
        assert_eq!(bus.messages.len(), RING);
        assert_eq!(bus.messages.back().unwrap().id, (RING + 49) as u64);
        assert_eq!(bus.recent(3).len(), 3);
    }
}
