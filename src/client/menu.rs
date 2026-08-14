//! Right-click context menus.
//!
//! What you get depends on what you clicked, so the menu is built from a target rather than
//! being one fixed list. Every entry is reachable another way — by key or by CLI — so the
//! menu is a discovery surface, not a second implementation.

use crate::proto::{Cmd, Dir, PaneId, Snapshot, SpaceId, TabId};

/// What was under the cursor when the button went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Pane(PaneId),
    /// A space row in the sidebar.
    Space(SpaceId),
    /// An agent row in the sidebar.
    Agent(PaneId),
    /// A tab in the tab bar.
    Tab(TabId),
    /// The bus drawer.
    Bus,
    /// Sidebar background, or anywhere with nothing specific under it.
    Root,
}

/// A text prompt opened from a menu.
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    RenamePane(PaneId),
    /// Label what a pane is for.
    SetRole(PaneId),
    RenameSpace(SpaceId),
    RenameTab(TabId),
    NewSpace,
    /// Send a bus message to an agent.
    SendTo(PaneId),
    /// Run a command in a new pane.
    RunCommand,
}

impl Prompt {
    pub fn title(&self) -> &'static str {
        match self {
            Prompt::RenamePane(_) => "rename pane",
            Prompt::SetRole(_) => "role for this pane",
            Prompt::RenameSpace(_) => "rename space",
            Prompt::RenameTab(_) => "rename tab",
            Prompt::NewSpace => "new space name",
            Prompt::SendTo(_) => "send message",
            Prompt::RunCommand => "run in new pane",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            Prompt::SendTo(_) => "enter sends · esc cancels",
            Prompt::RunCommand => "enter runs · esc cancels",
            _ => "enter saves · esc cancels · empty clears",
        }
    }
}

/// What activating an entry does.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Forward a command to the daemon.
    Cmd(Cmd),
    /// Open a text prompt.
    Prompt(Prompt),
    /// Replace the menu with a submenu.
    Submenu(Sub),
    /// Open the settings page.
    Settings,
    Help,
    /// Copy the pane's visible text to the clipboard.
    CopyPane(PaneId),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    Layout,
    Spawn,
    /// Retint a space. Carries the id because the submenu outlives the click that opened it.
    Accent(SpaceId),
    Role(PaneId),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: String,
    /// Key equivalent, shown right-aligned so the menu teaches the keyboard path.
    pub hint: String,
    pub act: Act,
    /// Overrides the label's colour. Used where the colour *is* the content — a colour
    /// picker that does not show colours is asking you to remember which slot was which.
    pub color: Option<crate::proto::Rgb>,
}

impl Item {
    fn new(label: &str, hint: &str, act: Act) -> Item {
        Item { label: label.into(), hint: hint.into(), act, color: None }
    }

    fn swatch(label: &str, act: Act, color: crate::proto::Rgb) -> Item {
        Item { label: label.into(), hint: String::new(), act, color: Some(color) }
    }

    pub fn separator() -> Item {
        Item { label: String::new(), hint: String::new(), act: Act::Close, color: None }
    }

    pub fn is_separator(&self) -> bool {
        self.label.is_empty() && self.act == Act::Close
    }
}

/// One open menu level. A submenu pushes a new level so `esc` can step back out.
#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    pub title: String,
    pub items: Vec<Item>,
    pub sel: usize,
}

impl Level {
    pub fn new(title: &str, items: Vec<Item>) -> Level {
        let sel = items.iter().position(|i| !i.is_separator()).unwrap_or(0);
        Level { title: title.into(), items, sel }
    }

    /// Move the selection, skipping separators and stopping at the ends.
    pub fn step(&mut self, delta: i32) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        let mut i = self.sel as i32;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n as i32);
            if !self.items[i as usize].is_separator() {
                self.sel = i as usize;
                return;
            }
        }
    }

    pub fn selected(&self) -> Option<&Item> {
        self.items.get(self.sel).filter(|i| !i.is_separator())
    }
}

