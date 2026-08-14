//! Reaching you when nothing is attached.
//!
//! Every other way horde tells you something needs a client on screen: a toast, the sidebar,
//! the bus drawer, the digest you read when you get back. That is the one situation where being
//! told adds nothing — you are already looking. The hour that actually needs a notification is
//! the hour nobody is watching, and until this module the only process still awake for it, the
//! daemon, had no way out at all.
//!
//! So alerts are deliberately the detached half of a pair. The client keeps its own macOS
//! notification for the attached case (see `Client::toast`), where a ping per notice is
//! reasonable because you are there to read it. Down here, where a ping might land on a phone,
//! the rules are stricter:
//!
//! - **Only while detached.** No overlap with the client's path, so nothing is ever delivered
//!   twice, and each half covers exactly what the other cannot.
//! - **Only settled facts.** An agent has to have wanted attention for a full minute. States
//!   passed through mid-turn are not news.
//! - **One ping per wait.** Keyed on when the state began, so an agent stuck for an hour is
//!   reported once — the same trick the board nudge uses, for the same reason.
//! - **One ping per window.** Whatever happened, it arrives as the digest headline rather than
//!   as one message per fact. A notifier you learn to ignore is worse than none.
//!
//! What it says is `Digest::headline()` — the same line the reattach toast uses, because "1
//! agent needs you, 2 tasks done" is already the answer to why you would go back.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::Notify;
use crate::proto::{AgentState, PaneId};

use super::journal::Kind;
use super::{digest, log_line, Engine};

/// How long an agent has to have been waiting before it counts.
///
/// Blocked and done are both states an agent can pass through in the middle of ordinary work —
/// a board worker is briefly `done` between tasks every time it finishes one. A minute is long
/// enough that nothing transient survives it, and short enough that a genuinely stuck agent is
/// reported while the hour it is wasting still has most of itself left.
const SETTLED: Duration = Duration::from_secs(60);

/// Minimum gap between alerts.
///
/// The cost of a missed notification is that you find out at the next one; the cost of too many
/// is that you stop reading them, which loses every notification after that. Five minutes with
/// the whole window summarised in one line errs in the cheaper direction.
const COALESCE: Duration = Duration::from_secs(300);

/// Set while a notify command is still running, so a script that hangs cannot accumulate one
/// stuck child per window for as long as you are away.
static COMMAND_BUSY: AtomicBool = AtomicBool::new(false);

/// Something worth telling you about. Carries what marking it spent requires.
enum Reason {
    /// An agent has been asking for a human, or sitting on a finished turn nobody has read.
    Attention { pane: PaneId, since: Instant },
    /// The board emptied — the fleet is done.
    BoardClear,
}

/// Called every tick. Decides whether to reach out, and does it if so.
pub fn consider(eng: &mut Engine) {
    let system = eng.cfg.notify == Notify::System;
    let command = eng.cfg.notify_command.clone();
    let Some(summary) = prepare(eng, system || command.is_some()) else { return };

    if system {
        deliver_system(&summary.text);
    }
    if let Some(cmd) = &command {
        deliver_command(cmd, &summary.text, &summary.payload);
    }
}

struct Alert {
    text: String,
    payload: String,
}

/// Decide, record, and spend the reasons. Returns what to say, or `None` to stay quiet.
///
/// Split from delivery so the whole decision can be tested without spawning anything: every
/// rule that keeps this quiet lives here, and running a user's script is the easy part.
fn prepare(eng: &mut Engine, have_sink: bool) -> Option<Alert> {
    // Attached is not away. The toast already said it, and the client owns that case.
    if !eng.clients.is_empty() {
        return None;
    }
    if !eng.cfg.notify.reaches_out() || !have_sink {
        return None;
    }

    let now = super::now_millis();
    // Rate-limited before deciding rather than after: a reason found inside the quiet window
    // has to stay found, so that nothing is spent on an alert that was never sent.
    if now.saturating_sub(eng.last_alert) < COALESCE.as_millis() as u64 {
        return None;
    }

    let since = if eng.last_alert == 0 { eng.started } else { eng.last_alert };
    let reasons = reasons(eng, since);
    if reasons.is_empty() {
        return None;
    }

    // The payload is the digest you would have read, over the window since the last ping.
    //
    // Building it deliberately does not advance `last_seen`: being told about something is not
    // the same as having looked at it, so the report waiting when you get back is still the
    // whole story rather than whatever happened after the last notification.
    let d = digest::build(eng, since);
    let text = d.headline()?;
    let payload = serde_json::to_string(&d).unwrap_or_else(|_| "{}".to_string());

    eng.journal.note(Kind::Notified, text.as_str());
    log_line(&format!("notified: {text}"));
    eng.last_alert = now;
    // Spent last, once there is definitely something to send.
    for r in reasons {
        if let Reason::Attention { pane, since } = r {
            if let Some(a) = eng.session.panes.get_mut(&pane).and_then(|p| p.agent.as_mut()) {
                a.alerted_since = Some(since);
            }
        }
    }

    Some(Alert { text, payload })
}

