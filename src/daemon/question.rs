//! Lifting a pending question off a blocked agent's screen.
//!
//! Detection already knows *that* an agent is waiting on you. This works out *what it asked*,
//! which is the difference between a sidebar that says six agents need you and one place that
//! shows you the six questions.
//!
//! # Why this is a heuristic, and why that is acceptable here
//!
//! Manifests decide state, and a wrong state is a lie the whole UI repeats. This decides
//! display text, and a wrong parse costs a question you go and read in its own pane instead.
//! So it is deliberately generic rather than per-agent: every agent that asks a question
//! draws it the same way, as a line ending in `?` above a numbered list, because that is what
//! a terminal makes easy. One parser covers Claude, Codex, Gemini and Cursor without six
//! manifests having to agree on a region.
//!
//! What it will not do is guess. Nothing matched means no question, and the approval queue
//! shows the agent with "open the pane" rather than inventing a prompt.

use crate::proto::{Choice, Question};

/// How far up the screen to look. The prompt is the newest thing on it; anything older is
/// the turn that led to the prompt, and a wider window only finds stale questions.
const LOOK_BACK: usize = 30;

/// Longest question text kept. Two lines of a wide overlay, past which it is being read in
/// the pane anyway.
const MAX_TEXT: usize = 160;

/// Most options offered. The queue answers by pressing the digit, so ten would need a key
/// that is not a digit and no agent asks that many.
const MAX_OPTIONS: usize = 9;

/// Box-drawing and decoration that frames a prompt without being part of it.
fn undecorate(line: &str) -> &str {
    line.trim().trim_matches(|c: char| {
        matches!(c, '│' | '┃' | '║' | '▌' | '▐' | '|' | '╭' | '╮' | '╰' | '╯' | '─' | '━' | '═')
            || c.is_whitespace()
    })
}

/// A numbered choice: `❯ 1. Yes`, `2) No`, `  3. Yes, and don't ask again`.
fn as_option(line: &str) -> Option<Choice> {
    let s = undecorate(line);
    // The selection marker is not part of the label, and which one is selected is the
    // agent's business rather than something to show in a list you pick from by number.
    let s = s.trim_start_matches(['❯', '>', '›', '»', '*']).trim_start();
    let (num, rest) = s.split_once(['.', ')'])?;
    let key = num.trim();
    if key.len() != 1 || !key.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let label = rest.trim();
    if label.is_empty() {
        return None;
    }
    Some(Choice { key: key.to_string(), label: label.chars().take(60).collect() })
}

/// Does this line read as the question the options answer?
fn asks(line: &str) -> bool {
    let l = undecorate(line).to_lowercase();
    l.ends_with('?')
        || l.contains("do you want")
        || l.contains("would you like")
        || l.contains("allow")
        || l.contains("proceed")
}

