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
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::proto::{AgentState, Delivery, Event, Message, MsgKind, PaneId, SpaceId};

use super::state::Session;

/// Messages kept in memory for the bus drawer.
const RING: usize = 500;

/// How long after the text to send Enter.
///
/// Agents detect a paste by noticing several bytes arriving in one read, and a trailing
/// carriage return inside a paste becomes a literal newline rather than a submit. Sending
/// Enter as its own write, a beat later, makes it read as a keypress. Verified against
/// Claude Code, which otherwise leaves the message sitting unsent in its input box.
pub const SUBMIT_DELAY: Duration = Duration::from_millis(120);

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
        // A submit is still pending for this pane. Typing now would land in front of an
        // Enter that has not fired yet, merging two messages into one prompt.
        if p.has_deferred() {
            return Gate::Hold("previous message is still being submitted");
        }
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
    #[allow(clippy::too_many_arguments)]
    pub fn send(
        &mut self,
        session: &mut Session,
        cfg: &Config,
        from_pane: Option<PaneId>,
        to: &str,
        body: &str,
        force: bool,
        expects_reply: bool,
        reply_to: Option<u64>,
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
            expects_reply,
            reply_to,
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

    /// Write a message into a pane.
    ///
    /// `submit` schedules Enter as a *separate* write rather than appending it, because an
    /// agent reading text and CR in one chunk treats the whole thing as a paste and turns
    /// the CR into a newline. See [`SUBMIT_DELAY`].
    fn deliver(&self, session: &mut Session, pane: PaneId, msg: &mut Message, submit: bool) {
        let text = format_for(msg);
        match session.panes.get_mut(&pane) {
            Some(p) => {
                msg.delivery = match p.write(text.as_bytes()) {
                    Ok(()) => {
                        if submit {
                            p.write_later(vec![b'\r'], SUBMIT_DELAY);
                        }
                        Delivery::Delivered
                    }
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
            match self.send(session, cfg, from_pane, &name, body, false, false, None) {
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
            // One message per pass. Each delivered message submits a prompt, so sending the
            // whole queue at once would stack several turns of work on the recipient — and
            // the second message's text would land before the first one's Enter. The rest
            // stay queued and flush on later passes as it returns to idle.
            let next: Option<Message> = session
                .panes
                .get_mut(&pane)
                .and_then(|p| p.agent.as_mut())
                .and_then(|a| if a.queued.is_empty() { None } else { Some(a.queued.remove(0)) });

            if let Some(mut msg) = next {
                self.deliver(session, pane, &mut msg, true);
                self.update_delivery(msg.id, msg.delivery);
                events.push(Event::BusMessage(msg));
            }
        }
        events
    }

    /// Record a reply for an asker that owns no pane — a `horde ask` run from a plain shell,
    /// where the sender is "user". There is nothing to type the answer into, but the waiting
    /// CLI polls [`Bus::reply_for`], so landing in the log *is* the delivery.
    pub fn record_reply(
        &mut self,
        session: &Session,
        from_pane: Option<PaneId>,
        to: &str,
        body: &str,
        reply_to: u64,
    ) -> Message {
        let msg = Message {
            id: self.next_id,
            ts: super::now_millis(),
            from: Self::sender_name(session, from_pane),
            to: to.to_string(),
            body: body.to_string(),
            delivery: Delivery::Delivered,
            broadcast: false,
            expects_reply: false,
            reply_to: Some(reply_to),
        };
        self.next_id += 1;
        self.record(msg.clone());
        msg
    }

    /// The first reply to `request`, if one has arrived.
    pub fn reply_for(&self, request: u64) -> Option<Message> {
        self.messages.iter().find(|m| m.reply_to == Some(request)).cloned()
    }

    /// A message by id, so a reply can be addressed back to whoever asked.
    pub fn message(&self, id: u64) -> Option<Message> {
        self.messages.iter().find(|m| m.id == id).cloned()
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

/// The exact bytes written into a recipient's terminal, minus the Enter that follows.
///
/// Newlines are flattened: a body containing one would submit early, one line at a time,
/// turning a single message into several half-messages. The `[horde]` prefix is the
/// recipient's signal that this is another agent rather than the human.
///
/// A request additionally spells out the command to answer with. Without that the sender has
/// to embed instructions by hand and hope they are followed, which is the difference between
/// a delegation that returns a value and one that returns nothing.
///
/// A request spells out the exact command to run. Without that the sender has to embed
/// instructions by hand and hope they are followed, which is the difference between a
/// delegation that returns a value and one that returns nothing.
pub fn format_for(msg: &Message) -> String {
    let body = msg.body.trim().replace(['\r', '\n'], " ");
    match msg.kind() {
        // Deliberately explicit. A recipient that treats `horde reply` as something to
        // investigate rather than run will spend a turn grepping for it — observed in
        // testing before this wording, and before the skill was installed.
        MsgKind::Request => format!(
            "[horde] request #{} from {}: {} \u{2014} answer by running this shell command \
             exactly once, and nothing else: horde reply {} \"<your one-line answer>\"",
            msg.id, msg.from, body, msg.id
        ),
        MsgKind::Reply => format!(
            "[horde] reply from {} (re #{}): {}",
            msg.from,
            msg.reply_to.unwrap_or(0),
            body
        ),
        MsgKind::Plain => format!("[horde] message from {}: {}", msg.from, body),
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

    use crate::config::Config;
    use crate::daemon::state::{AgentRuntime, Session};
    use crate::proto::AgentState;

    /// A session with one pane running `cat`, so anything written to the pty comes straight
    /// back and lands in the mirror where a test can see it.
    fn session_with_cat() -> (Config, Session) {
        let mut cfg = Config::default();
        cfg.shell = "cat".into();
        let mut session = Session::new(&cfg);
        session.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        (cfg, session)
    }

    fn give_agent(session: &mut Session, pane: PaneId, state: AgentState) {
        session.panes.get_mut(&pane).unwrap().agent = Some(AgentRuntime {
            kind: "claude".into(),
            name: "target".into(),
            state,
            since: std::time::Instant::now(),
            authority: "hook".into(),
            reason: "t".into(),
            seen: false,
            session_id: None,
            queued: Vec::new(),
        });
    }

    fn msg(id: u64, body: &str) -> Message {
        Message {
            id,
            ts: 0,
            from: "builder".into(),
            to: "target".into(),
            body: body.into(),
            delivery: Delivery::Queued,
            broadcast: false,
            expects_reply: false,
            reply_to: None,
        }
    }

    #[test]
    fn the_message_body_carries_no_carriage_return() {
        // The bug this guards: an agent reading text and CR in one chunk treats the whole
        // thing as a paste and inserts a newline instead of submitting, leaving the message
        // sitting unsent in its input box. Enter has to be a separate write.
        let text = format_for(&msg(1, "please review src/bus.rs"));
        assert!(!text.contains('\r'), "{text:?}");
        assert!(!text.contains('\n'), "{text:?}");
        assert_eq!(text, "[horde] message from builder: please review src/bus.rs");
    }

    #[test]
    fn newlines_in_a_body_are_flattened_not_submitted() {
        // Otherwise each line submits separately, splitting one message into several.
        let text = format_for(&msg(1, "line one\nline two\r\nline three"));
        assert_eq!(text, "[horde] message from builder: line one line two  line three");
        assert!(!text.contains('\n') && !text.contains('\r'));
    }

    #[test]
    fn delivering_writes_the_text_now_and_enter_later() {
        let (cfg, mut session) = session_with_cat();
        let pane = *session.panes.keys().next().unwrap();
        give_agent(&mut session, pane, AgentState::Idle);

        let bus = Bus::new(std::env::temp_dir().join("horde-test-deliver.jsonl"));
        let mut m = msg(1, "ping");
        bus.deliver(&mut session, pane, &mut m, true);
        assert_eq!(m.delivery, Delivery::Delivered);
        assert!(session.panes[&pane].has_deferred(), "Enter must be pending, not already sent");

        // The text itself reaches the pty immediately; `cat` echoes it back to us.
        let theme = cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw = false;
        while std::time::Instant::now() < deadline && !saw {
            session.panes.get_mut(&pane).unwrap().pump(&theme);
            saw = session.panes[&pane]
                .visible_text()
                .iter()
                .any(|l| l.contains("message from builder"));
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(saw, "the message text should have reached the pane");

        // And the deferred Enter fires once its delay has passed.
        std::thread::sleep(SUBMIT_DELAY + std::time::Duration::from_millis(60));
        session.panes.get_mut(&pane).unwrap().pump(&theme);
        assert!(!session.panes[&pane].has_deferred(), "Enter should have been written by now");

        for p in session.panes.values_mut() {
            p.kill();
        }
    }

    #[test]
    fn a_pending_submit_holds_the_next_message() {
        // Typing while an Enter is still queued would merge two messages into one prompt.
        let (_cfg, mut session) = session_with_cat();
        let pane = *session.panes.keys().next().unwrap();
        give_agent(&mut session, pane, AgentState::Idle);

        assert!(matches!(Bus::gate(&session, pane, false), Gate::Submit));
        session
            .panes
            .get_mut(&pane)
            .unwrap()
            .write_later(vec![b'\r'], std::time::Duration::from_secs(5));
        assert!(matches!(Bus::gate(&session, pane, false), Gate::Hold(_)));

        for p in session.panes.values_mut() {
            p.kill();
        }
    }

    #[test]
    fn flushing_delivers_one_message_per_pass() {
        // Each delivered message submits a prompt, so draining the whole queue at once would
        // stack turns on the recipient and land later text before earlier Enters.
        let (cfg, mut session) = session_with_cat();
        let pane = *session.panes.keys().next().unwrap();
        give_agent(&mut session, pane, AgentState::Idle);
        {
            let agent = session.panes.get_mut(&pane).unwrap().agent.as_mut().unwrap();
            agent.queued = vec![msg(1, "one"), msg(2, "two"), msg(3, "three")];
        }

        let mut bus = Bus::new(std::env::temp_dir().join("horde-test-flush.jsonl"));
        let events = bus.flush_queued(&mut session, &cfg);
        assert_eq!(events.len(), 1, "exactly one message per pass");
        assert_eq!(
            session.panes[&pane].agent.as_ref().unwrap().queued.len(),
            2,
            "the rest must stay queued"
        );

        // The pending Enter then holds the queue until it has fired.
        assert!(
            bus.flush_queued(&mut session, &cfg).is_empty(),
            "a pending submit should hold the queue"
        );

        for p in session.panes.values_mut() {
            p.kill();
        }
    }

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
            expects_reply: false,
            reply_to: None,
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
                expects_reply: false,
                reply_to: None,
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
