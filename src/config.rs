//! `~/.config/horde/config.toml`.
//!
//! horde runs with no config file at all; everything here has a default.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

use crate::proto::{Cmd, Dir, Rgb};
use crate::theme::{Theme, ThemeOverrides};

/// Where config, state, and the socket live.
///
/// Deliberately **not** `dirs::config_dir()`: on macOS that returns
/// `~/Library/Application Support`, which is wrong for a terminal tool — it puts a space in
/// the socket path and eats into the ~100 byte `AF_UNIX` limit. Terminal tools belong in
/// `~/.config`, which is also where tmux, zellij and herdr keep theirs.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HORDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("horde")
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("HORDE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir().join("horde.sock"))
}

pub fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

pub fn bus_log_path() -> PathBuf {
    config_dir().join("bus.jsonl")
}

pub fn tasks_path() -> PathBuf {
    config_dir().join("tasks.jsonl")
}

/// Scheduled rules. Its own log for the same reason the board has one: replayed on start, and
/// the record of what fired has to survive the restart it may have caused.
pub fn triggers_path() -> PathBuf {
    config_dir().join("triggers.jsonl")
}

/// Agent state changes and pane exits, for `horde digest`. Distinct from the bus and task
/// logs: each of the three owns facts the others do not.
pub fn journal_path() -> PathBuf {
    config_dir().join("events.jsonl")
}

pub fn log_path() -> PathBuf {
    config_dir().join("horde.log")
}

// ---------------------------------------------------------------------------
// Raw TOML shape
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    prefix: Option<String>,
    scrollback: Option<usize>,
    shell: Option<String>,
    #[serde(default)]
    theme: RawTheme,
    #[serde(default)]
    ui: RawUi,
    #[serde(default)]
    keys: HashMap<String, String>,
    #[serde(default)]
    agents: RawAgents,
    #[serde(default)]
    notifications: RawNotifications,
    #[serde(default)]
    triggers: RawTriggers,
    #[serde(default)]
    roles: Vec<RawRole>,
    #[serde(default)]
    handover: RawHandover,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    models: HashMap<String, RawModelProfile>,
}

/// The `[handover]` block.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHandover {
    #[serde(default)]
    warning: Vec<String>,
    #[serde(default)]
    exhausted: Vec<String>,
    profile: Option<String>,
    instruct: Option<String>,
    max_chain: Option<usize>,
}

/// One `[models.<name>]` block.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelProfile {
    cmd: String,
    #[serde(default)]
    order: Vec<String>,
    /// Screen text meaning "this model will not serve you any more".
    #[serde(default)]
    exhausted: Vec<String>,
    /// Typed into the pane to change model without restarting the agent.
    switch: Option<String>,
}

