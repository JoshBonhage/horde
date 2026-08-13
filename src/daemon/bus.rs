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
//!
//! # Why delivery is also gated on the terminal itself
//!
//! Two properties of a pty, both measured rather than assumed, decide whether a write can
//! succeed at all — and neither reports failure if you ignore it:
//!
//! - **A canonical tty discards a line past `MAX_CANON`** (1024 bytes on macOS). A 4000-byte
//!   message written to a shell pane arrives as 993 bytes with no error. Agent TUIs run in
//!   raw mode, which has no such limit, so this only bites a pane with no agent or one whose
//!   agent is still starting — which is exactly why the answer is to hold, not to truncate.
//! - **A pty master blocks on write by default.** It does not return short writes; it waits
//!   until the slave drains. Writes are issued from the single-threaded engine, so one agent
//!   that stopped reading would freeze every pane, the UI, and all RPCs. horde therefore puts
//!   every master in non-blocking mode and buffers what the terminal will not take yet, and
//!   the bus still checks writability first so a message for an agent that is not reading
//!   stays visibly queued rather than piling up in a buffer.
//!
//! Neither is overridable by `--now`: forcing a write the terminal cannot take does not
//! deliver the message, it loses it quietly or hangs the daemon.
//!
//! The whole bus is paused for now — see [`ENABLED`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::proto::{AgentState, Delivery, Event, Message, MsgKind, PaneId, SpaceId};

use super::state::Session;

/// Master switch for the bus itself. Off: agent-to-agent messaging is paused.
///
/// Every method that moves a message — send, reply, broadcast — is refused at the socket while
/// this is off, so nothing can be injected into a pane however it was asked for: not by hand,
/// not by a trigger, not by the idle nudge. `bus.tail` and `bus.reply_for` are deliberately
/// still allowed, because reading the record delivers nothing. The log on disk is left alone,
/// so pausing costs no history.
///
/// Paired with [`super::tasks::ENABLED`] rather than independent of it: the board's nudge was
/// the thing injecting messages nobody asked for, and half a pause would have left the other
/// half free to surprise you the same way.
///
/// TODO(bus-pause): turning this back on restores the bus by hand. Every gate is an early
/// return in front of code left intact, so there is nothing else to put back.
#[cfg(not(test))]
pub const ENABLED: bool = false;