/// Build the menu for a target, using the snapshot for names and state.
pub fn build(target: Target, snap: &Snapshot, prefix: &str) -> Level {
    let k = |s: &str| format!("{prefix} {s}");
    match target {
        Target::Pane(pane) | Target::Agent(pane) => {
            let info = snap.panes.iter().find(|p| p.id == pane);
            let title = info.map(|p| p.title.clone()).unwrap_or_else(|| format!("pane {pane}"));
            let is_agent = info.and_then(|p| p.agent.as_ref()).is_some();
            let zoomed = snap.view.zoom == Some(pane);

            let mut items = vec![
                Item::new("Split right", &k("|"), Act::Cmd(Cmd::SplitRight)),
                Item::new("Split down", &k("-"), Act::Cmd(Cmd::SplitDown)),
                Item::new("Start agent here…", "", Act::Submenu(Sub::Spawn)),
                Item::new("Run command…", "", Act::Prompt(Prompt::RunCommand)),
                Item::separator(),
                Item::new(
                    if zoomed { "Unzoom" } else { "Zoom" },
                    &k("z"),
                    Act::Cmd(Cmd::ToggleZoom),
                ),
                Item::new("Rename…", &k(","), Act::Prompt(Prompt::RenamePane(pane))),
                Item::new("Role", "", Act::Submenu(Sub::Role(pane))),
                Item::new(
                    if info.is_some_and(|p| p.pinned) { "Unpin" } else { "Pin to top" },
                    "",
                    Act::Cmd(Cmd::TogglePanePinned(pane)),
                ),
                Item::new("Copy visible text", "", Act::CopyPane(pane)),
            ];
            // Messaging only makes sense when something is listening.
            if is_agent {
                items.push(Item::new("Send message…", "", Act::Prompt(Prompt::SendTo(pane))));
            }
            items.extend([
                Item::separator(),
                Item::new("Layout", "", Act::Submenu(Sub::Layout)),
                Item::new("New tab", &k("c"), Act::Cmd(Cmd::NewTab)),
                Item::separator(),
                Item::new("Close pane", &k("x"), Act::Cmd(Cmd::ClosePane)),
                Item::separator(),
                Item::new("Settings…", &k("."), Act::Settings),
                Item::new("Keys", &k("?"), Act::Help),
            ]);
            Level::new(&title, items)
        }

        Target::Space(space) => {
            let info = snap.spaces.iter().find(|s| s.id == space);
            let name = info.map(|s| s.name.clone()).unwrap_or_else(|| "space".into());
            let collapsed = info.is_some_and(|s| s.collapsed);
            Level::new(
                &name,
                vec![
                    Item::new("Focus", "", Act::Cmd(Cmd::FocusSpace(space))),
                    Item::new("New tab here", "", Act::Cmd(Cmd::NewTabIn(space))),
                    Item::new("Rename…", "", Act::Prompt(Prompt::RenameSpace(space))),
                    Item::new("Colour", "", Act::Submenu(Sub::Accent(space))),
                    Item::new(
                        if collapsed { "Expand" } else { "Collapse" },
                        "",
                        Act::Cmd(Cmd::ToggleSpaceCollapsed(space)),
                    ),
                    Item::separator(),
                    Item::new("New space…", &k("S"), Act::Prompt(Prompt::NewSpace)),
                    Item::separator(),
                    // Closing a space kills every process in it, so it sits alone at the
                    // bottom, away from anything you might click by accident.
                    Item::new("Close space", "", Act::Cmd(Cmd::CloseSpace(space))),
                    Item::separator(),
                    Item::new("Settings…", &k("."), Act::Settings),
                ],
            )
        }

        Target::Tab(tab) => {
            let name = snap
                .tabs
                .iter()
                .find(|t| t.id == tab)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| "tab".into());
            Level::new(
                &name,
                vec![
                    Item::new("Focus", "", Act::Cmd(Cmd::FocusTab(tab))),
                    Item::new("Rename…", "", Act::Prompt(Prompt::RenameTab(tab))),
                    Item::new("Layout", "", Act::Submenu(Sub::Layout)),
                    Item::separator(),
                    Item::new("New tab", &k("c"), Act::Cmd(Cmd::NewTab)),
                    Item::new("Close tab", &k("X"), Act::Cmd(Cmd::CloseTab)),
                    Item::separator(),
                    Item::new("Settings…", &k("."), Act::Settings),
                ],
            )
        }

        Target::Bus => Level::new(
            "bus",
            vec![
                Item::new("Hide drawer", &k("b"), Act::Cmd(Cmd::ToggleBus)),
                Item::separator(),
                Item::new("Settings…", &k("."), Act::Settings),
            ],
        ),

        Target::Root => Level::new(
            "horde",
            vec![
                Item::new("New space…", &k("S"), Act::Prompt(Prompt::NewSpace)),
                Item::new("New tab", &k("c"), Act::Cmd(Cmd::NewTab)),
                Item::new("Start agent…", "", Act::Submenu(Sub::Spawn)),
                Item::separator(),
                Item::new("Layout", "", Act::Submenu(Sub::Layout)),
                Item::new("Toggle sidebar", &k("e"), Act::Cmd(Cmd::ToggleSidebar)),
                Item::new("Toggle bus drawer", &k("b"), Act::Cmd(Cmd::ToggleBus)),
                Item::new("Next agent needing you", &k("a"), Act::Cmd(Cmd::JumpAttention)),
                Item::new("What happened while away", &k("D"), Act::Cmd(Cmd::RequestDigest)),
                Item::separator(),
                Item::new("Settings…", &k("."), Act::Settings),
                Item::new("Keys", &k("?"), Act::Help),
            ],
        ),
    }
}