/// One `[[roles]]` block.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    name: String,
    color: Option<String>,
    glyph: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    name: Option<String>,
    #[serde(default)]
    custom: ThemeOverrides,
    /// Replacement colours for the project ramp, by position.
    ///
    /// Literal colours belong to a theme, not to a space: a space stores which slot it uses,
    /// so retinting here moves every project on that slot at once and none of them have to
    /// be told. Short lists are fine — only the slots you name are replaced.
    space_accents: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUi {
    sidebar: Option<bool>,
    sidebar_width: Option<u16>,
    bus: Option<bool>,
    bus_width: Option<u16>,
    pane_titles: Option<bool>,
    tab_bar: Option<bool>,
    status_bar: Option<bool>,
    animate: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgents {
    restore: Option<bool>,
    detection_lines: Option<usize>,
    /// Deliver a queued message even while the target is mid-stream.
    force_inject: Option<bool>,
    /// Tell an idle agent when work is waiting on the task board.
    task_nudge: Option<bool>,
    /// Live panes agents may have started between them.
    max_fleet: Option<usize>,
    /// Whether the shared task board accepts anything at all.
    board: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNotifications {
    delivery: Option<String>,
    command: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTriggers {
    unattended: Option<bool>,
    max_spawned: Option<usize>,
}

// ---------------------------------------------------------------------------
// Resolved config
// ---------------------------------------------------------------------------

/// A named list of models to work through, and the command that runs one.
///
/// The point of the list is that free models run out. `cmd` is a template containing `{model}`;
/// `order` is the sequence to try, best first. horde holds no catalogue of its own — this is the
/// user's list, and horde's only opinion about it is which entry an agent is currently on.
///
/// ```toml
/// [models.free]
/// cmd = "opencode --model openrouter/{model}"
/// order = ["qwen/qwen3-coder:free", "deepseek/deepseek-chat-v3.1:free"]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelProfile {
    /// Command template. `{model}` is replaced with an entry from `order`.
    pub cmd: String,
    /// Models to try, best first.
    pub order: Vec<String>,
    /// Screen text that means the current model is spent.
    ///
    /// horde cannot see an HTTP status — it reads panes. But a provider error is *rendered*
    /// into the pane by the agent, so the message is on screen. OpenRouter answers an exhausted
    /// free model with `429` and `"Rate limit exceeded"`, and opencode prints the message body,
    /// which is why the defaults below are what they are. Override when your agent words it
    /// differently; there is no way for horde to know in advance.
    pub exhausted: Vec<String>,
    /// What to type into the pane to move to the next model, with `{model}` substituted.
    ///
    /// Set, and switching keeps the agent's session — its plan and context survive, which a
    /// restart would throw away along with the rate limit. Unset, and horde reports that the
    /// model is spent and leaves it alone.
    pub switch: Option<String>,
}

/// Telling an agent to hand over while it still can.
///
/// The case this exists for: an agent on a metered plan is about to run out. Once it does it can
/// do nothing at all — not spawn a successor, not write a note, not answer a question. So the
/// only moment a handover can be arranged by the agent itself is *before*, and the only thing
/// that reliably notices is whatever is reading the screen. That is horde.
#[derive(Debug, Clone, Default)]
pub struct Handover {
    /// Screen text meaning "nearly out".
    pub warning: Vec<String>,
    /// Screen text meaning "out now, and nobody handed over".
    ///
    /// The net under `warning`. An agent that stopped mid-sentence never got the chance to write
    /// its own note, so horde spawns the successor itself and composes the brief from what it
    /// watched — which is less than the agent knew, and far more than nothing.
    pub exhausted: Vec<String>,
    /// How many successors a single lineage may have.
    ///
    /// A chain whose members keep dying would otherwise spawn forever. Small on purpose: if
    /// three agents in a row have run out, the answer is not a fourth.
    pub max_chain: usize,
    /// Model profile the successor should start on.
    pub profile: Option<String>,
    /// What the agent is told. `{name}` is its own name, `{profile}` the successor's profile.
    pub instruct: Option<String>,
}

/// What an agent is told when it is nearly out, if the config does not say otherwise.
///
/// Deliberately concrete. A vague nudge produces a vague handover, and the successor inherits
/// whatever ambiguity was left behind.
pub const DEFAULT_INSTRUCT: &str = "You are close to your usage limit and will stop being able to \
act shortly. Hand over now, while you still can: write what you are doing, what is done, what is \
half-done and what to be careful of to .horde/handoff-{name}.md, commit or stash your work so the \
tree is not left mid-edit, then run: horde spawn --profile {profile} --name {name}-next --worktree \
--brief \"You are taking over from {name}. Read .horde/handoff-{name}.md before changing anything.\"";

/// What horde assumes an exhausted model says, when a profile does not say otherwise.
pub const DEFAULT_EXHAUSTED: &[&str] =
    &["Rate limit exceeded", "rate_limit_exceeded", "429 Too Many Requests"];

impl ModelProfile {
    /// The command for the model at `index`, or `None` once the list is spent.
    ///
    /// Returning `None` rather than wrapping around is deliberate: a fleet that has exhausted
    /// every free model should stop and say so. Rotating forever turns "the free tier does not
    /// support this workload" into an agent that looks busy and achieves nothing.
    pub fn command(&self, index: usize) -> Option<String> {
        let model = self.order.get(index)?;
        Some(self.cmd.replace("{model}", model))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// In-app toast only.
    Horde,
    /// Toast plus a macOS notification.
    System,
    Off,
}

impl Notify {
    /// Whether horde may reach outside its own window at all. The alert path the daemon runs
    /// while nothing is attached is the one thing `off` has to silence completely — an alert
    /// arrives on your phone, where there is no toast to dismiss.
    pub fn reaches_out(&self) -> bool {
        *self != Notify::Off
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub prefix: Chord,
    pub scrollback: usize,
    pub shell: String,
    pub theme: Theme,
    pub sidebar: bool,
    pub sidebar_width: u16,
    pub bus: bool,
    pub bus_width: u16,
    pub pane_titles: bool,
    pub tab_bar: bool,
    pub status_bar: bool,
    pub animate: bool,
    pub restore_agents: bool,
    pub detection_lines: usize,
    pub force_inject: bool,
    /// Tell an idle agent when the board has work. Needs `board = true` to do anything.
    pub task_nudge: bool,
    /// Whether the shared task board is open.
    ///
    /// Separate from the bus on purpose. Messaging is agents talking to each other; the board is
    /// agents *taking work* nobody watched them take. Wanting the first without the second is a
    /// coherent position, and before this the only way to hold it was to hope nobody tried.
    pub board: bool,
    /// How many live panes agents may have started between them.
    ///
    /// Separate from `triggers.max_spawned`, which bounds what horde starts with nobody
    /// present. This bounds what an agent starts while you are sitting there, which is a
    /// different risk: not "is anyone watching" but "an agent in a loop opens panes until the
    /// machine gives up". A lead agent building a fleet is the intended use, so the number is
    /// a working team rather than a token allowance.
    pub max_fleet: usize,
    /// Whether triggers may fire at all. Off by default: acting with nobody watching is a
    /// different promise from running side by side, and has to be asked for.
    pub unattended: bool,
    /// How many agents horde may have running that it started itself.
    ///
    /// Small on purpose. This is the number of full-permission agents that can be working with
    /// nobody present, so the default is "enough to be useful, few enough to read the transcript
    /// of afterwards".
    pub max_spawned: usize,
    /// Extra environment handed to every pane.
    ///
    /// This is how a provider key reaches an agent. Inheriting it from the daemon's environment
    /// looks equivalent and is not: the daemon is `setsid`'d from whichever shell started it, so
    /// a key exported in `.bashrc` reaches it only when horde was started from an interactive
    /// shell — and a daemon started any other way gets a thin environment and an agent that
    /// cannot authenticate, with nothing on screen explaining why.
    ///
    /// **Values are secrets.** Nothing here may reach the log, the journal, `horde status`, or
    /// `state.json`. See `Pane::spawn`.
    pub env: HashMap<String, String>,
    /// Named model rotations, keyed by profile name. See `ModelProfile`.
    pub models: HashMap<String, ModelProfile>,
    /// Telling an agent to hand over before it runs out. See `Handover`.
    pub handover: Handover,
    pub notify: Notify,
    /// Program run when the daemon has something to tell you and nothing is attached. The
    /// summary arrives as `$1` and the full digest as JSON on stdin, which is what keeps
    /// Pushover, Telegram, ntfy and email out of horde and in a script you own.
    pub notify_command: Option<String>,
    /// Roles you have named, and how each one looks.
    ///
    /// Declaring a role styles it; it does not permit it. Any role name works whether or not
    /// it appears here — putting a config edit in front of a one-word label is not how
    /// anything else in horde behaves.
    pub roles: Vec<Role>,
    pub keys: Keymap,
}

/// A job you can give a pane, and how it is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Role {
    /// Already normalised, so `Reviewer` and `reviewer` are the same role.
    pub name: String,
    pub color: Rgb,
    /// One cell wide. Plain Unicode geometrics, no Nerd Font dependency — the same rule
    /// `AgentState::glyph` follows, and for the same reason: a replacement box is worse than
    /// no glyph at all.
    pub glyph: String,
}

/// The glyph an undeclared role is drawn with.
pub const ROLE_GLYPH: &str = "◆";

/// Canonical form of a role name: trimmed, lowercased, whitespace and underscores folded to
/// `-`, capped at 16 columns. `None` when nothing is left, which is how a role is cleared.
///
/// Roles are only worth having because they recur across projects, and `Reviewer` filed
/// separately from `reviewer` is exactly the failure that would stop them recurring.
/// Normalising here is cheaper than a whitelist and does not put a config file in front of a
/// label.
pub fn normalise_role(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut dash = false;
    for c in s.trim().chars() {
        if c.is_whitespace() || c == '_' || c == '-' {
            // Collapse runs, and never lead with one.
            dash = !out.is_empty();
            continue;
        }
        if dash {
            out.push('-');
            dash = false;
        }
        out.extend(c.to_lowercase());
        if out.chars().count() >= 16 {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The colour a role is drawn in: the one you declared, else one derived from its name.
///
/// Derived rather than random so the same role is the same colour in every project and
/// across restarts — which is the entire point of a role being a name rather than a note.
pub fn role_style(roles: &[Role], name: &str, theme: &Theme) -> (String, Rgb) {
    if let Some(r) = roles.iter().find(|r| r.name == name) {
        return (r.glyph.clone(), r.color);
    }
    let hash = name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    (ROLE_GLYPH.to_string(), theme.space_accent((hash % crate::theme::SPACE_ACCENTS as u32) as u8))
}

impl Default for Config {
    fn default() -> Self {
        Config {
            prefix: Chord::new(KeyModifiers::CONTROL, KeyCode::Char('b')),
            scrollback: 10_000,
            shell: crate::daemon::pane::default_shell(),
            theme: Theme::horde(),
            sidebar: true,
            sidebar_width: 24,
            bus: false,
            bus_width: 30,
            pane_titles: true,
            tab_bar: true,
            status_bar: true,
            animate: true,
            restore_agents: true,
            detection_lines: 40,
            force_inject: false,
            // Off by default: this is the half that acts without being asked, and it is worth
            // watching the board behave for a day before switching it on. Needs `board = true`.
            task_nudge: false,
            max_fleet: 6,
            board: true,
            unattended: false,
            max_spawned: 2,
            env: HashMap::new(),
            models: HashMap::new(),
            handover: Handover::default(),
            notify: Notify::Horde,
            notify_command: None,
            roles: Vec::new(),
            keys: Keymap::default(),
        }
    }
}

impl Config {
    pub fn load() -> (Config, Vec<String>) {
        Self::load_from(&config_dir().join("config.toml"))
    }

    /// Returns the config plus any non-fatal complaints, so a typo in one key doesn't
    /// prevent horde from starting.
    pub fn load_from(path: &Path) -> (Config, Vec<String>) {
        let mut warnings = Vec::new();
        let mut cfg = Config::default();

        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (cfg, warnings),
            Err(e) => {
                warnings.push(format!("could not read {}: {e}", path.display()));
                return (cfg, warnings);
            }
        };

        let raw: RawConfig = match toml::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("config.toml is invalid, using defaults: {e}"));
                return (cfg, warnings);
            }
        };

        if let Some(p) = &raw.prefix {
            match Chord::parse(p) {
                Ok(c) => cfg.prefix = c,
                Err(e) => warnings.push(format!("prefix: {e}")),
            }
        }
        if let Some(s) = raw.scrollback {
            cfg.scrollback = s.clamp(0, 1_000_000);
        }
        if let Some(s) = raw.shell {
            cfg.shell = s;
        }

        if let Some(name) = &raw.theme.name {
            match Theme::by_name(name) {
                Some(t) => cfg.theme = t,
                None => warnings.push(format!(
                    "unknown theme {name:?}; known themes: {}",
                    Theme::names().join(", ")
                )),
            }
        }
        cfg.theme.apply_overrides(&raw.theme.custom);
        if let Some(list) = &raw.theme.space_accents {
            if list.len() > crate::theme::SPACE_ACCENTS {
                warnings.push(format!(
                    "space_accents has {} entries; only the first {} are used",
                    list.len(),
                    crate::theme::SPACE_ACCENTS
                ));
            }
            for (i, s) in list.iter().take(crate::theme::SPACE_ACCENTS).enumerate() {
                match crate::theme::parse_color(s) {
                    Some(c) => cfg.theme.space_accent_overrides[i] = Some(c),
                    None => warnings.push(format!("space_accents[{i}]: bad color {s:?}")),
                }
            }
        }

        // Environment handed to every pane. Names are validated, values never inspected —
        // one of them is an API key, and the less code that touches it the better.
        for (k, v) in &raw.env {
            if k.is_empty() || k.contains('=') || k.contains('\0') {
                warnings.push(format!("env: {k:?} is not a usable variable name"));
                continue;
            }
            cfg.env.insert(k.clone(), v.clone());
        }

        // Model profiles. A profile with no models is a typo that would otherwise present as
        // "the agent silently refuses to start", so it is refused here where it can be explained.
        for (name, m) in &raw.models {
            if m.cmd.trim().is_empty() {
                warnings.push(format!("models.{name}: cmd is empty"));
                continue;
            }
            if m.order.is_empty() {
                warnings.push(format!("models.{name}: order lists no models"));
                continue;
            }
            if !m.cmd.contains("{model}") {
                warnings.push(format!(
                    "models.{name}: cmd has no {{model}} placeholder, so every entry in order \
                     would run the same command"
                ));
                continue;
            }
            let exhausted = if m.exhausted.is_empty() {
                DEFAULT_EXHAUSTED.iter().map(|s| s.to_string()).collect()
            } else {
                m.exhausted.clone()
            };
            if m.switch.as_ref().is_some_and(|c| !c.contains("{model}")) {
                warnings.push(format!(
                    "models.{name}: switch has no {{model}} placeholder, so every switch would \
                     ask for the same model"
                ));
                continue;
            }
            cfg.models.insert(
                name.clone(),
                ModelProfile {
                    cmd: m.cmd.clone(),
                    order: m.order.clone(),
                    exhausted,
                    switch: m.switch.clone(),
                },
            );
        }

        // Handover. A warning list with nothing to hand over *to* would fire and then have no
        // advice to give, so the profile is what makes the feature live.
        cfg.handover = Handover {
            warning: raw.handover.warning.clone(),
            exhausted: raw.handover.exhausted.clone(),
            profile: raw.handover.profile.clone(),
            instruct: raw.handover.instruct.clone(),
            max_chain: raw.handover.max_chain.unwrap_or(3),
        };
        if (!cfg.handover.warning.is_empty() || !cfg.handover.exhausted.is_empty())
            && cfg.handover.profile.is_none()
        {
            warnings.push(
                "handover is configured but handover.profile is not, so there is nothing to \
                 hand over to"
                    .to_string(),
            );
            cfg.handover.warning.clear();
            cfg.handover.exhausted.clear();
        }

        // Roles resolve after the theme, because an undeclared one derives its colour from
        // the project ramp and a declared one may name a palette colour.
        for (i, r) in raw.roles.iter().enumerate() {
            let Some(name) = normalise_role(&r.name) else {
                warnings.push(format!("roles[{i}]: empty name"));
                continue;
            };
            if cfg.roles.iter().any(|e| e.name == name) {
                warnings.push(format!("roles[{i}]: {name:?} is declared twice"));
                continue;
            }
            let color = match r.color.as_deref() {
                Some(s) => match crate::theme::parse_color(s) {
                    Some(c) => c,
                    None => {
                        warnings.push(format!("roles[{i}]: bad color {s:?}"));
                        role_style(&[], &name, &cfg.theme).1
                    }
                },
                None => role_style(&[], &name, &cfg.theme).1,
            };
            // A two-cell glyph would push every row that carries it one column wider than the
            // panel budgeted for, so it is refused rather than allowed to overflow.
            let glyph = match r.glyph.as_deref() {
                Some(g) if crate::client::ui::width(g) == 1 => g.to_string(),
                Some(g) => {
                    warnings.push(format!("roles[{i}]: glyph {g:?} is not one cell wide"));
                    ROLE_GLYPH.to_string()
                }
                None => ROLE_GLYPH.to_string(),
            };
            cfg.roles.push(Role { name, color, glyph });
        }

        let ui = raw.ui;
        cfg.sidebar = ui.sidebar.unwrap_or(cfg.sidebar);
        cfg.sidebar_width = ui.sidebar_width.unwrap_or(cfg.sidebar_width).clamp(14, 60);
        cfg.bus = ui.bus.unwrap_or(cfg.bus);
        cfg.bus_width = ui.bus_width.unwrap_or(cfg.bus_width).clamp(18, 70);
        cfg.pane_titles = ui.pane_titles.unwrap_or(cfg.pane_titles);
        cfg.tab_bar = ui.tab_bar.unwrap_or(cfg.tab_bar);
        cfg.status_bar = ui.status_bar.unwrap_or(cfg.status_bar);
        cfg.animate = ui.animate.unwrap_or(cfg.animate);

        cfg.restore_agents = raw.agents.restore.unwrap_or(cfg.restore_agents);
        cfg.detection_lines = raw.agents.detection_lines.unwrap_or(cfg.detection_lines).clamp(5, 200);
        cfg.force_inject = raw.agents.force_inject.unwrap_or(cfg.force_inject);
        cfg.task_nudge = raw.agents.task_nudge.unwrap_or(cfg.task_nudge);
        cfg.max_fleet = raw.agents.max_fleet.unwrap_or(cfg.max_fleet);
        cfg.board = raw.agents.board.unwrap_or(cfg.board);
        cfg.unattended = raw.triggers.unattended.unwrap_or(cfg.unattended);
        // Clamped rather than trusted: this bounds how many full-permission agents can run
        // unwatched, and a typo'd 200 should not be taken at face value.
        cfg.max_spawned = raw.triggers.max_spawned.unwrap_or(cfg.max_spawned).clamp(0, 16);

        if let Some(d) = &raw.notifications.delivery {
            cfg.notify = match d.as_str() {
                "horde" => Notify::Horde,
                "system" => Notify::System,
                "off" => Notify::Off,
                other => {
                    warnings.push(format!("unknown notification delivery {other:?}"));
                    cfg.notify
                }
            };
        }
        // Configuring a command is itself the opt-in, so it runs under `horde` delivery too —
        // `delivery` says where horde's *own* notifications go, and `off` still means silence.
        cfg.notify_command =
            raw.notifications.command.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());

        for (name, spec) in &raw.keys {
            match cfg.keys.rebind(name, spec) {
                Ok(()) => {}
                Err(e) => warnings.push(format!("keys.{name}: {e}")),
            }
        }

        (cfg, warnings)
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A single key plus modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: KeyModifiers,
    pub code: KeyCode,
}

impl Chord {
    pub fn new(mods: KeyModifiers, code: KeyCode) -> Self {
        // Shift is implicit in an uppercase char; keeping both would make `K` and
        // `shift+k` compare unequal even though terminals report them identically.
        let mods = match code {
            KeyCode::Char(c) if c.is_uppercase() => mods.difference(KeyModifiers::SHIFT),
            _ => mods,
        };
        Self { mods, code }
    }

    /// `ctrl+b`, `alt+shift+k`, `f1`, `enter`, `|`, `space`.
    pub fn parse(s: &str) -> Result<Chord> {
        let s = s.trim();
        if s.is_empty() {
            return Err(anyhow!("empty key spec"));
        }
        let mut mods = KeyModifiers::NONE;
        // Split on '+' but keep a literal trailing '+' as the key itself.
        let parts: Vec<&str> = if s == "+" {
            vec!["+"]
        } else {
            let mut v: Vec<&str> = s.split('+').collect();
            if v.last() == Some(&"") {
                v.pop();
                if let Some(last) = v.last_mut() {
                    if last.is_empty() {
                        *last = "+";
                    }
                }
                v.push("+");
                v.retain(|p| !p.is_empty());
            }
            v
        };

        let (key, mod_parts) = parts.split_last().ok_or_else(|| anyhow!("empty key spec"))?;
        for m in mod_parts {
            mods |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => KeyModifiers::CONTROL,
                "alt" | "opt" | "option" | "meta" | "m" => KeyModifiers::ALT,
                "shift" | "s" => KeyModifiers::SHIFT,
                "cmd" | "super" | "win" => KeyModifiers::SUPER,
                other => return Err(anyhow!("unknown modifier {other:?}")),
            };
        }

        let code = parse_keycode(key)?;
        Ok(Chord::new(mods, code))
    }

    pub fn describe(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push_str("ctrl+");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("alt+");
        }
        if self.mods.contains(KeyModifiers::SUPER) {
            s.push_str("cmd+");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("shift+");
        }
        s.push_str(&describe_keycode(self.code));
        s
    }
}

