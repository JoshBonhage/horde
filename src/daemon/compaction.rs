//! Noticing that an agent is about to lose its memory.
//!
//! Every coding agent handles a full context the same way: it summarises the conversation and
//! throws the detail away. What survives is what the summariser thought mattered, which is
//! almost never the thing that took the session to work out — the dead end you already ruled
//! out, the reason the obvious refactor does not work, where the real entry point is.
//!
//! horde can see this coming and the agent cannot act on it unprompted, which is exactly the
//! shape of thing a supervisor is for. So: watch for the warning, and while there is still
//! room to think, tell the agent to write down what it would be sorry to lose. The note goes
//! to [`memory`](super::memory), where it outlives the conversation, the pane, and the agent.
//!
//! # Why a percentage, and why a threshold
//!
//! Agents print this warning early and keep printing it. "Context left until auto-compact:
//! 45%" is not news — it is most of a session — and nudging on the phrase alone would mean an
//! interruption in the middle of ordinary work, every time. So the number is read, and only a
//! genuinely low one counts. A warning with no number at all is taken at face value, because
//! an agent that has stopped quantifying it is not being reassuring.
//!
//! # Why this is not a manifest rule
//!
//! Manifests decide *state*, and running low on context is not one: an agent at 8% is working,
//! or idle, or blocked, exactly as it was at 80%. This is a second, independent fact about the
//! same screen, and giving it a state would mean inventing one that nothing else can use.

/// Below this, the warning is worth acting on.
///
/// A quarter left is enough room to write a considered note and keep working; a tenth is
/// enough room to write a rushed one. The nudge is queued rather than injected mid-turn, so
/// this has to fire early enough to survive the wait.
const LOW_PERCENT: u32 = 25;

/// How far up the screen to look.
///
/// Small. This is drawn in the agent's status area, at the bottom, and is repainted every
/// frame — a wide window would find the warning in scrollback long after it stopped being
/// true, and go on nudging an agent that has already compacted and has plenty of room.
const LOOK_BACK: usize = 6;

/// Phrases that mean "this conversation is about to be summarised away".
///
/// Deliberately generic rather than per-agent, for the same reason `question` is: every agent
/// that has this problem describes it in the same handful of words, because there are not many
/// ways to say it.
///
/// Short, for the reason the Claude manifest matches `esc to int` rather than the whole
/// phrase: this is drawn in a status line, status lines are the first thing a narrow pane
/// wraps, and a wrapped phrase matches nothing. `auto-compact` rather than
/// `until auto-compact`, and the window is joined before it is searched — see `pressure`.
const WARNINGS: [&str; 4] = ["auto-compact", "context left", "context low", "context remaining"];

/// The compaction has already started. Too late to save anything — recorded rather than acted
/// on, so the digest can say what happened to a session that came back thinner than it left.
const UNDERWAY: [&str; 2] = ["compacting conversation", "compacting context"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// The agent is warning about its context, with this much left where it said.
    Low { percent: Option<u32> },
    /// It is summarising right now.
    Underway,
}

/// What the bottom of an agent's screen says about its remaining context.
///
/// `None` — which is the overwhelmingly common answer — means the screen said nothing about
/// it, and nothing should happen.
pub fn pressure(lines: &[String]) -> Option<Pressure> {
    let from = lines.len().saturating_sub(LOOK_BACK);
    // Joined with no separator, because that is exactly how a terminal wraps: a status line
    // wider than the pane is split mid-word with nothing inserted, so
    // `Context left until auto-compact: 6%` arrives as `…until auto` + `-compact: 6%` and
    // matches neither half. Searching the window whole is what makes a narrow pane behave
    // like a wide one — and it is also what keeps the percentage attached to its phrase,
    // which a per-line scan loses at exactly the wrap that matters.
    let window: String = lines[from..].concat().to_ascii_lowercase();
    if UNDERWAY.iter().any(|p| window.contains(p)) {
        return Some(Pressure::Underway);
    }
    // The last occurrence: the status line is repainted, and an older copy further up the
    // window is a number that has already been superseded.
    let at = WARNINGS.iter().filter_map(|p| window.rfind(p).map(|i| i + p.len())).max()?;
    let percent = percent_after(&window[at..]);
    // A number that is not low is the agent reporting healthy headroom, which is not a warning
    // at all however alarming the phrase around it sounds.
    match percent {
        Some(p) if p > LOW_PERCENT => None,
        _ => Some(Pressure::Low { percent }),
    }
}