/// Contents of a submenu.
pub fn submenu(sub: Sub, cfg: &crate::config::Config) -> Level {
    match sub {
        // Swatches rather than slot numbers — a colour picker that does not show colours is
        // asking you to remember which number was which.
        Sub::Accent(space) => Level::new(
            "colour",
            cfg.theme
                .space_accents()
                .into_iter()
                .enumerate()
                .map(|(slot, c)| {
                    Item::swatch(
                        "██████",
                        Act::Cmd(Cmd::SetSpaceAccent { space, slot: Some(slot as u8) }),
                        c,
                    )
                })
                .collect(),
        ),
        Sub::Role(pane) => Level::new(
            "role",
            cfg.roles
                .iter()
                .map(|r| {
                    Item::swatch(
                        &format!("{} {}", r.glyph, r.name),
                        Act::Cmd(Cmd::SetPaneRole { pane, role: r.name.clone() }),
                        r.color,
                    )
                })
                .chain([
                    Item::new("Other…", "", Act::Prompt(Prompt::SetRole(pane))),
                    Item::separator(),
                    Item::new(
                        "Clear role",
                        "",
                        Act::Cmd(Cmd::SetPaneRole { pane, role: String::new() }),
                    ),
                ])
                .collect(),
        ),
        Sub::Layout => Level::new(
            "layout",
            vec![
                Item::new("Solo", "1 pane", Act::Cmd(preset("solo"))),
                Item::new("Duo", "2 side by side", Act::Cmd(preset("duo"))),
                Item::new("Trio", "1 left, 2 right", Act::Cmd(preset("trio"))),
                Item::new("Dev", "main + logs + side", Act::Cmd(preset("dev"))),
                Item::new("Quad", "2x2", Act::Cmd(preset("quad"))),
            ],
        ),
        Sub::Spawn => Level::new(
            "start agent",
            AGENTS
                .iter()
                .map(|(label, cmd)| {
                    Item::new(
                        label,
                        cmd,
                        Act::Cmd(Cmd::SpawnAgent {
                            cmd: cmd.to_string(),
                            name: None,
                            split: Some(Dir::Right),
                        }),
                    )
                })
                .chain(std::iter::once(Item::new(
                    "Other…",
                    "",
                    Act::Prompt(Prompt::RunCommand),
                )))
                .collect(),
        ),
    }
}

/// Agents horde knows how to launch. Matching the bundled manifests means a spawned agent
/// is detected the moment it starts.
const AGENTS: &[(&str, &str)] = &[
    ("Claude Code", "claude"),
    ("Codex", "codex"),
    ("Gemini", "gemini"),
    ("Cursor Agent", "cursor-agent"),
    ("aider", "aider"),
    ("opencode", "opencode"),
];

fn preset(name: &str) -> Cmd {
    Cmd::ApplyLayout { preset: name.to_string() }
}