fn parse_keycode(k: &str) -> Result<KeyCode> {
    let lower = k.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "space" => KeyCode::Char(' '),
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "minus" | "dash" => KeyCode::Char('-'),
        "plus" => KeyCode::Char('+'),
        "pipe" | "bar" => KeyCode::Char('|'),
        "quote" => KeyCode::Char('"'),
        "percent" => KeyCode::Char('%'),
        "slash" => KeyCode::Char('/'),
        "question" => KeyCode::Char('?'),
        "comma" => KeyCode::Char(','),
        "period" | "dot" => KeyCode::Char('.'),
        "semicolon" => KeyCode::Char(';'),
        "colon" => KeyCode::Char(':'),
        _ => {
            if let Some(n) = lower.strip_prefix('f') {
                if let Ok(num) = n.parse::<u8>() {
                    if (1..=24).contains(&num) {
                        return Ok(KeyCode::F(num));
                    }
                }
            }
            // A single character stays case-sensitive so `K` can differ from `k`.
            let mut chars = k.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return Err(anyhow!("unknown key {k:?}")),
            }
        }
    })
}

fn describe_keycode(c: KeyCode) -> String {
    match c {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "del".into(),
        KeyCode::Insert => "ins".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pgup".into(),
        KeyCode::PageDown => "pgdn".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// What a key does. Most map straight through to a daemon command; the rest are handled
/// entirely inside the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Cmd(Cmd),
    Detach,
    Help,
    Palette,
    SpaceSwitcher,
    CopyMode,
    /// Open the rename prompt for the focused pane.
    RenamePane,
    /// Open the settings panel.
    Settings,
    /// Send the prefix key itself to the pane.
    SendPrefix,
    /// Give the sidebar the keyboard, so its list can be walked without the prefix.
    SidebarFocus,
    /// Pin or unpin the focused pane, from anywhere.
    TogglePin,
    /// Open the full-screen roster.
    Roster,
    /// Open the approval queue: every agent blocked on a decision, in one list.
    Approvals,
    /// Step the agent list's filter.
    CycleLens,
}

/// How a binding is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// Press the prefix, then this chord.
    Prefix(Chord),
    /// Modified chord that works without the prefix.
    Direct(Chord),
}

