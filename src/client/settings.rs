//! The settings page and the config writing behind it.
//!
//! Changes apply immediately and persist to `config.toml`. Writing goes through `toml_edit`
//! so a hand-maintained file keeps its comments, key order, and formatting — a settings
//! screen that silently reformats the file you edit by hand is worse than no settings
//! screen.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use toml_edit::{DocumentMut, Item, Table};

use crate::config::{Chord, Config, Notify, Trigger};
use crate::theme::Theme;

/// Left-hand nav of the settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Appearance,
    Keys,
    Agents,
    Notifications,
    Terminal,
    About,
}

impl Category {
    pub fn label(&self) -> &'static str {
        match self {
            Category::Appearance => "Appearance",
            Category::Keys => "Keybindings",
            Category::Agents => "Agents",
            Category::Notifications => "Notifications",
            Category::Terminal => "Terminal",
            Category::About => "About",
        }
    }

    pub fn all() -> &'static [Category] {
        &[
            Category::Appearance,
            Category::Keys,
            Category::Agents,
            Category::Notifications,
            Category::Terminal,
            Category::About,
        ]
    }
}

/// A setting that can be cycled or toggled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Theme,
    Sidebar,
    SidebarWidth,
    Bus,
    BusWidth,
    PaneTitles,
    Animate,
    Notifications,
    RestoreAgents,
    DetectionLines,
    ForceInject,
    TaskNudge,
    Scrollback,
}

/// Things the page can do beyond changing a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Open `config.toml` in `$EDITOR`, in a new pane.
    EditFile,
    /// Re-read `config.toml` from disk.
    Reload,
    /// Install the Claude Code lifecycle hooks.
    InstallClaudeHooks,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Setting(Field),
    /// A rebindable action, named by its canonical `keys.<name>`.
    Keybind(String),
    Action(Action),
    /// Not selectable; a value that can only be changed by editing the file.
    ReadOnly,
    Separator,
    /// Not selectable; explanatory text.
    Note,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub value: String,
    pub kind: Kind,
}

impl Row {
    pub fn selectable(&self) -> bool {
        matches!(self.kind, Kind::Setting(_) | Kind::Action(_) | Kind::Keybind(_))
    }
}

fn setting(label: &str, value: String, field: Field) -> Row {
    Row { label: label.into(), value, kind: Kind::Setting(field) }
}

fn note(text: &str) -> Row {
    Row { label: text.into(), value: String::new(), kind: Kind::Note }
}

fn read_only(label: &str, value: String) -> Row {
    Row { label: label.into(), value, kind: Kind::ReadOnly }
}

fn separator() -> Row {
    Row { label: String::new(), value: String::new(), kind: Kind::Separator }
}