/// What there is to tell you, if anything.
///
/// These are the digest's own top sections in its own order — what needs you, then what
/// finished. Nothing else earns a notification: the rest is texture you can read when you are
/// back, and `headline()` would not have mentioned it anyway.
fn reasons(eng: &Engine, since: u64) -> Vec<Reason> {
    let mut out = Vec::new();
    for p in eng.session.panes.values() {
        let Some(a) = &p.agent else { continue };
        // `Done` persists until the pane is looked at, so an unread finished turn stays a
        // reason for as long as it goes unread — which detached is the whole time.
        if !matches!(a.state, AgentState::Blocked | AgentState::Done) {
            continue;
        }
        if a.since.elapsed() < SETTLED || a.alerted_since == Some(a.since) {
            continue;
        }
        out.push(Reason::Attention { pane: p.id, since: a.since });
    }

    // An empty board is the "your fleet is finished" ping, and the one alert you would plan
    // around. Only when something actually closed in the window, though: a board that was
    // empty all along is not an event, and would otherwise alert forever.
    //
    // Strictly after `since`, where the digest's own window is inclusive of it. The two bounds
    // have to differ: a task that closed in the very millisecond of the last alert was already
    // in that alert's payload, so counting it again here is how one finished board produces two
    // notifications. Found by a test that hit the same millisecond.
    if eng.board.open_count() == 0
        && eng.board.claimed_count() == 0
        && eng.board.all().iter().any(|t| t.done_at.unwrap_or(0) > since)
    {
        out.push(Reason::BoardClear);
    }
    out
}

/// Desktop notification, sent from the daemon this time.
///
/// Unlike the client's copy of this, a host with no notifier is worth saying out loud. Nobody is
/// attached — that is the precondition for being here at all — so a silent no-op is
/// indistinguishable from horde having decided there was nothing to report, and the user is
/// waiting for a ping that is never coming. Said once per daemon rather than once per alert,
/// because it is a fact about the machine and will not have changed by the next window.
fn deliver_system(summary: &str) {
    match crate::platform::system_notify(summary) {
        Some(c) => run_detached(c, None, None),
        None => {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::SeqCst) {
                log_line(crate::platform::no_notifier_hint());
            }
        }
    }
}

/// The user's own program: summary as `$1`, the full digest as JSON on stdin.
///
/// Run through `sh -c` so the config can hold a pipeline rather than only a path, and so `$1`
/// means what it looks like it means. Both channels are offered because they suit different
/// scripts: a one-line curl to a phone wants the summary, and anything that decides what to do
/// wants the digest. This is the whole of horde's reach — Pushover, Telegram, ntfy, a mail
/// command — which is why there is no HTTP client and nowhere here to keep a token.
fn deliver_command(cmd: &str, summary: &str, payload: &str) {
    if COMMAND_BUSY.swap(true, Ordering::SeqCst) {
        log_line("notify command from the previous alert is still running; skipped this one");
        return;
    }
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).arg("horde").arg(summary);
    run_detached(c, Some(payload.to_string()), Some(&COMMAND_BUSY));
}