#[derive(Debug, Clone)]
pub struct Keymap {
    pub bindings: Vec<(Trigger, Action)>,
    /// Canonical action names, so `keys.<name>` rebinding can find them.
    names: HashMap<String, usize>,
}

impl Default for Keymap {
    fn default() -> Self {
        use Cmd::*;
        let d = |s: &str| Chord::parse(s).expect("built-in binding must parse");
        let table: Vec<(&str, Trigger, Action)> = vec![
            // Panes
            ("split_right", Trigger::Prefix(d("|")), Action::Cmd(SplitRight)),
            ("split_right_alt", Trigger::Prefix(d("%")), Action::Cmd(SplitRight)),
            ("split_down", Trigger::Prefix(d("-")), Action::Cmd(SplitDown)),
            ("split_down_alt", Trigger::Prefix(d("\"")), Action::Cmd(SplitDown)),
            ("close_pane", Trigger::Prefix(d("x")), Action::Cmd(ClosePane)),
            ("zoom", Trigger::Prefix(d("z")), Action::Cmd(ToggleZoom)),
            ("focus_left", Trigger::Prefix(d("h")), Action::Cmd(FocusDir(Dir::Left))),
            ("focus_down", Trigger::Prefix(d("j")), Action::Cmd(FocusDir(Dir::Down))),
            ("focus_up", Trigger::Prefix(d("k")), Action::Cmd(FocusDir(Dir::Up))),
            ("focus_right", Trigger::Prefix(d("l")), Action::Cmd(FocusDir(Dir::Right))),
            ("focus_left_arrow", Trigger::Prefix(d("left")), Action::Cmd(FocusDir(Dir::Left))),
            ("focus_down_arrow", Trigger::Prefix(d("down")), Action::Cmd(FocusDir(Dir::Down))),
            ("focus_up_arrow", Trigger::Prefix(d("up")), Action::Cmd(FocusDir(Dir::Up))),
            ("focus_right_arrow", Trigger::Prefix(d("right")), Action::Cmd(FocusDir(Dir::Right))),
            ("resize_left", Trigger::Prefix(d("H")), Action::Cmd(Resize { dir: Dir::Left, cells: 3 })),
            ("resize_down", Trigger::Prefix(d("J")), Action::Cmd(Resize { dir: Dir::Down, cells: 2 })),
            ("resize_up", Trigger::Prefix(d("K")), Action::Cmd(Resize { dir: Dir::Up, cells: 2 })),
            ("resize_right", Trigger::Prefix(d("L")), Action::Cmd(Resize { dir: Dir::Right, cells: 3 })),
            ("swap_left", Trigger::Prefix(d("ctrl+h")), Action::Cmd(SwapDir(Dir::Left))),
            ("swap_down", Trigger::Prefix(d("ctrl+j")), Action::Cmd(SwapDir(Dir::Down))),
            ("swap_up", Trigger::Prefix(d("ctrl+k")), Action::Cmd(SwapDir(Dir::Up))),
            ("swap_right", Trigger::Prefix(d("ctrl+l")), Action::Cmd(SwapDir(Dir::Right))),
            // Tabs
            ("new_tab", Trigger::Prefix(d("c")), Action::Cmd(NewTab)),
            ("next_tab", Trigger::Prefix(d("n")), Action::Cmd(NextTab)),
            ("prev_tab", Trigger::Prefix(d("p")), Action::Cmd(PrevTab)),
            ("close_tab", Trigger::Prefix(d("X")), Action::Cmd(CloseTab)),
            // Spaces
            ("new_space", Trigger::Prefix(d("S")), Action::Cmd(NewSpace { name: None })),
            ("next_space", Trigger::Prefix(d(")")), Action::Cmd(NextSpace)),
            ("prev_space", Trigger::Prefix(d("(")), Action::Cmd(PrevSpace)),
            ("space_switcher", Trigger::Prefix(d("s")), Action::SpaceSwitcher),
            // Panels and navigation
            ("toggle_sidebar", Trigger::Prefix(d("e")), Action::Cmd(ToggleSidebar)),
            ("sidebar_focus", Trigger::Prefix(d("E")), Action::SidebarFocus),
            ("toggle_pin", Trigger::Prefix(d("P")), Action::TogglePin),
            ("roster", Trigger::Prefix(d("o")), Action::Roster),
            // Next to `a`, which jumps to the next agent that needs you. Lower case goes to
            // one of them, upper case shows you all of them at once.
            ("approvals", Trigger::Prefix(d("A")), Action::Approvals),
            ("cycle_lens", Trigger::Prefix(d("f")), Action::CycleLens),
            ("toggle_bus", Trigger::Prefix(d("b")), Action::Cmd(ToggleBus)),
            ("jump_attention", Trigger::Prefix(d("a")), Action::Cmd(JumpAttention)),
            ("redraw", Trigger::Prefix(d("r")), Action::Cmd(Redraw)),
            ("digest", Trigger::Prefix(d("D")), Action::Cmd(RequestDigest)),
            ("palette", Trigger::Prefix(d("g")), Action::Palette),
            ("copy_mode", Trigger::Prefix(d("[")), Action::CopyMode),
            ("rename_pane", Trigger::Prefix(d(",")), Action::RenamePane),
            ("settings", Trigger::Prefix(d(".")), Action::Settings),
            ("help", Trigger::Prefix(d("?")), Action::Help),
            ("detach", Trigger::Prefix(d("d")), Action::Detach),
            ("send_prefix", Trigger::Prefix(d("ctrl+b")), Action::SendPrefix),
        ];

        let mut bindings = Vec::new();
        let mut names = HashMap::new();
        for (name, trigger, action) in table {
            names.insert(name.to_string(), bindings.len());
            bindings.push((trigger, action));
        }
        // prefix+1..9 jumps to a tab by position.
        for n in 1..=9usize {
            names.insert(format!("goto_tab_{n}"), bindings.len());
            bindings.push((
                Trigger::Prefix(Chord::new(
                    KeyModifiers::NONE,
                    KeyCode::Char(char::from_digit(n as u32, 10).unwrap()),
                )),
                Action::Cmd(GotoTab(n - 1)),
            ));
        }

        Keymap { bindings, names }
    }
}

