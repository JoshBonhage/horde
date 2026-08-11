//! The settings panel and the config writing behind it.
//!
//! Changes apply immediately and persist to `config.toml`. Writing goes through `toml_edit`
//! so a hand-maintained file keeps its comments, key order, and formatting — a settings
//! screen that silently reformats the file you edit by hand is worse than no settings
//! screen.

use anyhow::{Context, Result};
use std::path::PathBuf;
use toml_edit::{DocumentMut, Item, Table};

use crate::config::{Config, Notify};
use crate::theme::Theme;

/// A setting that can be changed from the panel.
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
    Scrollback,
}

/// Things the panel can do beyond changing a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Open `config.toml` in `$EDITOR`, in a new pane.
    EditFile,
    /// Re-read `config.toml` from disk.
    Reload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Setting(Field),
    Action(Action),
    /// Not selectable; shows a value that can only be changed by editing the file.
    ReadOnly,
    Separator,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub value: String,
    pub kind: Kind,
}

impl Row {
    pub fn selectable(&self) -> bool {
        matches!(self.kind, Kind::Setting(_) | Kind::Action(_))
    }
}

/// The panel's contents, built fresh from the live config each frame.
pub fn rows(cfg: &Config) -> Vec<Row> {
    let mut v = Vec::new();
    let mut set = |label: &str, value: String, field: Field| {
        v.push(Row { label: label.into(), value, kind: Kind::Setting(field) })
    };

    set("Theme", cfg.theme.name.clone(), Field::Theme);
    set("Sidebar", onoff(cfg.sidebar), Field::Sidebar);
    set("Sidebar width", cfg.sidebar_width.to_string(), Field::SidebarWidth);
    set("Bus drawer", onoff(cfg.bus), Field::Bus);
    set("Bus width", cfg.bus_width.to_string(), Field::BusWidth);
    set("Pane titles", onoff(cfg.pane_titles), Field::PaneTitles);
    set("Animations", onoff(cfg.animate), Field::Animate);
    set(
        "Notifications",
        match cfg.notify {
            Notify::Horde => "in-app",
            Notify::System => "in-app + macOS",
            Notify::Off => "off",
        }
        .into(),
        Field::Notifications,
    );
    set("Restore agents", onoff(cfg.restore_agents), Field::RestoreAgents);
    set("Scrollback", format!("{} lines", cfg.scrollback), Field::Scrollback);

    v.push(Row { label: "Prefix".into(), value: cfg.prefix.describe(), kind: Kind::ReadOnly });
    v.push(Row { label: String::new(), value: String::new(), kind: Kind::Separator });
    v.push(Row {
        label: "Edit config.toml".into(),
        value: "opens in $EDITOR".into(),
        kind: Kind::Action(Action::EditFile),
    });
    v.push(Row {
        label: "Reload from disk".into(),
        value: String::new(),
        kind: Kind::Action(Action::Reload),
    });
    v
}