/// Start a child and forget it, writing `payload` to its stdin if there is one.
///
/// On a thread rather than here, for two reasons that both end with a stalled multiplexer: the
/// engine is single-threaded and drives every pane, and writing to a pipe blocks as soon as the
/// payload outgrows the pipe buffer — which a busy digest does easily. The thread also reaps the
/// child, so a daemon left running for days collects no zombies.
fn run_detached(mut cmd: Command, payload: Option<String>, busy: Option<&'static AtomicBool>) {
    cmd.stdin(if payload.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log_line(&format!("notification could not start: {e}"));
            if let Some(b) = busy {
                b.store(false, Ordering::SeqCst);
            }
            return;
        }
    };
    std::thread::spawn(move || {
        let mut child = child;
        if let (Some(p), Some(mut stdin)) = (payload, child.stdin.take()) {
            let _ = stdin.write_all(p.as_bytes());
            // Dropped here, closing the pipe: a script reading to EOF would otherwise wait
            // for a writer that has nothing left to say.
        }
        let _ = child.wait();
        if let Some(b) = busy {
            b.store(false, Ordering::SeqCst);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::journal::Journal;
    use crate::proto::AgentState;

    /// An engine with `n` agents and log files of its own.
    ///
    /// The journal has to be per-test: these run in parallel, and `Journal::new` reads its file
    /// at construction, so a shared path would let one test's alerts appear in another's count.
    fn eng(tag: &str, n: usize) -> Engine {
        let jp = std::env::temp_dir().join(format!("horde-notify-{tag}-journal.jsonl"));
        let _ = std::fs::remove_file(&jp);
        let mut e = super::super::tests::engine_with_idle_agents(&format!("notify-{tag}"), n);
        e.journal = Journal::new(jp);
        e.clients.clear();
        e
    }

    /// Put an agent into a state, `secs` ago.
    fn set_state(e: &mut Engine, i: usize, state: AgentState, secs: u64) {
        let mut ids: Vec<PaneId> = e.session.panes.keys().copied().collect();
        ids.sort();
        let a = e.session.panes.get_mut(&ids[i]).unwrap().agent.as_mut().unwrap();
        a.state = state;
        a.since = Instant::now() - Duration::from_secs(secs);
    }

    fn notified(e: &Engine) -> Vec<String> {
        e.journal
            .since(0)
            .filter(|x| x.kind == Kind::Notified)
            .map(|x| x.subject.clone())
            .collect()
    }

    #[test]
    fn a_blocked_agent_is_worth_a_notification_once_it_has_waited() {
        let mut e = eng("blocked", 1);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        let a = prepare(&mut e, true).expect("a stuck agent with nobody watching is the case");
        assert!(a.text.contains("1 agent needs you"), "{}", a.text);
        // The payload is the digest itself, so a script can decide what to do with it.
        assert!(a.payload.contains("\"needs_you\""), "{}", a.payload);
        assert_eq!(notified(&e).len(), 1, "and the record says it was sent");
    }

    /// States are passed through in the middle of ordinary work. A board worker is briefly
    /// `done` between every task, and pinging a phone for that would be unusable.
    #[test]
    fn a_state_only_just_entered_is_not_yet_news() {
        let mut e = eng("settling", 1);
        set_state(&mut e, 0, AgentState::Blocked, 5);
        assert!(prepare(&mut e, true).is_none());
        assert!(notified(&e).is_empty());
    }

    #[test]
    fn nothing_is_sent_while_a_client_is_attached() {
        let mut e = eng("attached", 1);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        e.clients.insert(1, super::super::Client { out: tx, needs_full: Vec::new() });
        assert!(prepare(&mut e, true).is_none(), "the toast already said this");
    }

    #[test]
    fn notifications_can_be_turned_off() {
        let mut e = eng("off", 1);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        e.cfg.notify = Notify::Off;
        assert!(prepare(&mut e, true).is_none());
        // And with delivery on but nothing configured to deliver through.
        e.cfg.notify = Notify::Horde;
        assert!(prepare(&mut e, false).is_none());
    }

    /// The rule that decides whether this is usable: one ping per wait, not one per pass.
    #[test]
    fn one_wait_earns_exactly_one_notification() {
        let mut e = eng("once", 1);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        assert!(prepare(&mut e, true).is_some());

        // Same agent, same wait, and now well past the quiet window: still nothing.
        e.last_alert = super::super::now_millis() - COALESCE.as_millis() as u64 - 1;
        assert!(prepare(&mut e, true).is_none(), "an agent stuck an hour is not news twice");
        assert_eq!(notified(&e).len(), 1);
    }

    /// The other half of that rule, and the one a plain "already told you" flag gets wrong: an
    /// agent that blocks, gets answered, and blocks again is news the second time.
    #[test]
    fn a_fresh_wait_earns_a_fresh_notification() {
        let mut e = eng("again", 1);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        assert!(prepare(&mut e, true).is_some());

        e.last_alert = super::super::now_millis() - COALESCE.as_millis() as u64 - 1;
        set_state(&mut e, 0, AgentState::Working, 10);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        assert!(prepare(&mut e, true).is_some(), "a new wait is a new fact");
        assert_eq!(notified(&e).len(), 2);
    }

    #[test]
    fn a_second_alert_waits_for_the_quiet_window() {
        let mut e = eng("coalesce", 2);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        assert!(prepare(&mut e, true).is_some());

        // A different agent, so the reason is genuinely new — but it is 10 seconds later.
        set_state(&mut e, 1, AgentState::Blocked, 90);
        assert!(prepare(&mut e, true).is_none(), "one ping per window, whatever happened");

        // Suppressed, not spent: once the window opens it is still there to report.
        e.last_alert = super::super::now_millis() - COALESCE.as_millis() as u64 - 1;
        assert!(prepare(&mut e, true).is_some());
    }

    /// A finished board is the ping you would actually plan around, and the reason the window
    /// matters: an empty board that was empty all along would otherwise alert forever.
    #[test]
    fn an_emptied_board_is_reported_and_an_always_empty_one_is_not() {
        let mut e = eng("board", 1);
        assert!(prepare(&mut e, true).is_none(), "nothing has happened yet");

        e.board.add("port the parser", "user", None).unwrap();
        e.board.claim("worker0", None, None).unwrap();
        e.board.done("worker0", None, Some("done, 18 tests")).unwrap();
        let a = prepare(&mut e, true).expect("the fleet finishing is worth knowing");
        assert!(a.text.contains("1 task done"), "{}", a.text);

        // And it does not repeat, because the window has moved past the task that closed.
        //
        // Asserted through `reasons` rather than by winding `last_alert` back to open the quiet
        // window: that one field is both the rate limit and the reported window, so rewinding it
        // would drag the finished task back into view and test the opposite of the real case,
        // where the marker only ever moves forward.
        assert!(reasons(&e, e.last_alert).is_empty(), "an emptied board is not an event twice");
    }

    /// Being told is not the same as having looked. If an alert advanced the digest window, the
    /// report waiting when you got back would cover only what happened after the last ping.
    #[test]
    fn alerting_does_not_consume_the_digest_you_have_not_read() {
        let mut e = eng("window", 1);
        set_state(&mut e, 0, AgentState::Blocked, 90);
        let before = e.last_seen;
        assert!(prepare(&mut e, true).is_some());
        assert_eq!(e.last_seen, before, "the digest window is the human's, not the notifier's");
    }

    /// The contract a notify script is written against: `$1` and stdin.
    #[test]
    fn the_command_sink_gets_the_summary_as_an_argument_and_the_digest_on_stdin() {
        let dir = std::env::temp_dir();
        let arg = dir.join("horde-notify-sink.arg");
        let body = dir.join("horde-notify-sink.body");
        let _ = std::fs::remove_file(&arg);
        let _ = std::fs::remove_file(&body);

        let cmd = format!(
            "printf '%s' \"$1\" > {}; cat > {}",
            arg.to_string_lossy(),
            body.to_string_lossy()
        );
        deliver_command(&cmd, "1 agent needs you", "{\"needs_you\":[]}");

        // Spawned and reaped on a thread, so poll rather than assume it has finished.
        //
        // Polling for *content* rather than for the file, because `> path` creates the file when
        // the shell sets the redirection up — before `cat` has read a byte of stdin. Waiting on
        // `exists()` therefore races the write and observes an empty file, rarely enough to look
        // like magic and often enough to redden a CI run. Caught doing exactly that on Linux.
        let written = || std::fs::read_to_string(&body).is_ok_and(|s| !s.is_empty());
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) && !written() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&arg).unwrap(), "1 agent needs you");
        assert_eq!(std::fs::read_to_string(&body).unwrap(), "{\"needs_you\":[]}");

        let _ = std::fs::remove_file(&arg);
        let _ = std::fs::remove_file(&body);
    }
}