impl Keymap {
    /// Point an existing action at a different key. `"prefix+x"` binds under the prefix,
    /// anything else binds directly.
    pub fn rebind(&mut self, name: &str, spec: &str) -> Result<()> {
        let idx = *self
            .names
            .get(name)
            .ok_or_else(|| anyhow!("unknown action (see `horde keys` for the list)"))?;

        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("none") || spec.is_empty() {
            self.bindings[idx].0 = Trigger::Direct(Chord::new(KeyModifiers::NONE, KeyCode::Null));
            return Ok(());
        }

        let trigger = match spec.strip_prefix("prefix+") {
            Some(rest) => Trigger::Prefix(Chord::parse(rest)?),
            None => {
                let chord = Chord::parse(spec)?;
                // An unmodified printable key bound directly would swallow ordinary typing.
                if chord.mods.is_empty() && matches!(chord.code, KeyCode::Char(c) if !c.is_control())
                {
                    return Err(anyhow!(
                        "{spec:?} would shadow normal typing; use prefix+{spec} or add a modifier"
                    ));
                }
                Trigger::Direct(chord)
            }
        };
        self.bindings[idx].0 = trigger;
        Ok(())
    }

    pub fn lookup(&self, trigger: &Trigger) -> Option<&Action> {
        self.bindings.iter().find(|(t, _)| t == trigger).map(|(_, a)| a)
    }

