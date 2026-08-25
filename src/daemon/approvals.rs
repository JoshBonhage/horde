//! Answering a permission prompt on your behalf.
//!
//! An agent that stops to ask permission is stopped until someone presses a key. That is the
//! right default and it is the whole reason the approval queue exists. But an agent left
//! working overnight spends the night stopped on a question you would always have answered the
//! same way, and on a plan where prompts are frequent that is most of the night.
//!
//! # What this does, and what it deliberately does not
//!
//! It presses **the option the agent already selected** — the `❯` in its own menu — and only
//! when a rule you wrote says that prompt, in that project, with that label, may be answered.
//! It never decides *for* the agent; it defers to the choice the agent made for itself, which
//! is what a person pressing return on a highlighted default is doing anyway.
//!
//! That is a narrower promise than "approve everything". Claude Code already has
//! `--dangerously-skip-permissions` for that, and where an agent offers such a flag it is the
//! better tool: the agent knows which operation it is about to run, and horde is reading a
//! screen. What horde offers instead is the same courtesy for agents that have no such flag,
//! scoped per project and per role, revocable from one file, and written down afterwards.
//!
//! # The bar this has to clear
//!
//! [`super::question`] was built to a display standard — its own docs say a wrong parse costs
//! "a question you go and read in its own pane instead". Pressing a key raises that bar, so the
//! guards here are about certainty rather than policy:
//!
//! * The prompt must be **stable across two detection passes**. A screen still being drawn is
//!   the single likeliest way to read the wrong menu, and a prompt that is real is still there
//!   half a second later.
//! * The recommendation must be **unambiguous** — exactly one option marked. Two marks means
//!   the parse is wrong, because no menu recommends two things.
//! * The label must match a rule's `allow` list, so a recommendation of "Yes, and don't ask
//!   again" is not pressed on your behalf and does not permanently widen what the agent may do.
//! * Never for an agent horde started itself, and never without `triggers.unattended`.
//!
//! Everything answered is journalled as [`Kind::Fired`], which is also what the hourly ceiling
//! is counted from — the audit trail and the guard are the same fact, the way triggers do it.

use std::collections::HashMap;

use crate::config::{Approval, Config};
use crate::proto::{Choice, Event, NoticeLevel, PaneId, Question};

use super::journal::Kind;
use super::{now_millis, Engine};

/// A rolling hour, in millis.
const HOUR: u64 = 3_600_000;

/// Most prompts answered in any rolling hour, across every rule.
///
/// Lower than the trigger ceiling on purpose. A trigger firing starts work you asked for; this
/// answers a question you were meant to see, and a rule matching far more than expected should
/// stop and say so long before it has done it fifty times.
const MAX_PER_HOUR: usize = 8;

/// What a pane was last seen asking, so a prompt can be required to hold still.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// Question text and the key that was recommended, from the previous pass.
    last: HashMap<PaneId, (String, String)>,
    /// When the ceiling was last complained about, so it is said once an hour rather than
    /// every tick for as long as the condition lasts.
    capped_notice_at: u64,
}

/// The option the agent recommended, when exactly one is marked.
///
/// Two marks is a parse that has gone wrong rather than a menu with two defaults, and the safe
/// reading of a bad parse is to leave the question alone.
fn recommended(q: &Question) -> Option<&Choice> {
    let mut marked = q.options.iter().filter(|o| o.recommended);
    let first = marked.next()?;
    marked.next().is_none().then_some(first)
}

