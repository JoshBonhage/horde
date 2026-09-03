//! The setup walkthrough: what horde asks before you start using it.
//!
//! Shown once, on a session that has never been set up, and reachable afterwards from
//! Settings → Agents. It exists because the alternative is discovering the questions by
//! hitting them — being told "no vault" the first time you try to write a note is not a
//! prompt, it is a wall.
//!
//! Three rules it holds itself to:
//!
//! - **Every step has an answer already chosen**, so the whole thing can be finished by
//!   pressing enter. A walkthrough you cannot get through without deciding something is a
//!   form, and nobody sets up a tool by filling in a form.
//! - **It only asks what an answer can change.** A step that collects a preference nothing
//!   reads is a walkthrough that lies politely. Which languages the editor highlights is a
//!   property of how the binary was built, so it is reported in Settings → About instead of
//!   being asked about here.
//! - **Finishing writes through [`crate::client::settings::write`]**, the same
//!   comment-preserving merge the settings page uses. It used to write the file itself and
//!   skip a config that already existed, which meant running it a second time — from the
//!   settings page it advertises — collected four answers and silently discarded them.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{color, fill, logo, put_line};
use crate::client::settings::{self, Value};
use crate::theme::Theme;

/// One question in the walkthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Where notes live.
    Vault,
    /// Whether an agent reports its own state, or horde guesses from the screen.
    Hooks,
    /// Whether horde may act while nobody is attached.
    Unattended,
    /// What was chosen, and where to change it.
    Done,
}

impl Step {
    pub fn all() -> [Step; 4] {
        [Step::Vault, Step::Hooks, Step::Unattended, Step::Done]
    }

    pub fn title(&self) -> &'static str {
        match self {
            Step::Vault => "Where your notes live",
            Step::Hooks => "How horde reads your agents",
            Step::Unattended => "Whether horde acts on its own",
            Step::Done => "Ready",
        }
    }

    /// The prose above the choices. Says what the decision *is*, not what the keys are.
    pub fn body(&self) -> &'static [&'static str] {
        match self {
            Step::Vault => &[
                "horde keeps notes as plain markdown files — the same files",
                "Obsidian reads, and any editor after that.",
                "",
                "One vault is always there, whatever project you are in, so a",
                "thought can be written from anywhere. A project can also keep",
                "notes of its own alongside its code, and those win when you",
                "are in it.",
            ],
            Step::Hooks => &[
                "horde shows what each agent is doing. Without help it reads",
                "that off the screen, and a narrow pane can hide the marker —",
                "a working agent then looks idle, and messages meant to wait",
                "are delivered mid-thought.",
                "",
                "Claude Code can report its own state instead. This adds",
                "horde's hooks to ~/.claude/settings.json — backed up first,",
                "other tools' hooks untouched, undone by `horde integration",
                "uninstall claude` — and installs the skill that teaches an",
                "agent to use the bus.",
            ],
            Step::Unattended => &[
                "horde can start and nudge agents while nobody is attached: on",
                "a schedule, or when a condition you write is true.",
                "",
                "Off by default, and deliberately. An agent that acts while you",
                "are asleep is useful exactly as far as you trust it to, so this",
                "is a switch you turn on rather than one you find already on.",
            ],
            Step::Done => &[
                "This is what horde is about to write:",
            ],
        }
    }
}

/// Whether Claude Code has ever run here.
///
/// Asked so the hooks step can default to skipping on a machine that does not have it: writing
/// hooks for a tool you do not use is clutter in someone else's config file, and a wizard that
/// does it because you pressed enter is a wizard you stop pressing enter through.
pub fn claude_present() -> bool {
    dirs::home_dir().map(|h| h.join(".claude").is_dir()).unwrap_or(false)
}

/// What the walkthrough has been told so far.
#[derive(Debug, Clone)]
pub struct Answers {
    pub vault: String,
    /// Install the Claude Code hooks and the horde skill on finishing.
    pub hooks: bool,
    pub unattended: bool,
    /// Which choice on the current step is highlighted.
    pub cursor: usize,
}