/// Rows for one category, built fresh from the live config.
pub fn rows(cfg: &Config, cat: Category) -> Vec<Row> {
    match cat {
        Category::Appearance => vec![
            setting("Theme", cfg.theme.name.clone(), Field::Theme),
            note("horde · tokyo-night · catppuccin · gruvbox · terminal"),
            separator(),
            setting("Sidebar", onoff(cfg.sidebar), Field::Sidebar),
            setting("Sidebar width", cfg.sidebar_width.to_string(), Field::SidebarWidth),
            setting("Bus drawer", onoff(cfg.bus), Field::Bus),
            setting("Bus width", cfg.bus_width.to_string(), Field::BusWidth),
            separator(),
            setting("Pane titles", onoff(cfg.pane_titles), Field::PaneTitles),
            setting("Animations", onoff(cfg.animate), Field::Animate),
            note("Spinners for working agents; off is calmer over ssh."),
            separator(),
            Row {
                label: "Edit config.toml".into(),
                value: "opens in $EDITOR".into(),
                kind: Kind::Action(Action::EditFile),
            },
            Row {
                label: "Reload from disk".into(),
                value: String::new(),
                kind: Kind::Action(Action::Reload),
            },
        ],

        Category::Keys => {
            let mut v = vec![
                read_only("Prefix", cfg.prefix.describe()),
                note("Change the prefix by editing config.toml."),
                separator(),
            ];
            // Every rebindable action, in the order the keymap declares them.
            for (name, trigger, _) in cfg.keys.described() {
                v.push(Row {
                    label: name.replace('_', " "),
                    value: describe_trigger(&trigger, &cfg.prefix),
                    kind: Kind::Keybind(name),
                });
            }
            v
        }

        Category::Agents => vec![
            setting("Restore agents", onoff(cfg.restore_agents), Field::RestoreAgents),
            note("After a daemon restart, resume agents that reported a session id."),
            separator(),
            setting("Detection lines", cfg.detection_lines.to_string(), Field::DetectionLines),
            note("Rows of the live buffer that screen detection matches against."),
            separator(),
            setting("Force message delivery", onoff(cfg.force_inject), Field::ForceInject),
            note("Parked while the bus is reworked — nothing is delivered to force."),
            separator(),
            setting("Nudge idle agents", onoff(cfg.task_nudge), Field::TaskNudge),
            note("Parked while the board is reworked — this has no effect yet."),
            separator(),
            Row {
                label: "Install Claude Code hooks".into(),
                value: "authoritative state".into(),
                kind: Kind::Action(Action::InstallClaudeHooks),
            },
            note("Hooks beat screen detection; a narrow pane can hide the marker."),
        ],

        Category::Notifications => vec![
            setting(
                "Delivery",
                match cfg.notify {
                    Notify::Horde => "in-app toast".into(),
                    Notify::System => "toast + macOS".into(),
                    Notify::Off => "off".into(),
                },
                Field::Notifications,
            ),
            note("Raised when an agent becomes blocked or finishes unobserved."),
        ],

        Category::Terminal => vec![
            setting("Scrollback", format!("{} lines", cfg.scrollback), Field::Scrollback),
            note("Kept in memory per pane. Never written to disk."),
            separator(),
            read_only("Shell", cfg.shell.clone()),
            note("Set `shell` in config.toml to override $SHELL."),
        ],

        Category::About => vec![
            read_only("horde", env!("CARGO_PKG_VERSION").into()),
            read_only("protocol", crate::proto::PROTOCOL_VERSION.to_string()),
            separator(),
            read_only("config", config_file().display().to_string()),
            read_only("socket", crate::config::socket_path().display().to_string()),
            read_only("log", crate::config::log_path().display().to_string()),
            read_only("bus log", crate::config::bus_log_path().display().to_string()),
            separator(),
            note("The daemon outlives this client and survives rebuilds."),
            note("After rebuilding, run `horde stop` before reattaching."),
        ],
    }
}

/// How a binding reads in the settings list.
pub fn describe_trigger(t: &Trigger, prefix: &Chord) -> String {
    match t {
        Trigger::Prefix(c) => format!("{} {}", prefix.describe(), c.describe()),
        Trigger::Direct(c) => match c.code {
            // An unbound action shows as such rather than as a mystery key.
            crossterm::event::KeyCode::Null => "—".into(),
            _ => c.describe(),
        },
        // Spelled "leader" rather than resolved to `ctrl+space`, because that is how it is
        // written in config and how the which-key popup announces it.
        Trigger::Leader(s) => format!("leader {}", s.describe()),
    }
}

fn onoff(b: bool) -> String {
    if b {
        "on".into()
    } else {
        "off".into()
    }
}

/// Scrollback presets, so one key press moves through sensible sizes rather than by one.
const SCROLLBACK_STEPS: [usize; 5] = [1_000, 5_000, 10_000, 50_000, 200_000];

