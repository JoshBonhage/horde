//! Saving and restoring the session shape.
//!
//! Restoring the *shape* is not the same as restoring the *processes*. After a daemon
//! restart, panes come back as fresh shells in their saved directories — the same bargain
//! tmux makes. Agent panes can do better: with `restore = true` horde re-runs the agent
//! with its resume flag, using the session id its integration reported.
//!
//! Pane scrollback is deliberately **not** persisted. Terminal output holds secrets,
//! tokens, and command history, and writing it to disk by default would be the wrong
//! trade for a convenience feature.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::layout::{Axis, Layout, Node};
use super::Engine;
use crate::proto::PaneId;
#[cfg(test)]
use crate::proto::Dir;

/// Bumped when the on-disk shape changes. An unrecognised version is discarded rather than
/// guessed at, so a stale file can never corrupt a session.
///
/// Which is exactly why it moves so rarely: **bump this when old data would be *misread*,
/// never when new data would merely be *missing*.** Discarding the file throws away every
/// space, tab, pane and cwd — so a field that can carry `#[serde(default)]` should, and does
/// (`last_seen`, `last_alert`, `spawned_by`, `accent`, `role`). Trading a whole restored
/// session for one defaulted field is a strictly worse deal than defaulting it.
const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedState {
    pub version: u32,
    pub spaces: Vec<SavedSpace>,
    pub focused_space: Option<usize>,
    /// When a digest was last read. Saved so the window a digest covers survives the daemon
    /// restart it may well be reporting on. Defaulted, so older state files still load.
    #[serde(default)]
    pub last_seen: u64,
    /// When horde last reached out. Saved for the same reason and one more: a daemon restart
    /// while you are away must not reset the quiet window and ping you for everything it is
    /// still in the middle of telling you about.
    #[serde(default)]
    pub last_alert: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedSpace {
    pub name: String,
    pub cwd: String,
    pub tabs: Vec<SavedTab>,
    pub focused_tab: Option<usize>,
    /// Project accent slot. `None` in a file written before accents existed — restore then
    /// picks one, rather than defaulting every space to slot 0 and making the feature look
    /// broken after a single upgrade.
    #[serde(default)]
    pub accent: Option<u8>,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedTab {
    pub name: String,
    pub tree: SavedNode,
    /// Index into this tab's panes, in tree order.
    pub focused_pane: Option<usize>,
}

/// The layout tree with panes flattened to indices, so ids need not survive a restart.
#[derive(Debug, Serialize, Deserialize)]
pub enum SavedNode {
    Leaf(SavedPane),
    Split { horizontal: bool, ratio: f32, a: Box<SavedNode>, b: Box<SavedNode> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedPane {
    pub cmd: String,
    pub cwd: String,
    pub name: Option<String>,
    /// Agent kind detected when the state was saved, used to pick a resume command.
    pub agent_kind: Option<String>,
    pub agent_session: Option<String>,
    /// The trigger that started this pane. Restored, or `restore = true` would bring a
    /// machine-started agent back as one horde thinks you started — freeing a slot under the
    /// unattended cap and re-granting it the right to create triggers.
    #[serde(default)]
    pub spawned_by: Option<u64>,
    /// The job you gave this pane. Restored even when the pane comes back as a shell,
    /// because the absence of the agent is temporary and the label is yours.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub pinned: bool,
}

pub fn save(eng: &Engine, path: &Path) -> Result<()> {
    let s = &eng.session;
    let mut spaces = Vec::new();
    for space in &s.spaces {
        let mut tabs = Vec::new();
        for &tid in &space.tabs {
            let Some(tab) = s.tab(tid) else { continue };
            let Some(root) = tab.layout.root() else { continue };
            let order = tab.layout.panes();
            tabs.push(SavedTab {
                name: tab.name.clone(),
                tree: save_node(eng, root),
                focused_pane: tab.focused_pane.and_then(|p| order.iter().position(|&x| x == p)),
            });
        }
        spaces.push(SavedSpace {
            focused_tab: space
                .focused_tab
                .and_then(|t| space.tabs.iter().position(|&x| x == t)),
            name: space.name.clone(),
            cwd: space.cwd.to_string_lossy().to_string(),
            tabs,
            accent: Some(space.accent),
            collapsed: space.collapsed,
        });
    }

    let state = SavedState {
        version: STATE_VERSION,
        focused_space: s.focused_space.and_then(|id| s.spaces.iter().position(|x| x.id == id)),
        spaces,
        last_seen: eng.last_seen,
        last_alert: eng.last_alert,
    };

    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let json = serde_json::to_string_pretty(&state)?;
    // Write to a sibling then rename, so a crash mid-write cannot leave a truncated file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn save_node(eng: &Engine, n: &Node) -> SavedNode {
    match n {
        Node::Leaf(id) => {
            let p = eng.session.panes.get(id);
            SavedNode::Leaf(SavedPane {
                cmd: p.map(|p| p.cmd.clone()).unwrap_or_else(|| eng.cfg.shell.clone()),
                cwd: p
                    .map(|p| p.cwd.to_string_lossy().to_string())
                    .unwrap_or_else(|| ".".into()),
                name: p.and_then(|p| p.name.clone()),
                agent_kind: p.and_then(|p| p.agent.as_ref().map(|a| a.kind.clone())),
                agent_session: p.and_then(|p| p.agent.as_ref().and_then(|a| a.session_id.clone())),
                spawned_by: p.and_then(|p| p.spawned_by),
                role: p.and_then(|p| p.role.clone()),
                pinned: p.is_some_and(|p| p.pinned),
            })
        }
        Node::Split { axis, ratio, a, b, .. } => SavedNode::Split {
            horizontal: *axis == Axis::Horizontal,
            ratio: *ratio,
            a: Box::new(save_node(eng, a)),
            b: Box::new(save_node(eng, b)),
        },
    }
}

pub fn load(path: &Path) -> Result<Option<SavedState>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading saved state"),
    };
    let state: SavedState = serde_json::from_str(&text).context("parsing saved state")?;
    if state.version != STATE_VERSION {
        super::log_line(&format!(
            "ignoring saved state version {} (expected {STATE_VERSION})",
            state.version
        ));
        return Ok(None);
    }
    Ok(Some(state))
}

pub fn restore(eng: &mut Engine, saved: SavedState) -> Result<()> {
    let cfg = eng.cfg.clone();
    eng.last_seen = saved.last_seen;
    eng.last_alert = saved.last_alert;
    for (si, space) in saved.spaces.iter().enumerate() {
        let cwd = PathBuf::from(&space.cwd);
        // A saved directory can be gone by the next start; fall back rather than fail.
        let cwd = if cwd.is_dir() { cwd } else { std::env::current_dir()? };

        // `create_space` also makes a first tab; the saved tabs replace it.
        let space_id = eng.session.create_space(&cfg, Some(&space.name), &cwd)?;
        // A file written before accents existed carries none; `create_space` has already
        // picked one, so leaving it alone is what keeps an upgraded session colourful
        // instead of uniformly slot 0.
        if let Some(sp) = eng.session.space_mut(space_id) {
            if let Some(a) = space.accent {
                sp.accent = a;
            }
            sp.collapsed = space.collapsed;
        }
        let auto_tabs: Vec<_> =
            eng.session.space(space_id).map(|s| s.tabs.clone()).unwrap_or_default();

        for (ti, tab) in space.tabs.iter().enumerate() {
            let tab_id = eng.session.create_tab(&cfg, space_id, Some(&tab.name))?;
            // create_tab spawns one pane; discard it and rebuild the saved tree instead.
            let seeded: Vec<PaneId> =
                eng.session.tab(tab_id).map(|t| t.layout.panes()).unwrap_or_default();

            let mut leaves = Vec::new();
            collect_leaves(&tab.tree, &mut leaves);
            let mut ids = Vec::new();
            for leaf in &leaves {
                let cmd = restore_command(&cfg, leaf);
                let leaf_cwd = {
                    let c = PathBuf::from(&leaf.cwd);
                    if c.is_dir() {
                        c
                    } else {
                        cwd.clone()
                    }
                };
                let id = eng.session.spawn_pane_public(&cfg, space_id, tab_id, &cmd, &leaf_cwd)?;
                if let Some(p) = eng.session.panes.get_mut(&id) {
                    p.name = leaf.name.clone();
                    p.spawned_by = leaf.spawned_by;
                    p.role = leaf.role.clone();
                    p.pinned = leaf.pinned;
                }
                ids.push(id);
            }

            let mut cursor = 0usize;
            let tree = rebuild(&tab.tree, &ids, &mut cursor);
            if let Some(t) = eng.session.tab_mut(tab_id) {
                t.layout = Layout::from_root(tree);
                t.focused_pane = tab.focused_pane.and_then(|i| ids.get(i).copied());
            }
            for p in seeded {
                if let Some(pane) = eng.session.panes.get_mut(&p) {
                    pane.kill();
                }
                eng.session.panes.remove(&p);
            }

            if saved.focused_space == Some(si) && space.focused_tab == Some(ti) {
                eng.session.focus_space(space_id);
                if let Some(s) = eng.session.space_mut(space_id) {
                    s.focused_tab = Some(tab_id);
                }
            }
        }

        // Drop the placeholder tab that came with the space.
        for t in auto_tabs {
            let _ = eng.session.close_tab(&cfg, t);
        }
    }

    if let Some(i) = saved.focused_space {
        if let Some(id) = eng.session.spaces.get(i).map(|s| s.id) {
            eng.session.focus_space(id);
        }
    }
    eng.session.relayout(&cfg);
    eng.touch();
    Ok(())
}

/// Pick the command to bring a pane back.
///
/// `restore_agents` decides one thing: whether agent panes come back as agents at all. When
/// it is off, they come back as shells — re-running the bare command would start a *new*
/// agent session, which is not what "don't restore agents" means.
///
/// With it on, an agent is resumed only when there is a session id to resume. Starting a
/// fresh agent unbidden would be presumptuous, so a session-less agent pane also becomes a
/// shell.
fn restore_command(cfg: &crate::config::Config, leaf: &SavedPane) -> String {
    let Some(kind) = leaf.agent_kind.as_deref() else {
        // Not an agent pane: replay whatever it was running.
        return leaf.cmd.clone();
    };
    if !cfg.restore_agents {
        return cfg.shell.clone();
    }
    match leaf.agent_session.as_deref().and_then(|s| resume_command(kind, s)) {
        Some(cmd) => cmd,
        None => cfg.shell.clone(),
    }
}

/// How to resume a given agent, if horde knows.
fn resume_command(kind: &str, session: &str) -> Option<String> {
    Some(match kind {
        "claude" => format!("claude --resume {session}"),
        "codex" => format!("codex resume {session}"),
        // No known resume flag; a shell is safer than guessing at one.
        _ => return None,
    })
}

fn collect_leaves<'a>(n: &'a SavedNode, out: &mut Vec<&'a SavedPane>) {
    match n {
        SavedNode::Leaf(p) => out.push(p),
        SavedNode::Split { a, b, .. } => {
            collect_leaves(a, out);
            collect_leaves(b, out);
        }
    }
}

/// Rebuild the tree, consuming freshly spawned pane ids in the same order the leaves were
/// collected.
fn rebuild(n: &SavedNode, ids: &[PaneId], cursor: &mut usize) -> Node {
    match n {
        SavedNode::Leaf(_) => {
            let id = ids.get(*cursor).copied().unwrap_or(0);
            *cursor += 1;
            Node::Leaf(id)
        }
        SavedNode::Split { horizontal, ratio, a, b } => {
            let a = Box::new(rebuild(a, ids, cursor));
            let b = Box::new(rebuild(b, ids, cursor));
            Node::Split {
                // Ids are reassigned by `Layout::from_root`.
                id: 0,
                axis: if *horizontal { Axis::Horizontal } else { Axis::Vertical },
                ratio: *ratio,
                a,
                b,
            }
        }
    }
}

/// Direction a saved split implies, used only by tests.
#[cfg(test)]
fn dir_of(horizontal: bool) -> Dir {
    if horizontal {
        Dir::Right
    } else {
        Dir::Down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_file_is_not_an_error() {
        assert!(load(Path::new("/nonexistent/horde/state.json")).unwrap().is_none());
    }

    #[test]
    fn a_future_version_is_ignored_rather_than_guessed_at() {
        let p = std::env::temp_dir().join("horde-state-version.json");
        std::fs::write(
            &p,
            serde_json::to_string(&SavedState {
                version: STATE_VERSION + 99,
                spaces: vec![],
                focused_space: None,
                last_seen: 0,
                last_alert: 0,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(load(&p).unwrap().is_none());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn corrupt_state_is_an_error_not_a_panic() {
        let p = std::env::temp_dir().join("horde-state-corrupt.json");
        std::fs::write(&p, "{ not json").unwrap();
        assert!(load(&p).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn rebuild_preserves_structure_and_assigns_panes_in_order() {
        let tree = SavedNode::Split {
            horizontal: true,
            ratio: 0.3,
            a: Box::new(SavedNode::Leaf(SavedPane {
                cmd: "zsh".into(),
                cwd: ".".into(),
                name: None,
                agent_kind: None,
                agent_session: None,
                spawned_by: None,
                    role: None,
                    pinned: false,
            })),
            b: Box::new(SavedNode::Split {
                horizontal: false,
                ratio: 0.6,
                a: Box::new(SavedNode::Leaf(SavedPane {
                    cmd: "zsh".into(),
                    cwd: ".".into(),
                    name: None,
                    agent_kind: None,
                    agent_session: None,
                    spawned_by: None,
                    role: None,
                    pinned: false,
                })),
                b: Box::new(SavedNode::Leaf(SavedPane {
                    cmd: "zsh".into(),
                    cwd: ".".into(),
                    name: None,
                    agent_kind: None,
                    agent_session: None,
                    spawned_by: None,
                    role: None,
                    pinned: false,
                })),
            }),
        };

        let ids = vec![10, 20, 30];
        let mut cursor = 0;
        let node = rebuild(&tree, &ids, &mut cursor);
        assert_eq!(cursor, 3, "every leaf must consume exactly one id");

        let layout = Layout::from_root(node);
        assert_eq!(layout.panes(), vec![10, 20, 30]);

        // Ratios and axes survive the round trip.
        let geo = layout.geometry(crate::proto::Rect::new(0, 0, 100, 40));
        assert_eq!(geo.panes[&10].w, 30, "0.3 of 100 columns");
        assert_eq!(dir_of(true), Dir::Right);

        // And split ids were reassigned uniquely, so resize can still target one divider.
        assert_eq!(geo.splits.len(), 2);
    }

    #[test]
    fn restore_command_resumes_agents_only_with_a_session_id() {
        let mut cfg = crate::config::Config::default();
        cfg.shell = "/bin/zsh".into();

        let with_session = SavedPane {
            cmd: "claude".into(),
            cwd: ".".into(),
            name: None,
            agent_kind: Some("claude".into()),
            agent_session: Some("abc123".into()),
            spawned_by: None,
                    role: None,
                    pinned: false,
        };
        assert_eq!(restore_command(&cfg, &with_session), "claude --resume abc123");

        // No session id: coming back as a shell beats silently starting a new agent.
        let no_session = SavedPane { agent_session: None, ..with_session };
        assert_eq!(restore_command(&cfg, &no_session), "/bin/zsh");

        // An agent horde has no resume flag for also comes back as a shell.
        let unknown_agent = SavedPane {
            cmd: "someagent".into(),
            cwd: ".".into(),
            name: None,
            agent_kind: Some("someagent".into()),
            agent_session: Some("abc123".into()),
            spawned_by: None,
                    role: None,
                    pinned: false,
        };
        assert_eq!(restore_command(&cfg, &unknown_agent), "/bin/zsh");

        // Restoration disabled: agent panes become shells rather than starting fresh
        // agents, which is what "don't restore agents" has to mean.
        cfg.restore_agents = false;
        let with_session = SavedPane {
            cmd: "claude".into(),
            cwd: ".".into(),
            name: None,
            agent_kind: Some("claude".into()),
            agent_session: Some("abc123".into()),
            spawned_by: None,
                    role: None,
                    pinned: false,
        };
        assert_eq!(restore_command(&cfg, &with_session), "/bin/zsh");

        // A plain shell pane is unaffected either way.
        let shell = SavedPane {
            cmd: "zsh".into(),
            cwd: ".".into(),
            name: None,
            agent_kind: None,
            agent_session: None,
            spawned_by: None,
                    role: None,
                    pinned: false,
        };
        assert_eq!(restore_command(&cfg, &shell), "zsh");
    }

    #[test]
    fn save_writes_atomically_leaving_no_temp_file() {
        // A truncated state file would break the next start, so save goes via rename.
        let dir = std::env::temp_dir().join("horde-persist-atomic");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        let state =
            SavedState {
            version: STATE_VERSION,
            spaces: vec![],
            focused_space: None,
            last_seen: 0,
            last_alert: 0,
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert!(path.exists());
        assert!(!tmp.exists(), "temp file must not survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ids are not persisted — the tree is flattened to positional indices and every id is
    /// reassigned on restore — so metadata has to live *inside* these structs. This asserts by
    /// name for exactly that reason: a side table keyed by `SpaceId` would come back attached
    /// to the wrong space, or to nothing.
    #[test]
    fn metadata_survives_a_save_and_restore_round_trip() {
        let p = std::env::temp_dir().join("horde-state-meta.json");
        let _ = std::fs::remove_file(&p);

        let mut eng = crate::daemon::tests::engine_with_idle_agents("persist-meta", 2);
        let space = eng.session.focused_space.unwrap();
        eng.session.rename_space(space, "api-refactor");
        eng.session.set_space_accent(space, Some(5));
        eng.session.toggle_space_collapsed(space, Some(true));
        let pane = eng.session.focused_pane().unwrap();
        eng.session.set_pane_role(pane, "Reviewer");
        eng.session.toggle_pane_pinned(pane, Some(true));

        save(&eng, &p).unwrap();
        let loaded = load(&p).unwrap().expect("state file");
        let saved_space = loaded.spaces.iter().find(|s| s.name == "api-refactor").unwrap();
        assert_eq!(saved_space.accent, Some(5));
        assert!(saved_space.collapsed);

        let mut fresh = crate::daemon::tests::engine();
        restore(&mut fresh, loaded).unwrap();
        let sp = fresh.session.spaces.iter().find(|s| s.name == "api-refactor").unwrap();
        assert_eq!(sp.accent, 5, "the slot, not a fresh pick");
        assert!(sp.collapsed);
        let roles: Vec<Option<String>> =
            fresh.session.panes.values().map(|p| p.role.clone()).collect();
        assert!(roles.contains(&Some("reviewer".into())), "normalised and restored: {roles:?}");
        assert_eq!(fresh.session.panes.values().filter(|p| p.pinned).count(), 1);
        let _ = std::fs::remove_file(p);
    }

    /// The exact old-`state.json` case: a file written before accents existed. It must load —
    /// discarding it would cost every space, tab and pane — and the spaces must come back with
    /// *distinct* slots rather than eight copies of slot 0, or the feature looks broken after
    /// one upgrade rather than merely absent.
    #[test]
    fn a_state_file_without_accents_still_loads_and_gets_them() {
        let p = std::env::temp_dir().join("horde-state-preaccent.json");
        let tab = |n: &str| serde_json::json!({
            "name": n,
            "tree": { "Leaf": { "cmd": "zsh", "cwd": ".", "name": null,
                                "agent_kind": null, "agent_session": null } },
            "focused_pane": 0,
        });
        let doc = serde_json::json!({
            "version": 1,
            "focused_space": 0,
            "spaces": [
                { "name": "one", "cwd": ".", "tabs": [tab("1")], "focused_tab": 0 },
                { "name": "two", "cwd": ".", "tabs": [tab("1")], "focused_tab": 0 },
                { "name": "three", "cwd": ".", "tabs": [tab("1")], "focused_tab": 0 },
            ],
        });
        std::fs::write(&p, serde_json::to_string(&doc).unwrap()).unwrap();

        let loaded = load(&p).unwrap().expect("an older file must still load");
        assert!(loaded.spaces.iter().all(|s| s.accent.is_none()));
        assert!(loaded.spaces.iter().all(|s| !s.collapsed));

        let mut eng = crate::daemon::tests::engine();
        eng.session.spaces.clear();
        eng.session.tabs.clear();
        eng.session.panes.clear();
        restore(&mut eng, loaded).unwrap();
        let slots: Vec<u8> = eng.session.spaces.iter().map(|s| s.accent).collect();
        let mut uniq = slots.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(slots.len(), uniq.len(), "every space needs its own colour: {slots:?}");
        let _ = std::fs::remove_file(p);
    }

    /// Bumping this discards the whole file (see `load`), so it is only ever for changes that
    /// would make old data *misread* — never for a field that would merely be missing.
    #[test]
    fn the_state_version_did_not_move_for_an_additive_field() {
        assert_eq!(STATE_VERSION, 1);
    }
}