impl Default for Answers {
    fn default() -> Self {
        Answers {
            vault: dirs::home_dir()
                .unwrap_or_default()
                .join("notes")
                .to_string_lossy()
                .to_string(),
            hooks: claude_present(),
            unattended: false,
            cursor: 0,
        }
    }
}

impl Answers {
    /// The settings the walkthrough would write, as dotted keys.
    ///
    /// Keys rather than a block of TOML, because these go through the settings writer, which
    /// merges into whatever is already there and keeps its comments and ordering. The hooks
    /// answer is not here: it is not a setting, it is something to run.
    pub fn writes(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("vault.home", Value::Str(self.vault.clone())),
            ("triggers.unattended", Value::Bool(self.unattended)),
            // Last, and written whatever the answers were: it records that the walkthrough
            // happened, not what it decided. See `mark_done` for the skipped path.
            ("setup.done", Value::Bool(true)),
        ]
    }

    /// What the last step shows: the answers, in the order they were given.
    pub fn summary(&self) -> Vec<String> {
        vec![
            format!("  notes         {}", self.vault),
            format!(
                "  claude code   {}",
                if self.hooks {
                    "install hooks and the horde skill"
                } else if claude_present() {
                    "leave it to screen detection"
                } else {
                    "not installed here — nothing to do"
                }
            ),
            format!(
                "  unattended    {}",
                if self.unattended { "on — horde may act alone" } else { "off" }
            ),
        ]
    }

    /// Apply everything the walkthrough collected. Returns a line per thing that happened.
    ///
    /// Failures are returned rather than raised: three of four steps landing is worth saying,
    /// and a wizard that reports nothing because its last action failed leaves you guessing
    /// about the first three.
    pub fn apply(&self) -> Vec<Result<String, String>> {
        let mut out = Vec::new();
        for (key, value) in self.writes() {
            out.push(match settings::write(key, value) {
                Ok(()) => Ok(format!("saved {key}")),
                Err(e) => Err(format!("could not save {key}: {e:#}")),
            });
        }
        if self.hooks {
            // The reporting variant, because the printing one would put its four lines on top
            // of the frame this is drawn in and ratatui would leave them there.
            out.push(match crate::cli::integration::install_reporting("claude") {
                Ok(_) => Ok("installed the Claude Code hooks and the horde skill".into()),
                Err(e) => Err(format!("could not install the hooks: {e:#}")),
            });
        }
        out
    }
}

/// Record that the walkthrough has been offered, without applying any of it.
///
/// What `esc` does. Skipping used to change nothing at all, which meant it was not a skip: the
/// next launch found no answer recorded and asked again, and so did the one after that. Saying
/// "not now" once should be believed, and the walkthrough is still on the settings page.
pub fn mark_done() -> Result<(), String> {
    settings::write("setup.done", Value::Bool(true))
        .map_err(|e| format!("could not save setup.done: {e:#}"))
}

/// How many choices the step offers, for cursor bounds.
pub fn choices(step: Step, _a: &Answers) -> usize {
    match step {
        Step::Vault => 1,
        Step::Hooks => 2,
        Step::Unattended => 2,
        Step::Done => 0,
    }
}

/// The lines of the choice area.
pub fn choice_lines(step: Step, a: &Answers) -> Vec<String> {
    let radio = |on: bool| if on { "•" } else { " " };
    match step {
        Step::Vault => vec![format!("  {}", a.vault)],
        Step::Hooks => vec![
            format!("  ({}) install them — recommended", radio(a.hooks)),
            format!(
                "  ({}) {}",
                radio(!a.hooks),
                if claude_present() {
                    "not now — read the screen instead"
                } else {
                    "not now — Claude Code is not on this machine"
                }
            ),
        ],
        Step::Unattended => vec![
            format!("  ({}) leave it off", radio(!a.unattended)),
            format!("  ({}) let horde act on its own", radio(a.unattended)),
        ],
        Step::Done => {
            let mut out = a.summary();
            out.push(String::new());
            out.push("  All of it lives in config.toml and changes whenever you".into());
            out.push("  like — press `.` for settings, or edit the file directly.".into());
            out.push("  This walkthrough is under Settings → Agents.".into());
            out
        }
    }
}