/// On under `cfg(test)`, so the paused machinery goes on being exercised instead of rotting
/// while it waits — exactly as the board's switches are.
#[cfg(test)]
pub const ENABLED: bool = true;

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
    log: super::logfile::AppendLog,
    messages: VecDeque<Message>,
    next_id: u64,
    /// Messages that were still queued when the last daemon stopped.
    ///
    /// A held message lives on the target's `AgentRuntime`, which does not survive a restart —
    /// the panes come back as fresh processes with no agent until detection runs. Rather than
    /// persist the queue separately, it is recovered from the log: a message whose newest entry
    /// still says `queued` was, by definition, never delivered. They are re-homed by *name*
    /// once an agent answering to it exists again, which is the same addressing the bus uses
    /// everywhere else.
    orphaned: Vec<Message>,
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
        // Anything still marked queued in the log outlived the daemon that was holding it.
        let orphaned: Vec<Message> =
            messages.iter().filter(|m| m.delivery == Delivery::Queued).cloned().collect();
        if !orphaned.is_empty() {
            super::log_line(&format!(
                "bus: {} undelivered message(s) recovered from the log",
                orphaned.len()
            ));
        }
        Bus { log: super::logfile::AppendLog::new(log_path), messages, next_id, orphaned }
    }

    /// How many recovered messages are still waiting for their target to come back.
    pub fn orphan_count(&self) -> usize {
        self.orphaned.len()
    }

    /// Hand recovered messages back to the agents they were addressed to.
    ///
    /// Resolution is by name and happens on every flush pass, because the agent may not exist
    /// yet: after a restart a pane takes a moment to boot and be detected. Until then the
    /// message stays here and keeps showing as queued, which is honest — it has not arrived.
    fn rehome_orphans(&mut self, session: &mut Session) {
        if self.orphaned.is_empty() {
            return;
        }
        let mut still_waiting = Vec::new();
        for msg in std::mem::take(&mut self.orphaned) {
            match Self::resolve(session, &msg.to)
                .and_then(|p| session.panes.get_mut(&p))
                .and_then(|p| p.agent.as_mut())
            {
                Some(agent) => {
                    super::log_line(&format!(
                        "bus: message {} re-homed to {} after a restart",
                        msg.id, msg.to
                    ));
                    agent.queued.push(msg);
                }
                None => still_waiting.push(msg),
            }
        }
        self.orphaned = still_waiting;
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

    fn gate(session: &Session, pane: PaneId, force: bool, len: usize) -> Gate {
        let Some(p) = session.panes.get(&pane) else { return Gate::Hold("pane is gone") };
        // A submit is still pending for this pane. Typing now would land in front of an
        // Enter that has not fired yet, merging two messages into one prompt.
        if p.has_deferred() {
            return Gate::Hold("previous message is still being submitted");
        }
        // A canonical tty discards everything past MAX_CANON without telling anyone, so a
        // long message would arrive cut in half with no indication. Holding is the right
        // answer even under `force`: an agent that is still starting up is canonical for a
        // second or two and raw after that, so the message goes out intact on a later pass.
        // A pane with no agent has nowhere to queue, so this becomes a visible drop instead
        // of a mangled line.
        if p.max_input_line().is_some_and(|max| len > max) {
            return Gate::Hold("message is longer than the target's terminal accepts right now");
        }
        // The pty is a blocking fd. If the target has stopped draining its input, writing
        // would stall the engine — and with it every other pane. Queue instead; this is the
        // same answer the bus gives for every other "not right now".
        if !p.accepts_input() {
            return Gate::Hold("target's terminal is not accepting input");
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
        // Measure what will actually be typed, not the raw body: the `[horde] request …`
        // envelope is part of the line the tty has to accept.
        let len = format_for(&msg).len();
        match Self::gate(session, target, force, len) {
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
        // Messages recovered from the log join the normal queues first, so they take the same
        // gate and the same one-per-pass pacing as anything else.
        self.rehome_orphans(session);

        let candidates: Vec<PaneId> = session
            .panes
            .values()
            .filter(|p| p.agent.as_ref().is_some_and(|a| !a.queued.is_empty()))
            .map(|p| p.id)
            .collect();

        let mut events = Vec::new();
        for pane in candidates {
            // Length is measured against the head of the queue, since that is what would go
            // out on this pass.
            let head_len = session
                .panes
                .get(&pane)
                .and_then(|p| p.agent.as_ref())
                .and_then(|a| a.queued.first())
                .map(|m| format_for(m).len())
                .unwrap_or(0);
            if !matches!(Self::gate(session, pane, cfg.force_inject, head_len), Gate::Submit) {
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

    fn append_log(&mut self, msg: &Message) {
        if let Ok(line) = serde_json::to_string(msg) {
            self.log.append_line(&line);
        }
        // The ring is the live set: replaying it rebuilds the drawer and the orphan list.
        if self.log.rotation_due() {
            let carry: Vec<String> =
                self.messages.iter().filter_map(|m| serde_json::to_string(m).ok()).collect();
            self.log.rotate(&carry);
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
                activity: Default::default(),
                touched: Default::default(),
                nudged_since: None,
                alerted_since: None,
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

        assert!(matches!(Bus::gate(&session, pane, false, 80), Gate::Submit));
        session
            .panes
            .get_mut(&pane)
            .unwrap()
            .write_later(vec![b'\r'], std::time::Duration::from_secs(5));
        assert!(matches!(Bus::gate(&session, pane, false, 80), Gate::Hold(_)));

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


    /// A raw-mode pane standing in for an agent TUI, so a long write can be measured.
    fn session_with_raw_sink() -> (Config, Session) {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/raw_sink.py");
        let mut cfg = Config::default();
        cfg.shell = format!("python3 {script}");
        let mut session = Session::new(&cfg);
        session.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        (cfg, session)
    }

    /// Long messages must arrive whole.
    ///
    /// Measured, not assumed: the far end reports a running byte count, and this asserts the
    /// whole formatted message got there. 60KB is far past every buffer in the path — the tty
    /// input queue is about a kilobyte, so this only passes because a draining reader lets the
    /// blocking write through in pieces.
    ///
    /// Note what this does *not* prove: swapping `write_all` for a single `write` still passes,
    /// because a pty master is a blocking fd and does not return short. Truncation is prevented
    /// by that blocking behaviour; the risk it creates is covered by the next test.
    #[test]
    fn a_long_message_reaches_a_raw_mode_agent_whole() {
        let (cfg, mut session) = session_with_raw_sink();
        let pane = *session.panes.keys().next().unwrap();
        give_agent(&mut session, pane, AgentState::Idle);
        session.panes.get_mut(&pane).unwrap().resize(200, 60).unwrap();
        // Let python get the tty into raw mode before measuring anything.
        std::thread::sleep(std::time::Duration::from_millis(700));

        let body = "x".repeat(60_000);
        let bus = Bus::new(std::env::temp_dir().join("horde-test-long.jsonl"));
        let mut m = msg(1, &body);
        let expected = format_for(&m).len();
        assert!(expected > 60_000, "the envelope should make this longer still");
        bus.deliver(&mut session, pane, &mut m, true);
        assert_eq!(m.delivery, Delivery::Delivered);

        let theme = cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut best = 0usize;
        while std::time::Instant::now() < deadline && best < expected {
            session.panes.get_mut(&pane).unwrap().pump(&theme);
            let flat = session.panes[&pane].visible_text().join("");
            // The sink reports a running total; the largest one is what has arrived.
            for part in flat.split("GOT=").skip(1) {
                let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                best = best.max(digits.parse::<usize>().unwrap_or(0));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        for p in session.panes.values_mut() {
            p.kill();
        }
        assert!(
            best >= expected,
            "message was truncated: {best} of {expected} bytes arrived"
        );
    }

    /// The other half of the same problem: a canonical tty caps a line at MAX_CANON and
    /// discards the rest without erroring. A shell pane is canonical, so a long message must
    /// be held rather than typed in half.
    #[test]
    fn a_canonical_pane_holds_a_long_message_instead_of_cutting_it() {
        let (cfg, mut session) = session_with_cat();
        let pane = *session.panes.keys().next().unwrap();
        give_agent(&mut session, pane, AgentState::Idle);
        std::thread::sleep(std::time::Duration::from_millis(300));

        let max = session.panes[&pane].max_input_line();
        assert_eq!(max, Some(900), "a `cat` pane is canonical: {max:?}");

        // Short messages are unaffected.
        assert!(matches!(Bus::gate(&session, pane, false, 80), Gate::Submit));
        // Long ones are held, and `force` does not override this one — forcing would not
        // make the tty accept the bytes, it would only lose them louder.
        assert!(matches!(Bus::gate(&session, pane, false, 4000), Gate::Hold(_)));
        assert!(matches!(Bus::gate(&session, pane, true, 4000), Gate::Hold(_)));

        // And it queues rather than being reported as delivered.
        let mut bus = Bus::new(std::env::temp_dir().join("horde-test-canon.jsonl"));
        let m = bus
            .send(&mut session, &cfg, None, "target", &"y".repeat(4000), true, false, None)
            .unwrap();
        assert_eq!(m.delivery, Delivery::Queued, "a doomed write must not read as delivered");

        for p in session.panes.values_mut() {
            p.kill();
        }
    }

    /// A hung agent must not be able to freeze the daemon.
    ///
    /// Writes go out from the single-threaded engine onto a blocking pty. A slave that has
    /// stopped draining makes that write block, which would stall every pane, the UI, and all
    /// RPCs — one wedged agent taking down the whole session. So the bus asks whether the pty
    /// will take a write before issuing one, and queues if not.
    #[test]
    fn a_target_that_stopped_reading_queues_instead_of_hanging() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/deaf_sink.py");
        let mut cfg = Config::default();
        cfg.shell = format!("python3 {script}");
        let mut session = Session::new(&cfg);
        session.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        let pane = *session.panes.keys().next().unwrap();
        give_agent(&mut session, pane, AgentState::Idle);
        std::thread::sleep(std::time::Duration::from_millis(700));

        // Fill the tty input queue, which nothing is draining. One byte at a time: POLLOUT
        // promises *some* space, not a bufferful, so a larger write could block right here
        // and hang the test the same way it would hang the daemon.
        let mut filled = 0;
        while filled < 65_536 && session.panes[&pane].accepts_input() {
            if session.panes.get_mut(&pane).unwrap().write(b"z").is_err() {
                break;
            }
            filled += 1;
        }
        assert!(filled > 0, "should have written something before the queue filled");
        assert!(
            !session.panes[&pane].accepts_input(),
            "the queue should be full after {filled} bytes with nothing draining"
        );

        let mut bus = Bus::new(std::env::temp_dir().join("horde-test-deaf.jsonl"));
        let started = std::time::Instant::now();
        let m = bus
            .send(&mut session, &cfg, None, "target", "are you there?", false, false, None)
            .unwrap();
        let took = started.elapsed();

        for p in session.panes.values_mut() {
            p.kill();
        }

        assert_eq!(m.delivery, Delivery::Queued, "a wedged target must queue, not report sent");
        assert!(
            took < std::time::Duration::from_millis(500),
            "send blocked for {took:?} — the engine would have been frozen for that long"
        );
    }

    /// A queued message must survive the daemon that was holding it.
    ///
    /// The queue itself lives on an `AgentRuntime` and cannot survive — so this is recovered
    /// from the log instead, which already records the final delivery of every message.
    #[test]
    fn messages_still_queued_at_shutdown_are_recovered_from_the_log() {
        let p = std::env::temp_dir().join("horde-bus-orphan.jsonl");
        let _ = std::fs::remove_file(&p);
        let held = Message {
            id: 1,
            ts: 1,
            from: "builder".into(),
            to: "target".into(),
            body: "still waiting".into(),
            delivery: Delivery::Queued,
            broadcast: false,
            expects_reply: false,
            reply_to: None,
        };
        let done = Message { id: 2, delivery: Delivery::Delivered, ..held.clone() };
        // Message 3 was queued and then delivered — the later entry must win, so it is *not*
        // an orphan. This is the case a naive "any queued line" scan would get wrong.
        let requeued = Message { id: 3, ..held.clone() };
        let settled = Message { id: 3, delivery: Delivery::Delivered, ..held.clone() };
        let dropped = Message { id: 4, delivery: Delivery::Dropped, ..held.clone() };
        let text = [&held, &done, &requeued, &settled, &dropped]
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&p, format!("{text}\n")).unwrap();

        let bus = Bus::new(p.clone());
        assert_eq!(bus.orphan_count(), 1, "only the still-queued message is outstanding");
        assert_eq!(bus.orphaned[0].id, 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_recovered_message_is_delivered_once_its_agent_is_back() {
        let (cfg, mut session) = session_with_cat();
        let pane = *session.panes.keys().next().unwrap();

        let p = std::env::temp_dir().join("horde-bus-rehome.jsonl");
        let _ = std::fs::remove_file(&p);
        let held = Message {
            id: 1,
            ts: 1,
            from: "builder".into(),
            to: "target".into(),
            body: "from before the restart".into(),
            delivery: Delivery::Queued,
            broadcast: false,
            expects_reply: false,
            reply_to: None,
        };
        std::fs::write(&p, format!("{}\n", serde_json::to_string(&held).unwrap())).unwrap();
        let mut bus = Bus::new(p.clone());

        // No agent yet — the pane is still booting. The message must wait, not vanish.
        let events = bus.flush_queued(&mut session, &cfg);
        assert!(events.is_empty());
        assert_eq!(bus.orphan_count(), 1, "nothing to deliver to yet");

        // Detection finds the agent; now it can be re-homed and sent.
        give_agent(&mut session, pane, AgentState::Idle);
        let events = bus.flush_queued(&mut session, &cfg);
        assert_eq!(bus.orphan_count(), 0, "it should have been handed over");
        assert_eq!(events.len(), 1, "and delivered on the same pass");
        match &events[0] {
            Event::BusMessage(m) => {
                assert_eq!(m.id, 1);
                assert_eq!(m.delivery, Delivery::Delivered);
            }
            other => panic!("expected a bus message, got {other:?}"),
        }

        // The log now ends with a delivered entry, so a further restart will not re-send it.
        let reread = Bus::new(p.clone());
        assert_eq!(reread.orphan_count(), 0, "delivery must be recorded, or it loops forever");

        for pane in session.panes.values_mut() {
            pane.kill();
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_recovered_message_for_an_agent_that_never_returns_is_kept_not_dropped() {
        let (cfg, mut session) = session_with_cat();
        let p = std::env::temp_dir().join("horde-bus-orphan-forever.jsonl");
        let _ = std::fs::remove_file(&p);
        let held = Message {
            id: 1,
            ts: 1,
            from: "builder".into(),
            to: "someone-else".into(),
            body: "hello?".into(),
            delivery: Delivery::Queued,
            broadcast: false,
            expects_reply: false,
            reply_to: None,
        };
        std::fs::write(&p, format!("{}\n", serde_json::to_string(&held).unwrap())).unwrap();

        let mut bus = Bus::new(p.clone());
        for _ in 0..3 {
            assert!(bus.flush_queued(&mut session, &cfg).is_empty());
        }
        assert_eq!(bus.orphan_count(), 1, "it stays outstanding rather than being discarded");
        for pane in session.panes.values_mut() {
            pane.kill();
        }
        let _ = std::fs::remove_file(&p);
    }

    /// A long message to a *slow* reader must neither block nor lose bytes.
    ///
    /// This is the case the writability check alone did not cover: the tty accepts the first
    /// chunk, so the gate says go, and then the agent stops keeping up partway through. With a
    /// blocking master that stalled the engine for the rest of the message. Now the remainder
    /// is buffered and pushed by later ticks, so every `pump` returns promptly and the whole
    /// message still arrives.
    #[test]
    fn a_long_message_to_a_slow_reader_completes_without_stalling_a_tick() {
        let (cfg, mut session) = session_with_raw_sink();
        let pane = *session.panes.keys().next().unwrap();
        give_agent(&mut session, pane, AgentState::Idle);
        session.panes.get_mut(&pane).unwrap().resize(200, 60).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(700));

        let bus = Bus::new(std::env::temp_dir().join("horde-test-slow.jsonl"));
        let mut m = msg(1, &"x".repeat(40_000));
        let expected = format_for(&m).len();

        let started = std::time::Instant::now();
        bus.deliver(&mut session, pane, &mut m, true);
        let deliver_took = started.elapsed();
        assert_eq!(m.delivery, Delivery::Delivered);
        assert!(
            deliver_took < std::time::Duration::from_millis(200),
            "deliver held the engine for {deliver_took:?}"
        );
        assert!(
            session.panes[&pane].has_deferred(),
            "the tail should still be in flight, not silently dropped"
        );

        // Now pump it out, checking that no single tick takes long.
        let theme = cfg.theme.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut best = 0usize;
        let mut worst_tick = std::time::Duration::ZERO;
        while std::time::Instant::now() < deadline && best < expected {
            let t = std::time::Instant::now();
            session.panes.get_mut(&pane).unwrap().pump(&theme);
            worst_tick = worst_tick.max(t.elapsed());
            let flat = session.panes[&pane].visible_text().join("");
            for part in flat.split("GOT=").skip(1) {
                let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                best = best.max(digits.parse::<usize>().unwrap_or(0));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for p in session.panes.values_mut() {
            p.kill();
        }

        assert!(best >= expected, "only {best} of {expected} bytes arrived");
        assert!(
            worst_tick < std::time::Duration::from_millis(150),
            "one tick took {worst_tick:?} — the engine was blocked in a write"
        );
    }

    /// A pane that reads nothing must fail its writes rather than buffer without limit.
    #[test]
    fn a_pane_that_never_reads_stops_accepting_rather_than_growing_forever() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/deaf_sink.py");
        let mut cfg = Config::default();
        cfg.shell = format!("python3 {script}");
        let mut session = Session::new(&cfg);
        session.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        let pane = *session.panes.keys().next().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(700));

        // Writes succeed into the buffer until the cap, then refuse. None of them block.
        let chunk = vec![b'z'; 32 * 1024];
        let started = std::time::Instant::now();
        let mut refused = false;
        for _ in 0..40 {
            if session.panes.get_mut(&pane).unwrap().write(&chunk).is_err() {
                refused = true;
                break;
            }
        }
        let took = started.elapsed();
        for p in session.panes.values_mut() {
            p.kill();
        }
        assert!(refused, "the cap should have been reached and reported");
        assert!(took < std::time::Duration::from_secs(2), "writes blocked for {took:?}");
    }

    /// A queued message must survive log rotation as well as a restart.
    ///
    /// The orphan list is rebuilt from the log, so if rotation dropped the tail then a bounded
    /// log would quietly become a way to lose undelivered messages.
    #[test]
    fn rotating_the_log_keeps_undelivered_messages_recoverable() {
        let p = std::env::temp_dir().join("horde-bus-rotate.jsonl");
        let archive = std::env::temp_dir().join("horde-bus-rotate.jsonl.1");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&archive);

        {
            let mut bus = Bus::new(p.clone());
            bus.log = crate::daemon::logfile::AppendLog::with_max(p.clone(), 1);
            bus.record(Message {
                id: 1,
                ts: 1,
                from: "builder".into(),
                to: "reviewer".into(),
                body: "held across a rotation".into(),
                delivery: Delivery::Queued,
                broadcast: false,
                expects_reply: false,
                reply_to: None,
            });
            for i in 2..320u64 {
                bus.record(Message {
                    id: i,
                    ts: 1,
                    from: "a".into(),
                    to: "b".into(),
                    body: "chatter".into(),
                    delivery: Delivery::Delivered,
                    broadcast: false,
                    expects_reply: false,
                    reply_to: None,
                });
            }
        }
        assert!(archive.exists(), "history should have been archived");

        let bus = Bus::new(p.clone());
        assert_eq!(bus.orphan_count(), 1, "the undelivered message must still be found");
        assert_eq!(bus.orphaned[0].body, "held across a rotation");

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&archive);
    }
}