/// Widest label plus hint, used to size the popup.
pub fn width_for(level: &Level) -> u16 {
    let w = level
        .items
        .iter()
        .map(|i| {
            let hint = if i.hint.is_empty() { 0 } else { i.hint.chars().count() + 3 };
            i.label.chars().count() + hint
        })
        .chain(std::iter::once(level.title.chars().count() + 4))
        .max()
        .unwrap_or(20);
    (w as u16 + 4).clamp(20, 48)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{AgentInfo, AgentState, PaneInfo, Rect, SpaceInfo, TabInfo, ViewState};

    fn snap() -> Snapshot {
        let pane = |id: u32, agent: bool| PaneInfo {
            id,
            tab: 1,
            space: 1,
            title: format!("pane{id}"),
            cwd: "/tmp".into(),
            cell: Rect::new(0, 0, 10, 10),
            content: Rect::new(1, 1, 8, 8),
            cols: 8,
            rows: 8,
            agent: agent.then(|| AgentInfo {
                kind: "claude".into(),
                name: "builder".into(),
                class: Default::default(),
                state: AgentState::Idle,
                elapsed: 1,
                authority: "hook".into(),
                reason: "t".into(),
                activity: Default::default(),
                question: None,
            }),            spawned_by: None,
            exited: false,
            scroll_offset: 0,
            wants_mouse: false,
            bracketed_paste: false,
            role: None,
            pinned: false,
            board: false,
            repo: None,
        };
        Snapshot {
            protocol: 1,
            daemon_version: "test".into(),
            spaces: vec![SpaceInfo {
                id: 1,
                name: "api".into(),
                cwd: "/tmp".into(),
                tabs: vec![1],
                focused_tab: Some(1),
                agent_count: 1,
                attention_count: 0,
                accent: 0,
                collapsed: false,
                repo: None,
            }],
            tabs: vec![TabInfo {
                id: 1,
                space: 1,
                name: "agents".into(),
                panes: vec![1, 2],
                focused_pane: Some(1),
            }],
            panes: vec![pane(1, true), pane(2, false)],
            focused_space: Some(1),
            focused_tab: Some(1),
            focused_pane: Some(1),
            view: ViewState::default(),
            sidebar: Rect::default(),
            bus: Rect::default(),
            status: Rect::default(),
            tabbar: Rect::default(),
            tasks_open: 0,
            tasks_claimed: 0,
            triggers_armed: 0,
        }
    }

    #[test]
    fn every_target_produces_a_usable_menu() {
        let s = snap();
        for target in [
            Target::Pane(1),
            Target::Agent(1),
            Target::Space(1),
            Target::Tab(1),
            Target::Bus,
            Target::Root,
        ] {
            let l = build(target, &s, "ctrl+b");
            assert!(!l.title.is_empty(), "{target:?} has no title");
            assert!(l.items.iter().any(|i| !i.is_separator()), "{target:?} has no entries");
            // The initial selection must never land on a separator.
            assert!(l.selected().is_some(), "{target:?} starts on a separator");
        }
    }

    #[test]
    fn menu_titles_name_what_was_clicked() {
        let s = snap();
        assert_eq!(build(Target::Space(1), &s, "p").title, "api");
        assert_eq!(build(Target::Tab(1), &s, "p").title, "agents");
        assert_eq!(build(Target::Pane(2), &s, "p").title, "pane2");
    }

    #[test]
    fn send_message_appears_only_for_agent_panes() {
        let s = snap();
        let with = build(Target::Pane(1), &s, "p");
        let without = build(Target::Pane(2), &s, "p");
        assert!(with.items.iter().any(|i| i.label.starts_with("Send message")));
        assert!(!without.items.iter().any(|i| i.label.starts_with("Send message")));
    }

    #[test]
    fn zoom_entry_reflects_current_state() {
        let mut s = snap();
        assert!(build(Target::Pane(1), &s, "p").items.iter().any(|i| i.label == "Zoom"));
        s.view.zoom = Some(1);
        assert!(build(Target::Pane(1), &s, "p").items.iter().any(|i| i.label == "Unzoom"));
    }

    #[test]
    fn hints_show_the_keyboard_equivalent() {
        let l = build(Target::Pane(1), &snap(), "ctrl+b");
        let split = l.items.iter().find(|i| i.label == "Split right").unwrap();
        assert_eq!(split.hint, "ctrl+b |");
    }

    #[test]
    fn stepping_skips_separators_and_wraps() {
        let mut l = Level::new(
            "t",
            vec![
                Item::new("a", "", Act::Close),
                Item::separator(),
                Item::new("b", "", Act::Close),
            ],
        );
        assert_eq!(l.sel, 0);
        l.step(1);
        assert_eq!(l.selected().unwrap().label, "b", "separator must be skipped");
        l.step(1);
        assert_eq!(l.selected().unwrap().label, "a", "should wrap to the top");
        l.step(-1);
        assert_eq!(l.selected().unwrap().label, "b", "and wrap backwards");
    }

    #[test]
    fn stepping_an_all_separator_menu_does_not_hang() {
        let mut l = Level { title: "t".into(), items: vec![Item::separator()], sel: 0 };
        l.step(1);
        assert!(l.selected().is_none());
    }

    #[test]
    fn every_submenu_has_entries_and_no_dangling_submenu() {
        let cfg = crate::config::Config::default();
        for sub in [Sub::Layout, Sub::Spawn, Sub::Accent(1), Sub::Role(1)] {
            let l = submenu(sub, &cfg);
            assert!(l.selected().is_some(), "{sub:?}");
            // A submenu opening another submenu would need a deeper stack than exists.
            assert!(
                !l.items.iter().any(|i| matches!(i.act, Act::Submenu(_))),
                "{sub:?} nests a submenu"
            );
        }
    }

    #[test]
    fn layout_submenu_offers_exactly_the_known_presets() {
        let l = submenu(Sub::Layout, &crate::config::Config::default());
        let names: Vec<String> = l
            .items
            .iter()
            .filter_map(|i| match &i.act {
                Act::Cmd(Cmd::ApplyLayout { preset }) => Some(preset.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["solo", "duo", "trio", "dev", "quad"]);
        // And each one is a preset the layout engine actually knows.
        for n in &names {
            assert!(crate::daemon::layout::Layout::preset_pane_count(n).is_some(), "{n}");
        }
    }

    #[test]
    fn spawn_submenu_only_offers_agents_horde_can_detect() {
        let cfg = crate::config::Config::default();
        let detector = crate::daemon::agents::Detector::new(&cfg);
        let known = detector.manifest_names();
        for (_, cmd) in AGENTS {
            assert!(known.contains(&cmd.to_string()), "{cmd} has no detection manifest");
        }
    }

    #[test]
    fn width_stays_within_sensible_bounds() {
        let s = snap();
        for target in [Target::Pane(1), Target::Space(1), Target::Root, Target::Bus] {
            let w = width_for(&build(target, &s, "ctrl+b"));
            assert!((20..=48).contains(&w), "{target:?} -> {w}");
        }
    }

    #[test]
    fn prompts_all_describe_themselves() {
        for p in [
            Prompt::RenamePane(1),
            Prompt::RenameSpace(1),
            Prompt::RenameTab(1),
            Prompt::NewSpace,
            Prompt::SendTo(1),
            Prompt::RunCommand,
        ] {
            assert!(!p.title().is_empty());
            assert!(!p.hint().is_empty());
        }
    }

    #[test]
    fn a_space_menu_offers_its_colour_and_a_way_to_fold_it() {
        let l = build(Target::Space(1), &snap(), "p");
        assert!(l.items.iter().any(|i| i.act == Act::Submenu(Sub::Accent(1))), "{l:?}");
        assert!(
            l.items.iter().any(|i| i.act == Act::Cmd(Cmd::ToggleSpaceCollapsed(1))),
            "{l:?}"
        );
    }

    /// The label has to say what activating it does, not what the space currently is — the
    /// same pattern the Zoom entry already follows.
    #[test]
    fn the_collapse_entry_reflects_the_current_state() {
        let mut s = snap();
        assert!(build(Target::Space(1), &s, "p").items.iter().any(|i| i.label == "Collapse"));
        s.spaces[0].collapsed = true;
        assert!(build(Target::Space(1), &s, "p").items.iter().any(|i| i.label == "Expand"));
    }

    /// A role submenu with no way back out would make a mislabelled pane permanent.
    #[test]
    fn a_role_submenu_offers_a_way_to_clear() {
        let l = submenu(Sub::Role(1), &crate::config::Config::default());
        assert!(
            l.items.iter().any(|i| i.act == Act::Cmd(Cmd::SetPaneRole { pane: 1, role: String::new() })),
            "{l:?}"
        );
        // And a way to name one that was never declared.
        assert!(l.items.iter().any(|i| i.act == Act::Prompt(Prompt::SetRole(1))), "{l:?}");
    }

    /// Declaring a role is what puts it in the menu; it is not what makes it usable.
    #[test]
    fn declared_roles_appear_in_the_submenu() {
        let mut cfg = crate::config::Config::default();
        cfg.roles.push(crate::config::Role {
            name: "reviewer".into(),
            color: crate::proto::Rgb::new(1, 2, 3),
            glyph: "◈".into(),
        });
        let l = submenu(Sub::Role(1), &cfg);
        assert!(l.items.iter().any(|i| i.label == "◈ reviewer"), "{l:?}");
    }

    #[test]
    fn an_agent_menu_offers_a_role_and_a_pin() {
        let l = build(Target::Agent(1), &snap(), "p");
        assert!(l.items.iter().any(|i| i.act == Act::Submenu(Sub::Role(1))), "{l:?}");
        assert!(l.items.iter().any(|i| i.act == Act::Cmd(Cmd::TogglePanePinned(1))), "{l:?}");
    }
}