/// Which choice the cursor should open on, so stepping back and forth does not change an answer.
pub fn cursor_for(step: Step, a: &Answers) -> usize {
    match step {
        Step::Hooks => usize::from(!a.hooks),
        Step::Unattended => usize::from(a.unattended),
        _ => 0,
    }
}

/// Move the answer to wherever the cursor is. On a radio step, moving *is* choosing — a second
/// keystroke to confirm what the highlight already shows is a step that only exists to be missed.
pub fn choose(step: Step, a: &mut Answers) {
    match step {
        Step::Hooks => a.hooks = a.cursor == 0,
        Step::Unattended => a.unattended = a.cursor == 1,
        _ => {}
    }
}

/// The key hints for a step. Only keys the step actually has.
pub fn hint(step: Step) -> &'static str {
    match step {
        Step::Vault => "type to change it   enter continue   esc skip setup",
        Step::Hooks | Step::Unattended => "↑↓ choose   enter continue   esc skip setup",
        Step::Done => "enter to finish",
    }
}

pub fn draw(buf: &mut Buffer, area: TRect, theme: &Theme, step: Step, a: &Answers) {
    fill(buf, area, theme.ui.bg);
    if area.height < 12 || area.width < 40 {
        return;
    }
    let w = area.width.min(72);
    let x = area.x + (area.width - w) / 2;
    let mut y = area.y + 1;

    if area.height > 22 {
        y += logo::draw(buf, x, y, w, 8, theme);
        y += 1;
    }

    let n = Step::all().iter().position(|s| *s == step).unwrap_or(0) + 1;
    put_line(
        buf,
        x,
        y,
        w,
        Line::from(vec![
            Span::styled(
                format!("{}  ", step.title()),
                Style::default()
                    .fg(color(theme.ui.accent))
                    .bg(color(theme.ui.bg))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{n} of {}", Step::all().len()),
                Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
            ),
        ]),
    );
    y += 2;

    for line in step.body() {
        put_line(
            buf,
            x,
            y,
            w,
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(color(theme.ui.text_dim)).bg(color(theme.ui.bg)),
            )),
        );
        y += 1;
    }
    y += 1;

    let selectable = choices(step, a);
    for (i, line) in choice_lines(step, a).iter().enumerate() {
        let selected = i == a.cursor && i < selectable;
        if selected {
            fill(buf, TRect { x, y, width: w, height: 1 }, theme.ui.selection);
        }
        put_line(
            buf,
            x,
            y,
            w,
            Line::from(Span::styled(
                line.clone(),
                Style::default()
                    .fg(color(if selected { theme.ui.text } else { theme.ui.text_dim }))
                    .bg(color(if selected { theme.ui.selection } else { theme.ui.bg })),
            )),
        );
        y += 1;
    }

    put_line(
        buf,
        x,
        area.y + area.height.saturating_sub(2),
        w,
        Line::from(Span::styled(
            hint(step).to_string(),
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
        )),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every step has to be answerable by pressing enter: a walkthrough you cannot get
    /// through without deciding something is a form, and nobody sets up a tool by filling in
    /// a form.
    #[test]
    fn every_step_has_a_default_already_chosen() {
        let a = Answers::default();
        assert!(a.vault.ends_with("notes"), "somewhere obvious: {}", a.vault);
        assert!(!a.unattended, "and acting alone starts off, because that is the safe side");

        for step in Step::all() {
            assert!(!step.body().is_empty(), "{step:?} explains itself");
            assert!(a.cursor < choices(step, &a).max(1), "{step:?} opens on a valid choice");
        }
    }

    /// The hints name keys the step has, and no others.
    ///
    /// The bug this pins: the dropped languages step offered "space toggles ↑↓ move" on a step
    /// with nothing to toggle and nothing to move between. A hint that describes a key which
    /// does nothing is worse than no hint — it reads as a broken program.
    #[test]
    fn a_step_only_offers_keys_it_has() {
        for step in Step::all() {
            let hint = hint(step);
            if choices(step, &Answers::default()) < 2 {
                assert!(!hint.contains("↑↓"), "{step:?} has nothing to move between: {hint}");
            }
            assert!(!hint.contains("space"), "{step:?}: no step toggles with space: {hint}");
        }
    }

    /// Moving the highlight is the whole gesture on a radio step, and coming back to a step
    /// must show what you chose rather than resetting it.
    #[test]
    fn moving_the_highlight_is_choosing_and_the_choice_sticks() {
        let mut a = Answers { hooks: false, unattended: false, ..Answers::default() };

        a.cursor = 1;
        choose(Step::Unattended, &mut a);
        assert!(a.unattended, "the second option is on");
        assert_eq!(cursor_for(Step::Unattended, &a), 1, "and returning opens on it");

        a.cursor = 0;
        choose(Step::Unattended, &mut a);
        assert!(!a.unattended, "and back again");
        assert_eq!(cursor_for(Step::Unattended, &a), 0);

        a.cursor = 0;
        choose(Step::Hooks, &mut a);
        assert!(a.hooks, "install is the first option on the hooks step");
        assert_eq!(cursor_for(Step::Hooks, &a), 0);
    }

    /// What it writes has to be config horde can actually read back — and it has to *merge*,
    /// because the second time anyone runs this there is already a file.
    #[test]
    fn finishing_merges_into_a_config_that_already_exists() {
        let dir = std::env::temp_dir().join(format!("horde-setup-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        // A config someone wrote by hand, comment and all.
        std::fs::write(
            &path,
            "# mine, do not reformat\n[theme]\nname = \"gruvbox\"\n\n[vault]\nhome = \"/old/notes\"\n",
        )
        .unwrap();

        let a = Answers { vault: "/tmp/my-notes".into(), unattended: true, ..Answers::default() };
        for (key, value) in a.writes() {
            settings::write_to(&path, key, value).expect("it writes");
        }

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine, do not reformat"), "comments survive: {text}");
        assert!(text.contains("gruvbox"), "and so does everything it was not asked about");
        assert!(!text.contains("/old/notes"), "the answer replaced the old value: {text}");

        let (cfg, warnings) = crate::config::Config::load_from(&path);
        assert!(warnings.is_empty(), "it must parse cleanly: {warnings:?}");
        assert_eq!(cfg.vault_home, std::path::PathBuf::from("/tmp/my-notes"));
        assert!(cfg.unattended);
        assert!(cfg.setup_done, "finishing records that it happened: {text}");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Skipping is a decision, and it used to be forgotten the instant it was made: `esc` wrote
    /// nothing, so the next launch found nothing recorded and asked all over again.
    #[test]
    fn skipping_is_remembered_and_applies_none_of_the_answers() {
        let dir = std::env::temp_dir().join(format!("horde-skip-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");

        settings::write_to(&path, "setup.done", Value::Bool(true)).expect("it writes");

        let (cfg, warnings) = crate::config::Config::load_from(&path);
        assert!(warnings.is_empty(), "it must parse cleanly: {warnings:?}");
        assert!(cfg.setup_done, "so it is not offered again");
        assert_eq!(
            cfg.vault_home,
            crate::config::Config::default().vault_home,
            "and nothing the walkthrough would have set was set"
        );
        assert!(!cfg.unattended, "least of all this one");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The last step reports the answers rather than promising something generic, so what is
    /// about to happen is readable before it happens.
    #[test]
    fn the_last_step_shows_what_was_chosen() {
        let a = Answers { vault: "/n".into(), hooks: true, unattended: true, ..Answers::default() };
        let text = choice_lines(Step::Done, &a).join("\n");
        assert!(text.contains("/n"), "{text}");
        assert!(text.contains("hooks"), "{text}");
        assert!(text.contains("horde may act alone"), "{text}");
        assert_eq!(choices(Step::Done, &a), 0, "and nothing left to choose");
    }
}