    /// Bindings paired with their canonical names, for the help overlay.
    pub fn described(&self) -> Vec<(String, Trigger, Action)> {
        let mut by_idx: Vec<(usize, String)> =
            self.names.iter().map(|(n, i)| (*i, n.clone())).collect();
        by_idx.sort();
        by_idx
            .into_iter()
            .map(|(i, name)| (name, self.bindings[i].0, self.bindings[i].1.clone()))
            .collect()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_named_keys() {
        assert_eq!(
            Chord::parse("ctrl+b").unwrap(),
            Chord::new(KeyModifiers::CONTROL, KeyCode::Char('b'))
        );
        assert_eq!(Chord::parse("f5").unwrap().code, KeyCode::F(5));
        assert_eq!(Chord::parse("enter").unwrap().code, KeyCode::Enter);
        assert_eq!(Chord::parse("space").unwrap().code, KeyCode::Char(' '));
        assert_eq!(Chord::parse("|").unwrap().code, KeyCode::Char('|'));
        assert_eq!(Chord::parse("+").unwrap().code, KeyCode::Char('+'));
        let c = Chord::parse("alt+shift+tab").unwrap();
        assert!(c.mods.contains(KeyModifiers::ALT) && c.mods.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn uppercase_char_absorbs_its_own_shift() {
        // Terminals report shift+k as an uppercase K, so the two must compare equal.
        assert_eq!(Chord::parse("K").unwrap(), Chord::parse("shift+K").unwrap());
        assert_ne!(Chord::parse("K").unwrap(), Chord::parse("k").unwrap());
    }

    #[test]
    fn rejects_nonsense_specs() {
        assert!(Chord::parse("").is_err());
        assert!(Chord::parse("hyper+x").is_err());
        assert!(Chord::parse("notakey").is_err());
        assert!(Chord::parse("f99").is_err());
    }

    #[test]
    fn describe_round_trips() {
        for s in ["ctrl+b", "alt+f4", "enter", "|", "left", "ctrl+alt+shift+x"] {
            let c = Chord::parse(s).unwrap();
            assert_eq!(Chord::parse(&c.describe()).unwrap(), c, "{s}");
        }
    }

    #[test]
    fn default_keymap_has_no_duplicate_triggers() {
        let km = Keymap::default();
        let mut seen = std::collections::HashSet::new();
        for (t, a) in &km.bindings {
            // split_right/% and split_down/" are intentional aliases to the same action.
            assert!(seen.insert(*t), "duplicate trigger {t:?} -> {a:?}");
        }
    }

    #[test]
    fn rebinding_moves_the_action() {
        let mut km = Keymap::default();
        km.rebind("zoom", "prefix+f").unwrap();
        assert_eq!(
            km.lookup(&Trigger::Prefix(Chord::parse("f").unwrap())),
            Some(&Action::Cmd(Cmd::ToggleZoom))
        );
        assert!(km.lookup(&Trigger::Prefix(Chord::parse("z").unwrap())).is_none());
    }

    #[test]
    fn rebinding_rejects_bare_printable_keys() {
        let mut km = Keymap::default();
        // Binding `q` directly would eat the letter q while typing in a pane.
        let err = km.rebind("detach", "q").unwrap_err().to_string();
        assert!(err.contains("shadow normal typing"), "{err}");
        // With a modifier it is fine.
        assert!(km.rebind("detach", "ctrl+alt+q").is_ok());
    }

    #[test]
    fn rebinding_an_unknown_action_is_an_error() {
        let mut km = Keymap::default();
        assert!(km.rebind("does_not_exist", "prefix+q").is_err());
    }

    #[test]
    fn missing_config_file_yields_defaults_without_warnings() {
        let (cfg, warnings) = Config::load_from(Path::new("/nonexistent/horde/config.toml"));
        assert!(warnings.is_empty());
        assert_eq!(cfg.theme.name, "horde");
        assert_eq!(cfg.prefix, Chord::parse("ctrl+b").unwrap());
    }

    fn write_tmp(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("horde-cfgtest-{name}.toml"));
        std::fs::write(&p, body).unwrap();
        p
    }

    /// The shipped example has to actually load.
    ///
    /// It exists to be copied, and it documents settings by using them — so a key renamed in
    /// `RawConfig` breaks it silently, and the first person to find out is whoever copied it.
    /// `deny_unknown_fields` means a stale key is a hard error here, which is the point.
    #[test]
    fn the_example_config_parses_with_no_warnings() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let (cfg, warnings) = Config::load_from(&path);
        assert!(warnings.is_empty(), "the example must be clean: {warnings:?}");

        // And the parts it exists for resolve, rather than merely parsing.
        let free = cfg.models.get("free").expect("the free profile");
        assert!(!free.order.is_empty(), "it lists models");
        assert!(free.command(0).is_some(), "and can build a command from them");
        assert!(!free.exhausted.is_empty(), "it says what a spent model looks like");
        assert_eq!(cfg.handover.profile.as_deref(), Some("free"), "handover points at it");
        assert!(!cfg.handover.exhausted.is_empty(), "and knows what running out looks like");

        // The example must never carry a secret. It is committed to a public repository, and the
        // whole design keeps keys in the agent's own credential store instead.
        assert!(cfg.env.is_empty(), "the example must not ship an [env] block");
    }

    #[test]
    fn env_and_model_profiles_are_read() {
        let p = write_tmp(
            "envmodels",
            r#"
[env]
OPENROUTER_API_KEY = "sk-or-test"
OPENCODE_CONFIG = "/home/me/.config/opencode/opencode.json"

[models.free]
cmd = "opencode --model openrouter/{model}"
order = ["qwen/qwen3-coder:free", "deepseek/deepseek-chat-v3.1:free"]
"#,
        );
        let (cfg, warnings) = Config::load_from(&p);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.env.get("OPENROUTER_API_KEY").unwrap(), "sk-or-test");
        let free = cfg.models.get("free").expect("the profile is there");
        assert_eq!(free.order.len(), 2);
        assert_eq!(
            free.command(0).unwrap(),
            "opencode --model openrouter/qwen/qwen3-coder:free"
        );
    }