/// Change one setting in `cfg`, returning the `config.toml` key and its new value.
///
/// `delta` is +1 or -1 so every field can be stepped in both directions.
pub fn bump(cfg: &mut Config, field: Field, delta: i32) -> (String, Value) {
    match field {
        Field::Theme => {
            let names = Theme::names();
            let cur = names.iter().position(|n| *n == cfg.theme.name).unwrap_or(0);
            let next = wrap(cur, names.len(), delta);
            // Rebuilding from the palette drops any [theme.custom] overrides held in
            // memory; they are reapplied when the config is next loaded from disk.
            cfg.theme = Theme::by_name(names[next]).unwrap_or_else(Theme::horde);
            ("theme.name".into(), Value::Str(names[next].into()))
        }
        Field::Sidebar => {
            cfg.sidebar = !cfg.sidebar;
            ("ui.sidebar".into(), Value::Bool(cfg.sidebar))
        }
        Field::SidebarWidth => {
            cfg.sidebar_width = step_u16(cfg.sidebar_width, delta * 2, 14, 60);
            ("ui.sidebar_width".into(), Value::Int(cfg.sidebar_width as i64))
        }
        Field::Bus => {
            cfg.bus = !cfg.bus;
            ("ui.bus".into(), Value::Bool(cfg.bus))
        }
        Field::BusWidth => {
            cfg.bus_width = step_u16(cfg.bus_width, delta * 2, 18, 70);
            ("ui.bus_width".into(), Value::Int(cfg.bus_width as i64))
        }
        Field::PaneTitles => {
            cfg.pane_titles = !cfg.pane_titles;
            ("ui.pane_titles".into(), Value::Bool(cfg.pane_titles))
        }
        Field::Animate => {
            cfg.animate = !cfg.animate;
            ("ui.animate".into(), Value::Bool(cfg.animate))
        }
        Field::Notifications => {
            let order = [Notify::Horde, Notify::System, Notify::Off];
            let cur = order.iter().position(|n| *n == cfg.notify).unwrap_or(0);
            cfg.notify = order[wrap(cur, order.len(), delta)];
            let s = match cfg.notify {
                Notify::Horde => "horde",
                Notify::System => "system",
                Notify::Off => "off",
            };
            ("notifications.delivery".into(), Value::Str(s.into()))
        }
        Field::RestoreAgents => {
            cfg.restore_agents = !cfg.restore_agents;
            ("agents.restore".into(), Value::Bool(cfg.restore_agents))
        }
        Field::DetectionLines => {
            cfg.detection_lines =
                (cfg.detection_lines as i64 + delta as i64 * 10).clamp(5, 200) as usize;
            ("agents.detection_lines".into(), Value::Int(cfg.detection_lines as i64))
        }
        Field::TaskNudge => {
            cfg.task_nudge = !cfg.task_nudge;
            ("agents.task_nudge".into(), Value::Bool(cfg.task_nudge))
        }
        Field::ForceInject => {
            cfg.force_inject = !cfg.force_inject;
            ("agents.force_inject".into(), Value::Bool(cfg.force_inject))
        }
        Field::Scrollback => {
            let cur = SCROLLBACK_STEPS
                .iter()
                .position(|s| *s >= cfg.scrollback)
                .unwrap_or(SCROLLBACK_STEPS.len() - 1);
            cfg.scrollback = SCROLLBACK_STEPS[wrap(cur, SCROLLBACK_STEPS.len(), delta)];
            ("scrollback".into(), Value::Int(cfg.scrollback as i64))
        }
    }
}

/// Turn a captured chord into a binding for `action`.
///
/// A chord carrying ctrl/alt/cmd becomes a direct binding; a bare key becomes a prefix
/// binding, because binding a bare printable key directly would swallow ordinary typing.
pub fn rebind(cfg: &mut Config, action: &str, chord: Chord) -> Result<(String, Value)> {
    use crossterm::event::KeyModifiers;
    let direct = chord
        .mods
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    let spec = if direct { chord.describe() } else { format!("prefix+{}", chord.describe()) };
    let trigger = if direct { Trigger::Direct(chord) } else { Trigger::Prefix(chord) };

    // Refuse a chord already spoken for, rather than leaving two actions fighting over it.
    if let Some(other) = conflict(cfg, &trigger, action) {
        return Err(anyhow!("already bound to `{other}`"));
    }
    cfg.keys.rebind(action, &spec).with_context(|| format!("binding {spec}"))?;
    Ok((format!("keys.{action}"), Value::Str(spec)))
}

/// Action already using `trigger`, if any, ignoring `except`.
pub fn conflict(cfg: &Config, trigger: &Trigger, except: &str) -> Option<String> {
    cfg.keys
        .described()
        .into_iter()
        .find(|(name, t, _)| name != except && t == trigger)
        .map(|(name, _, _)| name)
}

/// Cycle an index, wrapping at both ends.
fn wrap(cur: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    ((cur as i32 + delta).rem_euclid(len as i32)) as usize
}

fn step_u16(cur: u16, delta: i32, min: u16, max: u16) -> u16 {
    (cur as i32 + delta).clamp(min as i32, max as i32) as u16
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(String),
}

pub fn config_file() -> PathBuf {
    crate::config::config_dir().join("config.toml")
}

