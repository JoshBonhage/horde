//! The setup walkthrough: what horde asks before you start using it.
//!
//! Shown once, on a session that has never been set up, and reachable afterwards from the
//! settings page. It exists because the alternative is discovering the questions by hitting
//! them — being told "no vault" the first time you try to write a note is not a prompt, it
//! is a wall.
//!
//! Each step is one decision with a sensible default already chosen, so the whole thing can
//! be finished by pressing enter four times and changed later.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{color, fill, logo, put_line};
use crate::theme::Theme;

/// One question in the walkthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Where notes live.
    Vault,
    /// Which languages the editor should know about.
    Languages,
    /// Whether horde may act while nobody is attached.
    Unattended,
    /// What was chosen, and where to change it.
    Done,
}

impl Step {
    pub fn all() -> [Step; 4] {
        [Step::Vault, Step::Languages, Step::Unattended, Step::Done]
    }

    pub fn title(&self) -> &'static str {
        match self {
            Step::Vault => "Where your notes live",
            Step::Languages => "What the editor should know",
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
            Step::Languages => &[
                "The editor recognises a file by its extension and colours it",
                "accordingly. These are the languages this build understands:",
                "",
                // Filled in from the build itself below — asking you to choose
                // would be a question nothing could act on, since grammars are
                // compiled in rather than loaded.
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
                "That is everything horde needs to know.",
                "",
                "All of it lives in config.toml and changes whenever you like —",
                "press `.` for settings, or edit the file directly. This",
                "walkthrough is under settings too, if you want it again.",
            ],
        }
    }
}

/// What the walkthrough has been told so far.
#[derive(Debug, Clone)]
pub struct Answers {
    pub vault: String,
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
            unattended: false,
            cursor: 0,
        }
    }
}

impl Answers {
    /// The config the walkthrough would write.
    pub fn to_config(&self) -> String {
        let langs = crate::client::syntax::available();
        format!(
            "# Written by horde's setup walkthrough. Yours to edit.\n\n\
             [vault]\nhome = \"{}\"\n\n\
             # Languages this build highlights: {}\n\
             [triggers]\nunattended = {}\n",
            self.vault,
            if langs.is_empty() { "markdown only".to_string() } else { langs.join(", ") },
            self.unattended,
        )
    }
}

/// How many choices the step offers, for cursor bounds.
pub fn choices(step: Step, _a: &Answers) -> usize {
    match step {
        Step::Vault => 1,
        Step::Languages => 0,
        Step::Unattended => 2,
        Step::Done => 0,
    }
}

/// The lines of the choice area, and which are selectable.
pub fn choice_lines(step: Step, _a: &Answers) -> Vec<String> {
    match step {
        Step::Vault => vec![format!("  {}", _a.vault)],
        Step::Languages => {
            let langs = crate::client::syntax::available();
            let mut out: Vec<String> = if langs.is_empty() {
                vec!["  none — this build was made without language features".into()]
            } else {
                langs.chunks(3).map(|row| format!("  {}", row.join("   "))).collect()
            };
            out.push(String::new());
            out.push("  Grammars are compiled in, so this is a property of the".into());
            out.push("  binary rather than a setting. A smaller build is".into());
            out.push("  `--no-default-features`, plus the ones you want.".into());
            out
        }
        Step::Unattended => vec![
            format!("  ({}) leave it off", if _a.unattended { " " } else { "•" }),
            format!("  ({}) let horde act on its own", if _a.unattended { "•" } else { " " }),
        ],
        Step::Done => Vec::new(),
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

    for (i, line) in choice_lines(step, a).iter().enumerate() {
        let selected = i == a.cursor;
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

    let hint = match step {
        Step::Vault => "type to change it   enter continue   esc skip setup",
        Step::Languages => "space toggles   ↑↓ move   enter continue",
        Step::Unattended => "↑↓ choose   enter continue",
        Step::Done => "enter to finish",
    };
    put_line(
        buf,
        x,
        area.y + area.height.saturating_sub(2),
        w,
        Line::from(Span::styled(
            hint.to_string(),
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

    /// What it writes has to be config horde can actually read back.
    #[test]
    fn the_walkthrough_writes_config_that_parses() {
        let mut a = Answers::default();
        a.vault = "/tmp/my-notes".into();
        a.unattended = true;
        let text = a.to_config();
        assert!(text.contains("home = \"/tmp/my-notes\""), "{text}");
        assert!(text.contains("unattended = true"), "{text}");

        let path = std::env::temp_dir().join(format!("horde-setup-{}.toml", std::process::id()));
        std::fs::write(&path, &text).unwrap();
        let (cfg, warnings) = crate::config::Config::load_from(&path);
        assert!(warnings.is_empty(), "it must parse cleanly: {warnings:?}");
        assert_eq!(cfg.vault_home, std::path::PathBuf::from("/tmp/my-notes"));
        assert!(cfg.unattended);
        let _ = std::fs::remove_file(path);
    }

    /// The step reports rather than asks. Grammars are compiled in, so a question about
    /// which to enable would be one the answer could not act on — and a walkthrough that
    /// collects a preference nothing reads is a walkthrough that lies politely.
    #[test]
    fn the_languages_step_reports_what_the_build_actually_has() {
        let lines = choice_lines(Step::Languages, &Answers::default());
        let text = lines.join(" ");
        for lang in crate::client::syntax::available() {
            assert!(text.contains(lang), "{lang} is missing from {text:?}");
        }
        assert!(text.contains("compiled in"), "and it says why it is not a setting");
        assert_eq!(choices(Step::Languages, &Answers::default()), 0, "nothing to choose");
    }
}