    /// Each of these is a typo that would otherwise present as "the agent will not start",
    /// which is the hardest possible way to be told about a config mistake.
    #[test]
    fn a_broken_profile_is_refused_with_a_reason() {
        let p = write_tmp(
            "badmodels",
            r#"
[models.nocmd]
cmd = ""
order = ["a"]

[models.noorder]
cmd = "opencode --model {model}"
order = []

[models.noplaceholder]
cmd = "opencode --model openrouter/qwen"
order = ["a", "b"]
"#,
        );
        let (cfg, warnings) = Config::load_from(&p);
        assert!(cfg.models.is_empty(), "none of these should be usable");
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("cmd is empty")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("lists no models")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("{model}")), "{warnings:?}");
    }

    /// The list is finite on purpose: a fleet that has burned through every free model should
    /// stop and say so rather than loop back to the one that just refused it.
    #[test]
    fn a_spent_profile_offers_no_further_command() {
        let m = ModelProfile {
            cmd: "opencode --model openrouter/{model}".into(),
            order: vec!["a".into(), "b".into()],
            exhausted: Vec::new(),
            switch: None,
        };
        assert!(m.command(0).is_some());
        assert!(m.command(1).is_some());
        assert_eq!(m.command(2), None, "the list does not wrap");
    }

    #[test]
    fn parses_a_full_config() {
        // Doubled hashes: the body contains `"#ff0000"`, which would close an `r#"` literal.
        let p = write_tmp(
            "full",
            r##"
prefix = "ctrl+a"
scrollback = 500

[theme]
name = "gruvbox"

[theme.custom]
accent = "#ff0000"

[ui]
sidebar = false
sidebar_width = 30
bus = true

[keys]
zoom = "prefix+f"

[notifications]
delivery = "system"
command = "  ~/bin/horde-ping  "
"##,
        );
        let (cfg, warnings) = Config::load_from(&p);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.prefix, Chord::parse("ctrl+a").unwrap());
        assert_eq!(cfg.scrollback, 500);
        assert_eq!(cfg.theme.name, "gruvbox");
        assert_eq!(cfg.theme.ui.accent, crate::proto::Rgb::new(255, 0, 0));
        assert!(!cfg.sidebar);
        assert_eq!(cfg.sidebar_width, 30);
        assert!(cfg.bus);
        assert_eq!(cfg.notify, Notify::System);
        // Trimmed, because a stray space would be passed to `sh -c` and fail obscurely.
        assert_eq!(cfg.notify_command.as_deref(), Some("~/bin/horde-ping"));
        assert_eq!(
            cfg.keys.lookup(&Trigger::Prefix(Chord::parse("f").unwrap())),
            Some(&Action::Cmd(Cmd::ToggleZoom))
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn bad_values_warn_but_do_not_stop_startup() {
        let p = write_tmp(
            "bad",
            r#"
prefix = "hyper+q"
[theme]
name = "nope"
[keys]
zoom = "not a key"
bogus_action = "prefix+q"
"#,
        );
        let (cfg, warnings) = Config::load_from(&p);
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        // Every bad value falls back to its default rather than breaking the session.
        assert_eq!(cfg.prefix, Chord::parse("ctrl+b").unwrap());
        assert_eq!(cfg.theme.name, "horde");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults() {
        let p = write_tmp("broken", "this is not = = toml [[[");
        let (cfg, warnings) = Config::load_from(&p);
        assert_eq!(warnings.len(), 1);
        assert_eq!(cfg.theme.name, "horde");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn widths_are_clamped_to_something_usable() {
        let p = write_tmp("clamp", "[ui]\nsidebar_width = 2\nbus_width = 900\n");
        let (cfg, _) = Config::load_from(&p);
        assert_eq!(cfg.sidebar_width, 14);
        assert_eq!(cfg.bus_width, 70);
        let _ = std::fs::remove_file(p);
    }


    /// Roles are only worth having because they recur across projects, and `Reviewer` filed
    /// separately from `reviewer` is exactly the failure that would stop them recurring.
    #[test]
    fn role_names_are_normalised_so_one_job_is_one_role() {
        for (input, want) in [
            ("reviewer", "reviewer"),
            ("Reviewer", "reviewer"),
            ("  REVIEWER  ", "reviewer"),
            ("code reviewer", "code-reviewer"),
            ("code_reviewer", "code-reviewer"),
            ("code   reviewer", "code-reviewer"),
            ("code-reviewer", "code-reviewer"),
        ] {
            assert_eq!(normalise_role(input).as_deref(), Some(want), "{input:?}");
        }
    }

    /// Empty clears the role, which is the same contract `rename` uses for an empty name.
    #[test]
    fn an_empty_role_clears_rather_than_naming_nothing() {
        assert_eq!(normalise_role(""), None);
        assert_eq!(normalise_role("   "), None);
        assert_eq!(normalise_role(" _ - "), None);
    }

    /// A row budgets a fixed number of columns for the role, so the name cannot be unbounded.
    #[test]
    fn a_long_role_is_capped_rather_than_overflowing_a_row() {
        let r = normalise_role("an-absurdly-long-role-name-nobody-would-type").unwrap();
        assert!(r.chars().count() <= 16, "{r:?}");
    }

    /// Declaring a role styles it; it does not permit it. An undeclared one still works, and
    /// gets the same colour every time — which is the property that matters, because a role
    /// is a name you expect to recognise in another project tomorrow.
    ///
    /// Deliberately *not* asserting that two roles differ: there are six colours, so enough
    /// roles must collide. Distinctness is not on offer and pretending otherwise would make
    /// this test a hostage to the hash function.
    #[test]
    fn an_undeclared_role_still_renders_and_stays_the_same_colour() {
        let t = Theme::horde();
        let (glyph, color) = role_style(&[], "reviewer", &t);
        assert_eq!(glyph, ROLE_GLYPH);
        assert_eq!(role_style(&[], "reviewer", &t).1, color, "stable across calls");
        assert!(t.space_accents().contains(&color), "drawn from the palette, not invented");
    }

    #[test]
    fn a_declared_role_wins_over_the_derived_look() {
        let t = Theme::horde();
        let roles = vec![Role { name: "reviewer".into(), color: Rgb::new(1, 2, 3), glyph: "◈".into() }];
        assert_eq!(role_style(&roles, "reviewer", &t), ("◈".to_string(), Rgb::new(1, 2, 3)));
    }

    /// A two-cell glyph would push every row carrying it one column past what the panel
    /// budgeted, so it warns and falls back rather than being allowed through.
    #[test]
    fn a_wide_role_glyph_is_refused_without_stopping_startup() {
        let p = write_tmp("wide-glyph", "[[roles]]\nname = \"reviewer\"\nglyph = \"🚀\"\n");
        let (cfg, warnings) = Config::load_from(&p);
        assert_eq!(cfg.roles.len(), 1);
        assert_eq!(cfg.roles[0].glyph, ROLE_GLYPH, "fell back rather than overflowing");
        assert!(warnings.iter().any(|w| w.contains("one cell")), "{warnings:?}");
    }

    #[test]
    fn roles_and_space_accents_parse_from_config() {
        let p = write_tmp(
            "roles",
            "[theme]\nspace_accents = [\"#ff0000\", \"bad\"]\n\n\
             [[roles]]\nname = \"Code Reviewer\"\ncolor = \"#00ff00\"\nglyph = \"◈\"\n",
        );
        let (cfg, warnings) = Config::load_from(&p);
        assert_eq!(cfg.roles[0].name, "code-reviewer", "normalised on the way in");
        assert_eq!(cfg.roles[0].color, Rgb::new(0, 255, 0));
        assert_eq!(cfg.theme.space_accent(0), Rgb::new(255, 0, 0));
        assert!(warnings.iter().any(|w| w.contains("space_accents[1]")), "{warnings:?}");
        // A bad slot must not disturb the ones around it.
        assert_eq!(cfg.theme.space_accent(2), Theme::horde().space_accent(2));
    }

    #[test]
    fn a_role_declared_twice_warns_and_keeps_the_first() {
        let p = write_tmp(
            "dup-roles",
            "[[roles]]\nname = \"reviewer\"\nglyph = \"◈\"\n\n\
             [[roles]]\nname = \"Reviewer\"\nglyph = \"◆\"\n",
        );
        let (cfg, warnings) = Config::load_from(&p);
        assert_eq!(cfg.roles.len(), 1);
        assert_eq!(cfg.roles[0].glyph, "◈");
        assert!(warnings.iter().any(|w| w.contains("twice")), "{warnings:?}");
    }

    /// The documented example is the first config most people copy, and `deny_unknown_fields`
    /// means one stale key there costs the reader their *entire* config, not just that line.
    /// Adding a second `[theme]` table while writing these docs is how this test came to exist.
    #[test]
    fn the_documented_example_config_parses_without_warnings() {
        let doc = include_str!("../docs/configuration.md");
        let block = doc
            .split("```toml")
            .nth(1)
            .and_then(|b| b.split("```").next())
            .expect("configuration.md must carry a toml example");
        let p = write_tmp("documented", block);
        let (cfg, warnings) = Config::load_from(&p);
        assert!(warnings.is_empty(), "the documented example warns: {warnings:?}");
        // And it is actually being applied, not silently falling back to defaults.
        assert_eq!(cfg.scrollback, 10000);
        assert_eq!(cfg.roles.len(), 2, "{:?}", cfg.roles);
        assert_eq!(cfg.theme.space_accent(0), Rgb::new(0x79, 0xc0, 0xff));
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn config_lives_under_dot_config_not_apple_application_support() {
        // `dirs::config_dir()` returns ~/Library/Application Support on macOS, which would
        // put a space in the socket path and waste the AF_UNIX length budget.
        let dir = config_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with("/horde"), "{s}");
        assert!(!s.contains("Application Support"), "{s}");
        assert!(!s.contains(' '), "socket paths with spaces are asking for trouble: {s}");
    }

    #[test]
    fn socket_path_fits_the_os_limit_with_room_to_spare() {
        // Bind fails outright past ~104 bytes, so the default must be nowhere near it.
        assert!(socket_path().as_os_str().len() < 100, "{:?}", socket_path());
    }
}