/// The question a blocked agent is waiting on, if one can be read off the screen.
///
/// `lines` is the detection snapshot: newest at the end.
pub fn extract(lines: &[String]) -> Option<Question> {
    let from = lines.len().saturating_sub(LOOK_BACK);
    let tail = &lines[from..];

    // Undecorate first, then drop what is left with nothing in it.
    //
    // The dropping is load-bearing, not tidiness. A prompt box wider than the pane wraps, and
    // every wrapped row leaves a fragment of border — ` │` — sitting between two options. On
    // the raw lines those fragments break one list of three choices into three lists of one,
    // and a list of one is not a menu, so a narrow pane silently produced no question at all.
    let rows: Vec<String> = tail
        .iter()
        .map(|l| undecorate(l).to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // Find the last run of numbered options. Last rather than first because a transcript can
    // hold every prompt of the session, and only the bottom one is still being asked.
    let mut end = None;
    let mut start = 0;
    for (i, l) in rows.iter().enumerate() {
        if as_option(l).is_some() {
            if end.is_none_or(|e| i > e + 1) {
                // A gap means this is a new run rather than a continuation of the last.
                start = i;
            }
            end = Some(i);
        }
    }

    match end {
        Some(end) => {
            let options: Vec<Choice> =
                rows[start..=end].iter().filter_map(|l| as_option(l)).take(MAX_OPTIONS).collect();
            // One option is a list of length one, which is a menu nobody is choosing from.
            if options.len() < 2 {
                return None;
            }
            let text = question_above(&rows[..start])?;
            Some(Question { text, options })
        }
        // No numbered list. A plain yes/no prompt is the other shape agents ask in, and it
        // has no list to find — the marker itself is the whole answer.
        None => yes_no(&rows),
    }
}

/// The nearest line above the options that reads like a question.
///
/// Walks up rather than taking the immediately preceding line, because prompts put a blank
/// line and sometimes the command being approved between the two.
fn question_above(before: &[String]) -> Option<String> {
    let mut fallback = None;
    for l in before.iter().rev().take(8) {
        let s = undecorate(l);
        if s.is_empty() {
            continue;
        }
        if asks(s) {
            return Some(s.chars().take(MAX_TEXT).collect());
        }
        // Something was there, even if it does not end in a question mark. Better than
        // reporting a set of options with nothing above them.
        fallback.get_or_insert_with(|| s.chars().take(MAX_TEXT).collect::<String>());
    }
    fallback
}

/// `(y/n)`, `[y/N]`, `(y)es/(n)o` — one keypress, no list.
fn yes_no(tail: &[String]) -> Option<Question> {
    let hit = tail.iter().rev().take(8).find(|l| {
        let s = undecorate(l).to_lowercase();
        s.contains("(y/n)") || s.contains("[y/n]") || s.contains("(y)es") || s.contains("y/n?")
    })?;
    let text = undecorate(hit).chars().take(MAX_TEXT).collect::<String>();
    Some(Question {
        text,
        options: vec![
            Choice { key: "y".into(), label: "yes".into() },
            Choice { key: "n".into(), label: "no".into() },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(|l| l.to_string()).collect()
    }

    /// The real thing, captured from a live Claude Code session.
    #[test]
    fn a_real_permission_prompt_is_read_whole() {
        let screen = lines(include_str!("../../tests/fixtures/claude-blocked.txt"));
        let q = extract(&screen).expect("a blocked screen has a question on it");
        assert_eq!(q.text, "Do you want to make this edit to src/mux.rs?");
        assert_eq!(q.options.len(), 3);
        assert_eq!(q.options[0].key, "1");
        assert_eq!(q.options[0].label, "Yes");
        // The selection marker belongs to the agent, not to a list you pick from by number.
        assert!(!q.options[0].label.contains('❯'));
        assert_eq!(q.options[2].label, "No, and tell Claude what to do differently");
    }

    /// Captured from a live pane too narrow for the prompt box, where every wrapped row left
    /// a border fragment between the options. On the raw lines this parsed as three menus of
    /// one choice each, which is to say as nothing at all.
    #[test]
    fn a_prompt_box_wider_than_its_pane_still_parses() {
        let screen = lines(
            "? for shortcuts\n\
             ⏺ I need to edit a file.\n\
             ╭─────────────────────────────────────────────\n\
             ─╮\n\
             │ Do you want to make this edit to src/mux.rs?\n\
             \x20│\n\
             │\n\
             \x20│\n\
             │ ❯ 1. Yes\n\
             \x20│\n\
             │   2. Yes, and do not ask again\n\
             \x20│\n\
             │   3. No, and tell Claude what to do\n\
             \x20│\n\
             ╰─────────────────────────────────────────────\n\
             ─╯\n",
        );
        let q = extract(&screen).expect("a wrapped box is still a prompt");
        assert_eq!(q.text, "Do you want to make this edit to src/mux.rs?");
        assert_eq!(q.options.len(), 3);
        assert_eq!(q.options[2].label, "No, and tell Claude what to do");
    }

    /// A transcript holds every prompt of the session. Only the bottom one is still being
    /// asked, and answering the one from twenty minutes ago would be worse than useless.
    #[test]
    fn the_newest_prompt_wins_over_everything_above_it() {
        let screen = lines(
            "Do you want to run the tests?\n\
             ❯ 1. Yes\n\
             \x20 2. No\n\
             \n\
             ⏺ Ran the tests.\n\
             \n\
             Do you want to commit this?\n\
             ❯ 1. Yes\n\
             \x20 2. No, and tell Claude what to do\n",
        );
        let q = extract(&screen).unwrap();
        assert_eq!(q.text, "Do you want to commit this?");
        assert_eq!(q.options.len(), 2);
    }

    #[test]
    fn a_plain_yes_no_prompt_has_no_list_to_find() {
        let screen = lines("aider> Apply edits to main.py? (y/n)");
        let q = extract(&screen).unwrap();
        assert_eq!(q.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["y", "n"]);
        assert!(q.text.contains("Apply edits"));
    }

    /// An idle agent must produce nothing. A queue that invents questions is worse than one
    /// that occasionally misses them.
    #[test]
    fn an_idle_screen_asks_nothing() {
        let screen = lines(include_str!("../../tests/fixtures/claude-idle.txt"));
        assert_eq!(extract(&screen), None);
    }

    #[test]
    fn a_working_screen_asks_nothing() {
        let screen = lines(include_str!("../../tests/fixtures/claude-working.txt"));
        assert_eq!(extract(&screen), None);
    }

    /// A numbered list is a menu; one item is not.
    #[test]
    fn a_single_numbered_line_is_not_a_menu() {
        let screen = lines("Here is what I found:\n1. the parser is wrong\n");
        assert_eq!(extract(&screen), None);
    }

    /// Prompts are drawn inside a box, and the box is not part of the question.
    #[test]
    fn box_drawing_is_stripped_from_both_ends() {
        let screen = lines(
            "╭────────────────────────╮\n\
             │ Allow this command?    │\n\
             │                        │\n\
             │ ❯ 1. Yes               │\n\
             │   2. No                │\n\
             ╰────────────────────────╯\n",
        );
        let q = extract(&screen).unwrap();
        assert_eq!(q.text, "Allow this command?");
        assert_eq!(q.options[1].label, "No");
    }

    /// The line immediately above a prompt is often blank, or the command being approved.
    #[test]
    fn the_question_is_found_above_a_gap() {
        let screen = lines(
            "Do you want to proceed?\n\
             \n\
             ❯ 1. Yes\n\
             \x20 2. No\n",
        );
        assert_eq!(extract(&screen).unwrap().text, "Do you want to proceed?");
    }

    /// Ten options would need a key that is not a digit, and the queue answers by digit.
    #[test]
    fn more_options_than_there_are_digits_are_capped() {
        let mut s = String::from("Pick one?\n");
        for i in 1..=12 {
            s.push_str(&format!("{i}. option {i}\n"));
        }
        // Two-digit numbers are not options at all, so this is really a cap on the parse.
        let q = extract(&lines(&s)).unwrap();
        assert!(q.options.len() <= MAX_OPTIONS, "{}", q.options.len());
        assert!(q.options.iter().all(|o| o.key.len() == 1));
    }
}