/// Write one dotted key into the user's `config.toml`.
pub fn write(key: &str, value: Value) -> Result<()> {
    write_to(&config_file(), key, value)
}

/// Write one dotted key into `path`, creating the file and any parent table.
///
/// Takes the path explicitly rather than reading it from the environment, so it can be
/// tested without mutating process-global state.
pub fn write_to(path: &std::path::Path, key: &str, value: Value) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => template(),
    };
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("{} is not valid TOML; fix it by hand first", path.display()))?;

    set_path(&mut doc, key, value);

    // Write via a temp file then rename, so an interrupted write cannot truncate the config.
    let tmp = path.with_extension("toml.horde-tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Set `a` or `a.b`. Every key horde writes is at most two levels deep.
fn set_path(doc: &mut DocumentMut, key: &str, value: Value) {
    let item = match value {
        Value::Bool(b) => toml_edit::value(b),
        Value::Int(i) => toml_edit::value(i),
        Value::Str(s) => toml_edit::value(s),
    };
    let mut parts = key.split('.');
    let first = parts.next().unwrap_or(key);
    match parts.next() {
        None => doc[first] = item,
        Some(second) => {
            if !doc.as_table().contains_key(first) {
                let mut t = Table::new();
                // Implicit would omit the [header] and produce dotted keys instead.
                t.set_implicit(false);
                doc[first] = Item::Table(t);
            }
            doc[first][second] = item;
        }
    }
}

/// Starting point written when no config exists, so opening it in an editor is useful
/// rather than a blank buffer.
pub fn template() -> String {
    "\
# horde configuration. Everything here is optional.
# Run `horde keys` for the full list of rebindable action names.

prefix = \"ctrl+b\"
scrollback = 10000

[theme]
# horde · tokyo-night · catppuccin · gruvbox · terminal
name = \"horde\"

# [theme.custom]
# accent = \"#7ee2c0\"

[ui]
sidebar = true
sidebar_width = 24
bus = false
bus_width = 30
pane_titles = true
animate = true

[agents]
# Resume agents after a daemon restart, when a session id was reported.
restore = true
detection_lines = 40

[notifications]
# horde · system · off
delivery = \"horde\"

[keys]
# zoom = \"prefix+f\"
"
    .to_string()
}

/// `$EDITOR` if set, else something that certainly exists.
pub fn editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horde-settings-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn every_category_has_rows_and_a_selectable_one() {
        let cfg = Config::default();
        for cat in Category::all() {
            let rs = rows(&cfg, *cat);
            assert!(!rs.is_empty(), "{cat:?} is empty");
            assert!(!cat.label().is_empty());
            // About is deliberately informational; the rest must be usable.
            if *cat != Category::About {
                assert!(rs.iter().any(|r| r.selectable()), "{cat:?} has nothing to change");
            }
            for r in &rs {
                if !matches!(r.kind, Kind::Separator) {
                    assert!(!r.label.is_empty(), "{cat:?}: {r:?}");
                }
            }
        }
    }

    #[test]
    fn keybindings_category_lists_every_rebindable_action() {
        let cfg = Config::default();
        let rs = rows(&cfg, Category::Keys);
        let listed: Vec<String> = rs
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Keybind(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        let expected: Vec<String> =
            cfg.keys.described().into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(listed, expected);
        assert!(listed.contains(&"zoom".to_string()));
        // Each shows its current key.
        let zoom = rs.iter().find(|r| r.kind == Kind::Keybind("zoom".into())).unwrap();
        assert_eq!(zoom.value, "ctrl+b z");
    }

    #[test]
    fn about_page_reports_versions_and_paths() {
        let rs = rows(&Config::default(), Category::About);
        let labels: Vec<&str> = rs.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"horde"));
        assert!(labels.contains(&"socket"));
        assert!(labels.contains(&"config"));
    }

    #[test]
    fn rebinding_a_bare_key_becomes_a_prefix_binding() {
        let mut cfg = Config::default();
        // `v` rather than a key the default table already owns — rebinding onto a taken chord
        // is refused, which is a different behaviour and has its own test below.
        let chord = Chord::new(KeyModifiers::NONE, KeyCode::Char('v'));
        let (key, value) = rebind(&mut cfg, "zoom", chord).unwrap();
        assert_eq!(key, "keys.zoom");
        assert_eq!(value, Value::Str("prefix+v".into()));
        assert_eq!(cfg.keys.lookup(&Trigger::Prefix(chord)).is_some(), true);
    }

    #[test]
    fn rebinding_a_modified_key_becomes_a_direct_binding() {
        let mut cfg = Config::default();
        let chord = Chord::parse("ctrl+alt+z").unwrap();
        let (_, value) = rebind(&mut cfg, "zoom", chord).unwrap();
        assert_eq!(value, Value::Str("ctrl+alt+z".into()));
        assert!(cfg.keys.lookup(&Trigger::Direct(chord)).is_some());
    }

    #[test]
    fn rebinding_refuses_a_chord_another_action_already_owns() {
        let mut cfg = Config::default();
        // `x` is close_pane out of the box.
        let chord = Chord::new(KeyModifiers::NONE, KeyCode::Char('x'));
        let err = rebind(&mut cfg, "zoom", chord).unwrap_err().to_string();
        assert!(err.contains("close_pane"), "{err}");
        // And the original binding is untouched.
        assert_eq!(cfg.keys.lookup(&Trigger::Prefix(chord)).is_some(), true);
    }

    #[test]
    fn rebinding_an_action_to_its_own_key_is_allowed() {
        let mut cfg = Config::default();
        let chord = Chord::new(KeyModifiers::NONE, KeyCode::Char('z'));
        assert!(rebind(&mut cfg, "zoom", chord).is_ok(), "re-confirming a key must not conflict");
    }

    #[test]
    fn a_rebind_round_trips_through_the_config_loader() {
        let dir = tmpdir("rebind");
        let mut cfg = Config::default();
        let chord = Chord::parse("f5").unwrap();
        let (key, value) = rebind(&mut cfg, "zoom", chord).unwrap();
        write_to(&dir.join("config.toml"), &key, value).unwrap();

        let (loaded, warnings) = Config::load_from(&dir.join("config.toml"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(loaded.keys.lookup(&Trigger::Prefix(chord)).is_some());
    }

    #[test]
    fn unbound_actions_read_as_a_dash() {
        let mut cfg = Config::default();
        cfg.keys.rebind("zoom", "none").unwrap();
        let rs = rows(&cfg, Category::Keys);
        let zoom = rs.iter().find(|r| r.kind == Kind::Keybind("zoom".into())).unwrap();
        assert_eq!(zoom.value, "—");
    }

    #[test]
    fn theme_cycles_forward_and_back_through_every_palette() {
        let mut cfg = Config::default();
        let names = Theme::names();
        for expected in names.iter().skip(1).chain(names.iter().take(1)) {
            let (key, _) = bump(&mut cfg, Field::Theme, 1);
            assert_eq!(key, "theme.name");
            assert_eq!(&cfg.theme.name, expected);
        }
        assert_eq!(cfg.theme.name, names[0]);
        bump(&mut cfg, Field::Theme, -1);
        assert_eq!(&cfg.theme.name, names.last().unwrap());
    }

    #[test]
    fn toggles_flip_regardless_of_direction() {
        let mut cfg = Config::default();
        let before = cfg.sidebar;
        bump(&mut cfg, Field::Sidebar, 1);
        assert_eq!(cfg.sidebar, !before);
        bump(&mut cfg, Field::Sidebar, -1);
        assert_eq!(cfg.sidebar, before);
    }

    #[test]
    fn numeric_fields_step_and_clamp() {
        let mut cfg = Config::default();
        for _ in 0..100 {
            bump(&mut cfg, Field::SidebarWidth, 1);
        }
        assert_eq!(cfg.sidebar_width, 60);
        for _ in 0..100 {
            bump(&mut cfg, Field::SidebarWidth, -1);
        }
        assert_eq!(cfg.sidebar_width, 14);

        for _ in 0..100 {
            bump(&mut cfg, Field::DetectionLines, -1);
        }
        assert_eq!(cfg.detection_lines, 5);
    }

    #[test]
    fn scrollback_moves_through_presets_not_by_one() {
        let mut cfg = Config::default();
        assert_eq!(cfg.scrollback, 10_000);
        bump(&mut cfg, Field::Scrollback, 1);
        assert_eq!(cfg.scrollback, 50_000);
        bump(&mut cfg, Field::Scrollback, -1);
        assert_eq!(cfg.scrollback, 10_000);
    }

    #[test]
    fn writing_preserves_comments_and_unrelated_keys() {
        let dir = tmpdir("preserve");
        let path = dir.join("config.toml");
        let original = "\
# my careful comment
prefix = \"ctrl+a\"   # trailing note

[ui]
# keep the sidebar wide
sidebar_width = 30
";
        std::fs::write(&path, original).unwrap();
        write_to(&path, "ui.sidebar", Value::Bool(false)).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        // The whole point of toml_edit: comments and formatting survive.
        assert!(after.contains("# my careful comment"), "{after}");
        assert!(after.contains("# trailing note"), "{after}");
        assert!(after.contains("# keep the sidebar wide"), "{after}");
        assert!(after.contains("prefix = \"ctrl+a\""), "{after}");
        assert!(after.contains("sidebar_width = 30"), "{after}");
        assert!(after.contains("sidebar = false"), "{after}");

        let (cfg, warnings) = Config::load_from(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!cfg.sidebar);
        assert_eq!(cfg.sidebar_width, 30);
    }

    #[test]
    fn writing_creates_the_file_and_parent_table_when_absent() {
        let dir = tmpdir("create");
        let path = dir.join("config.toml");
        write_to(&path, "ui.bus", Value::Bool(true)).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("[ui]"), "{after}");
        assert!(after.contains("bus = true"), "{after}");
        let (cfg, warnings) = Config::load_from(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(cfg.bus);
    }

    #[test]
    fn a_top_level_key_writes_without_a_table() {
        let dir = tmpdir("toplevel");
        let path = dir.join("config.toml");
        write_to(&path, "scrollback", Value::Int(1234)).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("scrollback = 1234"));
        let (cfg, _) = Config::load_from(&path);
        assert_eq!(cfg.scrollback, 1234);
    }

    #[test]
    fn every_field_writes_a_key_the_loader_accepts() {
        // A settings change the loader rejects would silently revert on restart, so
        // round-trip all of them through the real parser.
        let dir = tmpdir("roundtrip");
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        for f in [
            Field::Theme,
            Field::Sidebar,
            Field::SidebarWidth,
            Field::Bus,
            Field::BusWidth,
            Field::PaneTitles,
            Field::Animate,
            Field::Notifications,
            Field::RestoreAgents,
            Field::DetectionLines,
            Field::ForceInject,
            Field::TaskNudge,
            Field::Scrollback,
        ] {
            let (key, value) = bump(&mut cfg, f, 1);
            write_to(&path, &key, value).unwrap();
        }
        let (loaded, warnings) = Config::load_from(&path);
        assert!(warnings.is_empty(), "round-trip produced warnings: {warnings:?}");

        assert_eq!(loaded.theme.name, cfg.theme.name);
        assert_eq!(loaded.sidebar, cfg.sidebar);
        assert_eq!(loaded.sidebar_width, cfg.sidebar_width);
        assert_eq!(loaded.bus, cfg.bus);
        assert_eq!(loaded.bus_width, cfg.bus_width);
        assert_eq!(loaded.pane_titles, cfg.pane_titles);
        assert_eq!(loaded.animate, cfg.animate);
        assert_eq!(loaded.notify, cfg.notify);
        assert_eq!(loaded.restore_agents, cfg.restore_agents);
        assert_eq!(loaded.detection_lines, cfg.detection_lines);
        assert_eq!(loaded.force_inject, cfg.force_inject);
        assert_eq!(loaded.scrollback, cfg.scrollback);
    }

    #[test]
    fn the_template_parses_cleanly() {
        let dir = tmpdir("template");
        let p = dir.join("config.toml");
        std::fs::write(&p, template()).unwrap();
        let (cfg, warnings) = Config::load_from(&p);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.theme.name, "horde");
        assert_eq!(cfg.scrollback, 10_000);
    }

    #[test]
    fn malformed_config_refuses_to_write_rather_than_clobbering() {
        let dir = tmpdir("broken");
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is [[[ not toml").unwrap();
        let err = write_to(&path, "ui.bus", Value::Bool(true)).unwrap_err().to_string();
        assert!(err.contains("not valid TOML"), "{err}");
        // The user's file is untouched, so nothing is lost.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is [[[ not toml");
    }
}