fn onoff(b: bool) -> String {
    if b { "on".into() } else { "off".into() }
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
            // Rebuilding from the palette discards any [theme.custom] overrides held in
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
    let text = match std::fs::read_to_string(&path) {
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
    std::fs::rename(&tmp, &path)?;
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
    format!(
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
    )
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

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horde-settings-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn every_row_has_a_label_except_separators() {
        let cfg = Config::default();
        for r in rows(&cfg) {
            if r.kind != Kind::Separator {
                assert!(!r.label.is_empty(), "{r:?}");
            }
        }
    }

    #[test]
    fn panel_offers_both_settings_and_actions() {
        let cfg = Config::default();
        let rs = rows(&cfg);
        assert!(rs.iter().any(|r| matches!(r.kind, Kind::Setting(Field::Theme))));
        assert!(rs.iter().any(|r| r.kind == Kind::Action(Action::EditFile)));
        assert!(rs.iter().any(|r| r.kind == Kind::Action(Action::Reload)));
        // The prefix is shown but not cycleable.
        assert!(rs.iter().any(|r| r.kind == Kind::ReadOnly && r.label == "Prefix"));
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
        // Full circle returns to the start.
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
    fn widths_step_and_clamp_to_usable_values() {
        let mut cfg = Config::default();
        for _ in 0..100 {
            bump(&mut cfg, Field::SidebarWidth, 1);
        }
        assert_eq!(cfg.sidebar_width, 60);
        for _ in 0..100 {
            bump(&mut cfg, Field::SidebarWidth, -1);
        }
        assert_eq!(cfg.sidebar_width, 14);
    }

    #[test]
    fn notifications_cycle_through_all_three_modes() {
        let mut cfg = Config::default();
        let mut seen = vec![cfg.notify];
        for _ in 0..2 {
            bump(&mut cfg, Field::Notifications, 1);
            seen.push(cfg.notify);
        }
        assert!(seen.contains(&Notify::Horde));
        assert!(seen.contains(&Notify::System));
        assert!(seen.contains(&Notify::Off));
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
        let original = "\
# my careful comment
prefix = \"ctrl+a\"   # trailing note

[ui]
# keep the sidebar wide
sidebar_width = 30
";
        std::fs::write(dir.join("config.toml"), original).unwrap();

        write_to(&dir.join("config.toml"), "ui.sidebar", Value::Bool(false)).unwrap();
        let after = std::fs::read_to_string(dir.join("config.toml")).unwrap();

        // The whole point of toml_edit: comments and formatting survive.
        assert!(after.contains("# my careful comment"), "{after}");
        assert!(after.contains("# trailing note"), "{after}");
        assert!(after.contains("# keep the sidebar wide"), "{after}");
        assert!(after.contains("prefix = \"ctrl+a\""), "{after}");
        assert!(after.contains("sidebar_width = 30"), "{after}");
        assert!(after.contains("sidebar = false"), "{after}");

        // And it round-trips back through the real parser.
        let (cfg, warnings) = Config::load_from(&dir.join("config.toml"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!cfg.sidebar);
        assert_eq!(cfg.sidebar_width, 30);
    }

    #[test]
    fn writing_creates_the_file_and_parent_table_when_absent() {
        let dir = tmpdir("create");

        write_to(&dir.join("config.toml"), "ui.bus", Value::Bool(true)).unwrap();
        let after = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(after.contains("[ui]"), "{after}");
        assert!(after.contains("bus = true"), "{after}");

        let (cfg, warnings) = Config::load_from(&dir.join("config.toml"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(cfg.bus);
    }

    #[test]
    fn a_top_level_key_writes_without_a_table() {
        let dir = tmpdir("toplevel");
        write_to(&dir.join("config.toml"), "scrollback", Value::Int(1234)).unwrap();
        let after = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(after.contains("scrollback = 1234"), "{after}");
        let (cfg, _) = Config::load_from(&dir.join("config.toml"));
        assert_eq!(cfg.scrollback, 1234);
    }

    #[test]
    fn every_field_writes_a_key_the_loader_accepts() {
        // A settings change that config.rs rejects would silently revert on restart, so
        // round-trip all of them through the real parser.
        let dir = tmpdir("roundtrip");
        let mut cfg = Config::default();
        let fields = [
            Field::Theme,
            Field::Sidebar,
            Field::SidebarWidth,
            Field::Bus,
            Field::BusWidth,
            Field::PaneTitles,
            Field::Animate,
            Field::Notifications,
            Field::RestoreAgents,
            Field::Scrollback,
        ];
        for f in fields {
            let (key, value) = bump(&mut cfg, f, 1);
            write_to(&dir.join("config.toml"), &key, value).unwrap();
        }
        let (loaded, warnings) = Config::load_from(&dir.join("config.toml"));
        assert!(warnings.is_empty(), "round-trip produced warnings: {warnings:?}");

        // The loaded config must match what the panel thinks it set.
        assert_eq!(loaded.theme.name, cfg.theme.name);
        assert_eq!(loaded.sidebar, cfg.sidebar);
        assert_eq!(loaded.sidebar_width, cfg.sidebar_width);
        assert_eq!(loaded.bus, cfg.bus);
        assert_eq!(loaded.bus_width, cfg.bus_width);
        assert_eq!(loaded.pane_titles, cfg.pane_titles);
        assert_eq!(loaded.animate, cfg.animate);
        assert_eq!(loaded.notify, cfg.notify);
        assert_eq!(loaded.restore_agents, cfg.restore_agents);
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
        std::fs::write(dir.join("config.toml"), "this is [[[ not toml").unwrap();
        let err = write_to(&dir.join("config.toml"), "ui.bus", Value::Bool(true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid TOML"), "{err}");
        // The user's file is untouched, so nothing is lost.
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).unwrap(),
            "this is [[[ not toml"
        );
    }
}