/// The first `NN%` in `tail`, when there is one close by.
///
/// Bounded to the start of the tail because the status line holds other numbers — a token
/// count, a duration — and a percentage found twenty characters away is not the one the
/// phrase was introducing.
fn percent_after(tail: &str) -> Option<u32> {
    let window: String = tail.chars().take(12).collect();
    let at = window.find('%')?;
    let digits: String =
        window[..at].chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
    digits.parse().ok()
}

/// What horde tells an agent that is running out of room.
///
/// Concrete, for the same reason the handover instruction is: a vague nudge produces a vague
/// note, and a vague note is worse than none — it costs a read to find out it says nothing.
/// It names the command, because an agent that has to go and find out how to save a memory
/// will spend the context it was told to protect finding out.
pub fn nudge(name: &str) -> String {
    format!(
        "You are running low on context and will compact shortly, losing the detail of this \
         conversation. Before that happens, save what you would be sorry to lose: run \
         `horde memory save {name}-context` and pipe in what a fresh agent on this project \
         would need and could not work out from the code — what you ruled out and why, where \
         the real entry points are, which of the plausible designs this codebase actually \
         uses. Not a summary of what you did; git has that. Then carry on."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(lines: &[&str]) -> Option<Pressure> {
        pressure(&lines.iter().map(|l| l.to_string()).collect::<Vec<_>>())
    }

    /// The signal, as agents actually draw it.
    #[test]
    fn a_low_warning_with_a_number_is_read() {
        assert_eq!(
            p(&["Context left until auto-compact: 8%"]),
            Some(Pressure::Low { percent: Some(8) })
        );
        assert_eq!(
            p(&["  ⚠ Context low (12% remaining) · Run /compact"]),
            Some(Pressure::Low { percent: Some(12) })
        );
    }

    /// The whole reason the number is parsed rather than the phrase matched: most of a session
    /// is spent above the threshold, and nudging there is an interruption during ordinary work.
    #[test]
    fn plenty_of_headroom_is_not_a_warning() {
        assert_eq!(p(&["Context left until auto-compact: 62%"]), None);
        assert_eq!(p(&["Context left until auto-compact: 26%"]), None);
        assert_eq!(
            p(&["Context left until auto-compact: 25%"]),
            Some(Pressure::Low { percent: Some(25) })
        );
    }

    /// An agent that has stopped quantifying it is not being reassuring.
    #[test]
    fn a_warning_with_no_number_is_taken_at_face_value() {
        assert_eq!(p(&["Context low — run /compact"]), Some(Pressure::Low { percent: None }));
    }

    #[test]
    fn a_compaction_already_running_is_recorded_not_acted_on() {
        assert_eq!(p(&["Compacting conversation…"]), Some(Pressure::Underway));
    }

    /// Nothing on an ordinary screen may look like this, or every agent gets interrupted.
    #[test]
    fn an_ordinary_screen_says_nothing_about_context() {
        assert_eq!(p(&["> ", "esc to interrupt · 42 tokens", "src/main.rs edited"]), None);
        assert_eq!(p(&["running 12 tests", "test result: ok. 100% passed"]), None);
    }

    /// A status line wider than the pane is split mid-word with nothing inserted. This is the
    /// exact screen a twenty-column pane produced, and every phrase missed it.
    #[test]
    fn a_warning_wrapped_across_two_lines_is_still_found() {
        assert_eq!(
            p(&["Context left until auto", "-compact: 6%"]),
            Some(Pressure::Low { percent: Some(6) })
        );
        // And the number still has to survive the wrap, or a healthy agent gets nudged.
        assert_eq!(p(&["Context left until auto", "-compact: 71%"]), None);
    }

    /// The warning is repainted in the status area every frame. Finding it in scrollback long
    /// after it stopped being true would nudge an agent that has already compacted.
    #[test]
    fn a_stale_warning_further_up_the_screen_is_not_found() {
        let mut lines = vec!["Context left until auto-compact: 4%".to_string()];
        lines.extend((0..20).map(|i| format!("line {i}")));
        assert_eq!(pressure(&lines), None);
    }

    /// A percentage belonging to some other number on the same status line is not this one.
    #[test]
    fn a_distant_percentage_is_not_the_one_the_phrase_introduced() {
        assert_eq!(
            p(&["context low · 1.2k tokens · cache 94% hit"]),
            Some(Pressure::Low { percent: None })
        );
    }

    #[test]
    fn the_nudge_names_the_command_and_the_note() {
        let n = nudge("builder");
        assert!(n.contains("horde memory save builder-context"), "{n}");
    }
}