/// The rule that permits answering this question here, if any.
fn matching<'a>(
    rules: &'a [Approval],
    space: Option<&str>,
    role: Option<&str>,
    question: &str,
    label: &str,
) -> Option<&'a Approval> {
    let (question, label) = (question.to_lowercase(), label.to_lowercase());
    rules.iter().find(|r| {
        r.space.as_deref().is_none_or(|s| Some(s) == space)
            && r.role.as_deref().is_none_or(|s| Some(s) == role)
            && question.contains(&r.matches)
            // The question is matched loosely and the label exactly, and the asymmetry is the
            // point. A prompt names a file that changes every time, so it has to be a
            // substring. A label is a fixed set the agent chose from — and matching it loosely
            // means `allow = ["yes"]` also permits "Yes, and don't ask again", which is the one
            // answer this guard exists to withhold. Caught by a test, not by inspection.
            && r.allow.iter().any(|a| label == *a)
    })
}

/// Answer what may be answered. Called once per detection pass.
pub fn consider(eng: &mut Engine) -> Vec<Event> {
    let cfg: Config = eng.cfg.clone();
    if cfg.approvals.is_empty() || !cfg.unattended {
        // Nothing configured, or horde is not armed. Forget what was seen: a prompt that was
        // holding still while disarmed should not count toward its stability once armed.
        eng.approvals.last.clear();
        return Vec::new();
    }

    // What is being asked right now, per pane, before anything is written.
    let mut asking: HashMap<PaneId, (String, String, Choice, Option<String>, Option<String>)> =
        HashMap::new();
    for (id, pane) in &eng.session.panes {
        let Some(agent) = &pane.agent else { continue };
        if agent.state != crate::proto::AgentState::Blocked {
            continue;
        }
        // Never for an agent horde started itself. The same rule that stops a machine-started
        // agent making triggers: a loop with no human anywhere in it is the thing to prevent,
        // and an agent that can both ask and answer closes it.
        if pane.spawned_by.is_some() || pane.spawned_by_pane.is_some() {
            continue;
        }
        let Some(q) = super::question::extract(&pane.detection_snapshot(cfg.detection_lines))
        else {
            continue;
        };
        let Some(choice) = recommended(&q) else { continue };
        let space = eng.session.space(pane.space).map(|s| s.name.clone());
        asking.insert(
            *id,
            (q.text.clone(), choice.key.clone(), choice.clone(), space, pane.role.clone()),
        );
    }

    // Held still since the previous pass?
    let steady: Vec<PaneId> = asking
        .iter()
        .filter(|(id, (text, key, ..))| {
            eng.approvals.last.get(id).is_some_and(|(t, k)| t == text && k == key)
        })
        .map(|(id, _)| *id)
        .collect();
    eng.approvals.last =
        asking.iter().map(|(id, (t, k, ..))| (*id, (t.clone(), k.clone()))).collect();

    let mut events = Vec::new();
    let now = now_millis();
    let spent = eng.journal.since(now.saturating_sub(HOUR)).filter(|e| e.kind == Kind::Fired).count();
    let mut budget = MAX_PER_HOUR.saturating_sub(spent);

    for id in steady {
        let Some((text, _, choice, space, role)) = asking.get(&id) else { continue };
        let Some(rule) = matching(&cfg.approvals, space.as_deref(), role.as_deref(), text, &choice.label)
        else {
            continue;
        };

        if budget == 0 {
            // Refused, and visibly so. A rule that quietly stopped answering leaves an agent
            // stopped for a reason nobody can see.
            if now.saturating_sub(eng.approvals.capped_notice_at) >= HOUR {
                eng.approvals.capped_notice_at = now;
                events.push(Event::Notice {
                    level: NoticeLevel::Warn,
                    text: format!(
                        "{MAX_PER_HOUR} prompts answered this hour is the ceiling — the rest \
                         are waiting for you"
                    ),
                });
            }
            break;
        }

        let name = eng
            .session
            .panes
            .get(&id)
            .and_then(|p| p.agent.as_ref())
            .map(|a| a.name.clone())
            .unwrap_or_else(|| id.to_string());

        let bytes = choice.answer_bytes();
        let wrote = eng.session.panes.get_mut(&id).is_some_and(|p| p.write_input(&bytes).is_ok());
        if !wrote {
            super::log_line(&format!("could not answer {name}: the pty refused the keystroke"));
            continue;
        }
        budget -= 1;

        // Journalled as a firing: same record, same counter, same digest section as anything
        // else horde decided on its own. `matches` is included so the entry says which rule
        // did it, which is what makes a surprising answer traceable to the line that allowed it.
        let line = format!("answered {name}: {} — {:?} (rule {:?})", choice.key, choice.label, rule.matches);
        eng.journal.note(Kind::Fired, line.as_str());
        super::log_line(&line);
        events.push(Event::Notice {
            level: NoticeLevel::Info,
            text: format!("answered {name}: {}", choice.label),
        });
        // It is no longer asking, and the next pass must not read the settling screen as a
        // fresh prompt that has already held still once.
        eng.approvals.last.remove(&id);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, opts: &[(&str, &str, bool)]) -> Question {
        Question {
            text: text.into(),
            options: opts
                .iter()
                .map(|(k, l, r)| Choice {
                    key: (*k).into(),
                    label: (*l).into(),
                    recommended: *r,
                })
                .collect(),
        }
    }

    fn rule(matches: &str, allow: &[&str]) -> Approval {
        Approval {
            space: None,
            role: None,
            matches: matches.to_lowercase(),
            allow: allow.iter().map(|a| a.to_lowercase()).collect(),
        }
    }

    /// One mark is a recommendation. Two is a parse that has gone wrong, and the safe reading
    /// of a bad parse is to leave the question for a human.
    #[test]
    fn a_recommendation_is_only_read_when_exactly_one_option_is_marked() {
        let one = q("proceed?", &[("1", "Yes", true), ("2", "No", false)]);
        assert_eq!(recommended(&one).map(|c| c.key.as_str()), Some("1"));

        let none = q("proceed?", &[("1", "Yes", false), ("2", "No", false)]);
        assert!(recommended(&none).is_none(), "nothing marked is nothing to defer to");

        let two = q("proceed?", &[("1", "Yes", true), ("2", "No", true)]);
        assert!(two.options.iter().filter(|o| o.recommended).count() == 2);
        assert!(recommended(&two).is_none(), "two marks means the parse is wrong");
    }

    /// The `allow` list is the guard that matters. An agent recommending "don't ask again" is
    /// recommending a permanent widening of what it may do without asking, and pressing that
    /// on someone's behalf is a different decision from approving one edit.
    #[test]
    fn a_recommendation_outside_the_allow_list_is_left_alone() {
        let rules = [rule("do you want to make this edit", &["yes"])];
        let ask = "Do you want to make this edit to src/mux.rs?";

        assert!(matching(&rules, None, None, ask, "Yes").is_some());
        assert!(
            matching(&rules, None, None, ask, "Yes, and don't ask again").is_none(),
            "\"yes\" must not match \"yes, and don't ask again\""
        );
        assert!(matching(&rules, None, None, ask, "No").is_none());
    }

    /// Scoping is restrictive on both axes: a rule written for one project or one role does
    /// not answer for another.
    #[test]
    fn a_rule_only_answers_where_it_was_scoped() {
        let mut r = rule("proceed", &["yes"]);
        r.space = Some("horde".into());
        r.role = Some("builder".into());
        let rules = [r];

        assert!(matching(&rules, Some("horde"), Some("builder"), "proceed?", "Yes").is_some());
        assert!(matching(&rules, Some("other"), Some("builder"), "proceed?", "Yes").is_none());
        assert!(matching(&rules, Some("horde"), Some("reviewer"), "proceed?", "Yes").is_none());
        assert!(matching(&rules, None, None, "proceed?", "Yes").is_none(), "unscoped pane");
    }

    /// A rule names the prompts it answers. A question it does not name is one to read.
    #[test]
    fn an_unnamed_question_is_never_answered() {
        let rules = [rule("make this edit", &["yes"])];
        assert!(matching(&rules, None, None, "Run `rm -rf /` in the shell?", "Yes").is_none());
    }

}
