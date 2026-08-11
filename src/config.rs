//! `~/.config/horde/config.toml`.
//!
//! horde runs with no config file at all; everything here has a default.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

use crate::proto::{Cmd, Dir};
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    name: Option<String>,
    #[serde(default)]
    custom: ThemeOverrides,
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNotifications {
    delivery: Option<String>,
}

// ---------------------------------------------------------------------------
// Resolved config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// In-app toast only.
    Horde,
    /// Toast plus a macOS notification.
    System,
    Off,
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
    pub notify: Notify,
    pub keys: Keymap,
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
            notify: Notify::Horde,
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
            ("toggle_bus", Trigger::Prefix(d("b")), Action::Cmd(ToggleBus)),
            ("jump_attention", Trigger::Prefix(d("a")), Action::Cmd(JumpAttention)),
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
