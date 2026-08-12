//! Session model: spaces own tabs, tabs own panes, panes tile inside a tab's layout.
//!
//! This module also owns all screen geometry. The client is a renderer that draws where it
//! is told, which keeps one source of truth for where a pane lives and means PTY sizes and
//! drawn rects can never disagree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use super::layout::Layout;
use super::pane::Pane;
use crate::config::Config;
use crate::proto::{
    Dir, PaneId, PaneInfo, Rect, SpaceId, SpaceInfo, Snapshot, TabId, TabInfo, ViewState,
    PROTOCOL_VERSION,
};

/// Fallback geometry while nothing is attached, so panes still hold a workable size.
const DETACHED_COLS: u16 = 120;
const DETACHED_ROWS: u16 = 40;

pub struct Space {
    pub id: SpaceId,
    pub name: String,
    pub cwd: PathBuf,
    pub tabs: Vec<TabId>,
    pub focused_tab: Option<TabId>,
}

pub struct Tab {
    pub id: TabId,
    pub space: SpaceId,
    pub name: String,
    pub layout: Layout,
    pub focused_pane: Option<PaneId>,
}

/// Where each region of the screen sits. Computed once per relayout.
#[derive(Debug, Clone, Copy, Default)]
pub struct Chrome {
    pub tabbar: Rect,
    pub sidebar: Rect,
    pub bus: Rect,
    pub status: Rect,
    /// Region the pane tree tiles into.
    pub panes: Rect,
}

pub struct Session {
    pub spaces: Vec<Space>,
    pub tabs: Vec<Tab>,
    pub panes: HashMap<PaneId, Pane>,
    pub focused_space: Option<SpaceId>,
    pub view: ViewState,
    pub chrome: Chrome,
    /// Last size reported by a client; retained while detached.
    pub client_cols: u16,
    pub client_rows: u16,

    next_space: SpaceId,
    next_tab: TabId,
    next_pane: PaneId,
}

impl Session {
    pub fn new(cfg: &Config) -> Session {
        Session {
            spaces: Vec::new(),
            tabs: Vec::new(),
            panes: HashMap::new(),
            focused_space: None,
            view: ViewState {
                sidebar_open: cfg.sidebar,
                bus_open: cfg.bus,
                sidebar_width: cfg.sidebar_width,
                bus_width: cfg.bus_width,
                zoom: None,
            },
            chrome: Chrome::default(),
            client_cols: DETACHED_COLS,
            client_rows: DETACHED_ROWS,
            next_space: 1,
            next_tab: 1,
            next_pane: 1,
        }
    }

    // -- lookups ----------------------------------------------------------

    pub fn space(&self, id: SpaceId) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn space_mut(&mut self, id: SpaceId) -> Option<&mut Space> {
        self.spaces.iter_mut().find(|s| s.id == id)
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn focused_tab(&self) -> Option<TabId> {
        self.space(self.focused_space?)?.focused_tab
    }

    pub fn focused_pane(&self) -> Option<PaneId> {
        self.tab(self.focused_tab()?)?.focused_pane
    }

    /// Panes in the currently visible tab, in stable visual order.
    pub fn visible_panes(&self) -> Vec<PaneId> {
        self.focused_tab()
            .and_then(|t| self.tab(t))
            .map(|t| t.layout.panes())
            .unwrap_or_default()
    }

    pub fn find_space_by_name(&self, name: &str) -> Option<SpaceId> {
        self.spaces.iter().find(|s| s.name == name).map(|s| s.id)
    }

    // -- creation ---------------------------------------------------------

    pub fn create_space(&mut self, cfg: &Config, name: Option<&str>, cwd: &Path) -> Result<SpaceId> {
        let id = self.next_space;
        self.next_space += 1;

        let base = name.map(|s| s.to_string()).unwrap_or_else(|| {
            cwd.file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("space{id}"))
        });
        let name = self.unique_space_name(&base);

        self.spaces.push(Space {
            id,
            name,
            cwd: cwd.to_path_buf(),
            tabs: Vec::new(),
            focused_tab: None,
        });
        // Focus the new space. Creating one and being left in the old one means the next
        // `pane split` or spawned agent lands somewhere you did not ask for — which is how
        // three freshly created spaces end up empty while the original fills up.
        self.focused_space = Some(id);
        self.create_tab(cfg, id, None)?;
        Ok(id)
    }

    fn unique_space_name(&self, base: &str) -> String {
        if self.find_space_by_name(base).is_none() {
            return base.to_string();
        }
        for n in 2.. {
            let cand = format!("{base}-{n}");
            if self.find_space_by_name(&cand).is_none() {
                return cand;
            }
        }
        unreachable!()
    }

    pub fn create_tab(&mut self, cfg: &Config, space: SpaceId, name: Option<&str>) -> Result<TabId> {
        let cwd = self.space(space).ok_or_else(|| anyhow!("no such space"))?.cwd.clone();
        let id = self.next_tab;
        self.next_tab += 1;

        let n = self.space(space).map(|s| s.tabs.len()).unwrap_or(0) + 1;
        self.tabs.push(Tab {
            id,
            space,
            name: name.map(|s| s.to_string()).unwrap_or_else(|| format!("{n}")),
            layout: Layout::new(),
            focused_pane: None,
        });
        if let Some(s) = self.space_mut(space) {
            s.tabs.push(id);
            s.focused_tab = Some(id);
        }

        let pane = self.spawn_pane(cfg, space, id, &cfg.shell.clone(), &cwd)?;
        if let Some(t) = self.tab_mut(id) {
            t.layout = Layout::single(pane);
            t.focused_pane = Some(pane);
        }
        self.relayout(cfg);
        Ok(id)
    }

    /// Spawn a pane without attaching it to a layout. Used by restore, which builds the
    /// tree itself after every pane exists.
    pub fn spawn_pane_public(
        &mut self,
        cfg: &Config,
        space: SpaceId,
        tab: TabId,
        cmd: &str,
        cwd: &Path,
    ) -> Result<PaneId> {
        self.spawn_pane(cfg, space, tab, cmd, cwd)
    }

    fn spawn_pane(
        &mut self,
        cfg: &Config,
        space: SpaceId,
        tab: TabId,
        cmd: &str,
        cwd: &Path,
    ) -> Result<PaneId> {
        let id = self.next_pane;
        self.next_pane += 1;
        // Real size is applied by the relayout that follows; this is just a starting point.
        let pane = Pane::spawn(
            id,
            tab,
            space,
            cmd,
            cwd,
            80,
            24,
            cfg.scrollback,
            &crate::config::socket_path(),
        )?;
        self.panes.insert(id, pane);
        Ok(id)
    }

    /// Split the focused pane (or `target`) and put a new pane beside it.
    pub fn split(
        &mut self,
        cfg: &Config,
        target: Option<PaneId>,
        dir: Dir,
        cmd: Option<&str>,
    ) -> Result<PaneId> {
        let target = target.or_else(|| self.focused_pane()).ok_or_else(|| anyhow!("no pane"))?;
        let pane = self.panes.get(&target).ok_or_else(|| anyhow!("no such pane"))?;
        let (space, tab) = (pane.space, pane.tab);
        // New panes inherit the cwd of the pane they split from, which is what you want
        // when fanning out work in one project.
        let cwd = pane.cwd.clone();
        let cmd = cmd.map(|s| s.to_string()).unwrap_or_else(|| cfg.shell.clone());

        // Zoom hides siblings, so splitting while zoomed would put the new pane somewhere
        // invisible. Drop the zoom first.
        if self.view.zoom.is_some() {
            self.view.zoom = None;
            self.relayout(cfg);
        }

        let area = self.chrome.panes;
        let new_id = self.next_pane;
        let fits = self
            .tab(tab)
            .map(|t| {
                let mut probe = t.layout.clone();
                probe.split(target, dir, new_id, area)
            })
            .unwrap_or(false);
        if !fits {
            return Err(anyhow!("not enough room to split — try zooming out or closing a pane"));
        }

        let pane_id = self.spawn_pane(cfg, space, tab, &cmd, &cwd)?;
        if let Some(t) = self.tab_mut(tab) {
            t.layout.split(target, dir, pane_id, area);
            t.focused_pane = Some(pane_id);
        }
        self.relayout(cfg);
        Ok(pane_id)
    }

    // -- destruction ------------------------------------------------------

    pub fn close_pane(&mut self, cfg: &Config, pane: PaneId) -> Result<()> {
        let Some(p) = self.panes.get_mut(&pane) else { return Ok(()) };
        let tab_id = p.tab;
        p.kill();
        self.panes.remove(&pane);

        if self.view.zoom == Some(pane) {
            self.view.zoom = None;
        }

        let mut tab_empty = false;
        if let Some(t) = self.tab_mut(tab_id) {
            t.layout.close(pane);
            if t.focused_pane == Some(pane) {
                // Focus the first survivor rather than leaving the tab focus dangling.
                t.focused_pane = t.layout.panes().first().copied();
            }
            tab_empty = t.layout.is_empty();
        }
        if tab_empty {
            self.close_tab(cfg, tab_id)?;
        }
        self.relayout(cfg);
        Ok(())
    }

    pub fn close_tab(&mut self, cfg: &Config, tab: TabId) -> Result<()> {
        let Some(t) = self.tab(tab) else { return Ok(()) };
        let space_id = t.space;
        let panes = t.layout.panes();
        for p in panes {
            if let Some(pane) = self.panes.get_mut(&p) {
                pane.kill();
            }
            self.panes.remove(&p);
        }
        self.tabs.retain(|t| t.id != tab);

        let mut space_empty = false;
        if let Some(s) = self.space_mut(space_id) {
            let idx = s.tabs.iter().position(|&t| t == tab);
            s.tabs.retain(|&t| t != tab);
            if s.focused_tab == Some(tab) {
                // Prefer the tab that took this one's place, else the one before it.
                s.focused_tab = idx
                    .and_then(|i| s.tabs.get(i).or_else(|| s.tabs.get(i.saturating_sub(1))))
                    .copied();
            }
            space_empty = s.tabs.is_empty();
        }
        if space_empty {
            self.close_space(cfg, space_id)?;
        }
        self.relayout(cfg);
        Ok(())
    }

    pub fn close_space(&mut self, cfg: &Config, space: SpaceId) -> Result<()> {
        let tabs: Vec<TabId> =
            self.space(space).map(|s| s.tabs.clone()).unwrap_or_default();
        for t in tabs {
            let panes = self.tab(t).map(|t| t.layout.panes()).unwrap_or_default();
            for p in panes {
                if let Some(pane) = self.panes.get_mut(&p) {
                    pane.kill();
                }
                self.panes.remove(&p);
            }
            self.tabs.retain(|x| x.id != t);
        }
        let idx = self.spaces.iter().position(|s| s.id == space);
        self.spaces.retain(|s| s.id != space);
        if self.focused_space == Some(space) {
            self.focused_space = idx
                .and_then(|i| self.spaces.get(i).or_else(|| self.spaces.get(i.saturating_sub(1))))
                .map(|s| s.id);
        }
        self.relayout(cfg);
        Ok(())
    }

    /// Reap panes whose child exited. Returns the ids removed.
    ///
    /// A pane whose command exited is closed the way tmux does it — the pane goes away and
    /// its space is reclaimed by its neighbours.
    /// True when any pane still holds bytes the tty has not taken.
    ///
    /// Used to keep the tick loop fast while a message is draining, since each tick pushes
    /// only what the terminal will accept at that moment.
    pub fn has_pending_output(&self) -> bool {
        self.panes.values().any(|p| p.has_deferred())
    }

    pub fn reap_exited(&mut self, cfg: &Config) -> Vec<PaneId> {
        let dead: Vec<PaneId> = self
            .panes
            .values()
            .filter(|p| p.exited.is_some())
            .map(|p| p.id)
            .collect();
        for p in &dead {
            let _ = self.close_pane(cfg, *p);
        }
        dead
    }

    // -- focus and navigation --------------------------------------------

    pub fn focus_pane(&mut self, pane: PaneId) -> bool {
        let Some(p) = self.panes.get(&pane) else { return false };
        let (space, tab) = (p.space, p.tab);
        self.focused_space = Some(space);
        if let Some(s) = self.space_mut(space) {
            s.focused_tab = Some(tab);
        }
        if let Some(t) = self.tab_mut(tab) {
            t.focused_pane = Some(pane);
        }
        true
    }

    pub fn focus_dir(&mut self, dir: Dir) -> bool {
        // While zoomed there are no visible neighbours to move to.
        if self.view.zoom.is_some() {
            return false;
        }
        let Some(tab) = self.focused_tab() else { return false };
        let Some(from) = self.focused_pane() else { return false };
        let area = self.chrome.panes;
        let Some(next) = self.tab(tab).and_then(|t| t.layout.focus_dir(from, dir, area)) else {
            return false;
        };
        if let Some(t) = self.tab_mut(tab) {
            t.focused_pane = Some(next);
        }
        true
    }

    pub fn rename_space(&mut self, space: SpaceId, name: &str) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        // Names address spaces in `horde send` and `space focus`, so they must stay unique.
        let unique = if self.find_space_by_name(name).is_some_and(|id| id != space) {
            self.unique_space_name(name)
        } else {
            name.to_string()
        };
        match self.space_mut(space) {
            Some(s) => {
                s.name = unique;
                true
            }
            None => false,
        }
    }

    pub fn rename_tab(&mut self, tab: TabId, name: &str) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        match self.tab_mut(tab) {
            Some(t) => {
                t.name = name.to_string();
                true
            }
            None => false,
        }
    }

    /// Focus a tab, switching space if it lives in another one.
    pub fn focus_tab(&mut self, tab: TabId) -> bool {
        let Some(space) = self.tab(tab).map(|t| t.space) else { return false };
        self.focused_space = Some(space);
        if let Some(s) = self.space_mut(space) {
            s.focused_tab = Some(tab);
        }
        true
    }

    pub fn focus_space(&mut self, space: SpaceId) -> bool {
        if self.space(space).is_none() {
            return false;
        }
        self.focused_space = Some(space);
        true
    }

    pub fn cycle_space(&mut self, delta: i32) -> bool {
        if self.spaces.is_empty() {
            return false;
        }
        let cur = self
            .focused_space
            .and_then(|id| self.spaces.iter().position(|s| s.id == id))
            .unwrap_or(0);
        let n = self.spaces.len() as i32;
        let next = (cur as i32 + delta).rem_euclid(n) as usize;
        self.focused_space = Some(self.spaces[next].id);
        true
    }

    pub fn cycle_tab(&mut self, delta: i32) -> bool {
        let Some(space) = self.focused_space else { return false };
        let Some(s) = self.space(space) else { return false };
        if s.tabs.is_empty() {
            return false;
        }
        let cur = s
            .focused_tab
            .and_then(|id| s.tabs.iter().position(|&t| t == id))
            .unwrap_or(0);
        let n = s.tabs.len() as i32;
        let next = (cur as i32 + delta).rem_euclid(n) as usize;
        let tab = s.tabs[next];
        if let Some(s) = self.space_mut(space) {
            s.focused_tab = Some(tab);
        }
        true
    }

    pub fn goto_tab(&mut self, index: usize) -> bool {
        let Some(space) = self.focused_space else { return false };
        let Some(&tab) = self.space(space).and_then(|s| s.tabs.get(index)) else { return false };
        if let Some(s) = self.space_mut(space) {
            s.focused_tab = Some(tab);
        }
        true
    }

    /// Next pane anywhere in the session whose agent wants attention.
    ///
    /// Ordering follows the sidebar, and the search starts after the currently focused
    /// pane so pressing it repeatedly walks the queue instead of sticking on one agent.
    pub fn next_attention(&self) -> Option<PaneId> {
        let ordered = self.attention_order();
        if ordered.is_empty() {
            return None;
        }
        let cur = self.focused_pane();
        let start = cur
            .and_then(|c| ordered.iter().position(|&p| p == c))
            .map(|i| i + 1)
            .unwrap_or(0);
        for k in 0..ordered.len() {
            let p = ordered[(start + k) % ordered.len()];
            if Some(p) != cur {
                return Some(p);
            }
        }
        ordered.first().copied()
    }

    fn attention_order(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        for s in &self.spaces {
            for &t in &s.tabs {
                if let Some(tab) = self.tab(t) {
                    for p in tab.layout.panes() {
                        if let Some(pane) = self.panes.get(&p) {
                            if pane
                                .agent
                                .as_ref()
                                .is_some_and(|a| a.state.needs_attention())
                            {
                                out.push(p);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    // -- layout mutation --------------------------------------------------

    pub fn resize_pane(&mut self, cfg: &Config, dir: Dir, cells: u16) -> bool {
        let Some(tab) = self.focused_tab() else { return false };
        let Some(pane) = self.focused_pane() else { return false };
        let area = self.chrome.panes;
        let ok = self
            .tab_mut(tab)
            .map(|t| t.layout.resize(pane, dir, cells, area))
            .unwrap_or(false);
        if ok {
            self.relayout(cfg);
        }
        ok
    }

    pub fn swap_dir(&mut self, cfg: &Config, dir: Dir) -> bool {
        let Some(tab) = self.focused_tab() else { return false };
        let Some(from) = self.focused_pane() else { return false };
        let area = self.chrome.panes;
        let Some(other) = self.tab(tab).and_then(|t| t.layout.focus_dir(from, dir, area)) else {
            return false;
        };
        let ok = self.tab_mut(tab).map(|t| t.layout.swap(from, other)).unwrap_or(false);
        if ok {
            self.relayout(cfg);
        }
        ok
    }

    pub fn toggle_zoom(&mut self, cfg: &Config) -> bool {
        let Some(pane) = self.focused_pane() else { return false };
        self.view.zoom = if self.view.zoom == Some(pane) { None } else { Some(pane) };
        self.relayout(cfg);
        true
    }

    pub fn toggle_sidebar(&mut self, cfg: &Config) {
        self.view.sidebar_open = !self.view.sidebar_open;
        self.relayout(cfg);
    }

    pub fn toggle_bus(&mut self, cfg: &Config) {
        self.view.bus_open = !self.view.bus_open;
        self.relayout(cfg);
    }

    pub fn set_client_size(&mut self, cfg: &Config, cols: u16, rows: u16) {
        self.client_cols = cols.max(20);
        self.client_rows = rows.max(6);
        self.relayout(cfg);
    }

    /// Apply a named preset to the focused tab, spawning or closing panes to match.
    pub fn apply_preset(&mut self, cfg: &Config, name: &str) -> Result<()> {
        let want = Layout::preset_pane_count(name)
            .ok_or_else(|| anyhow!("unknown layout {name:?} (solo, duo, trio, dev, quad)"))?;
        let tab = self.focused_tab().ok_or_else(|| anyhow!("no tab"))?;
        let mut have = self.tab(tab).map(|t| t.layout.panes()).unwrap_or_default();

        // Grow or shrink to the required pane count before rebuilding the tree, so the
        // preset always gets exactly the panes it expects.
        while have.len() > want {
            let victim = have.pop().unwrap();
            if let Some(p) = self.panes.get_mut(&victim) {
                p.kill();
            }
            self.panes.remove(&victim);
            if let Some(t) = self.tab_mut(tab) {
                t.layout.close(victim);
            }
        }
        while have.len() < want {
            let (space, cwd) = {
                let t = self.tab(tab).ok_or_else(|| anyhow!("no tab"))?;
                let cwd = self
                    .space(t.space)
                    .map(|s| s.cwd.clone())
                    .unwrap_or_else(|| PathBuf::from("."));
                (t.space, cwd)
            };
            let id = self.spawn_pane(cfg, space, tab, &cfg.shell.clone(), &cwd)?;
            have.push(id);
        }

        let layout = Layout::preset(name, &have)
            .ok_or_else(|| anyhow!("could not build layout {name:?}"))?;
        if let Some(t) = self.tab_mut(tab) {
            t.layout = layout;
            t.focused_pane = have.first().copied();
        }
        self.view.zoom = None;
        self.relayout(cfg);
        Ok(())
    }

    // -- geometry ---------------------------------------------------------

    /// Recompute chrome and pane rects, then push the new sizes to the PTYs.
    ///
    /// Every structural change funnels through here, which is why a pane's PTY size and
    /// its drawn rect can never drift apart.
    pub fn relayout(&mut self, cfg: &Config) {
        let (cols, rows) = (self.client_cols, self.client_rows);
        let mut c = Chrome::default();

        let mut top = 0u16;
        let mut bottom = rows;
        // Only spend a row on the tab bar when the focused space actually has tabs.
        let has_tabs = self
            .focused_space
            .and_then(|s| self.space(s))
            .map(|s| !s.tabs.is_empty())
            .unwrap_or(false);
        if cfg.tab_bar && has_tabs && rows > 4 {
            c.tabbar = Rect::new(0, 0, cols, 1);
            top += 1;
        }
        if cfg.status_bar && rows > 4 {
            bottom -= 1;
            c.status = Rect::new(0, bottom, cols, 1);
        }

        let body_h = bottom.saturating_sub(top);
        let mut left = 0u16;
        let mut right = cols;

        // Panels only appear when there is room left for a usable pane area.
        if self.view.sidebar_open {
            let w = self.view.sidebar_width.min(cols / 3);
            if w >= 14 {
                c.sidebar = Rect::new(0, top, w, body_h);
                left += w;
            }
        }
        if self.view.bus_open {
            let w = self.view.bus_width.min(cols / 3);
            if right.saturating_sub(left) > w + 20 {
                right -= w;
                c.bus = Rect::new(right, top, w, body_h);
            }
        }

        c.panes = Rect::new(left, top, right.saturating_sub(left), body_h);
        self.chrome = c;

        // Push geometry into the panes of the focused tab, and resize the PTYs.
        let Some(tab_id) = self.focused_tab() else { return };
        let area = c.panes;
        let zoom = self.view.zoom;

        let assignments: Vec<(PaneId, Rect)> = match zoom {
            Some(z) if self.tab(tab_id).is_some_and(|t| t.layout.contains(z)) => {
                vec![(z, area)]
            }
            _ => self
                .tab(tab_id)
                .map(|t| {
                    let geo = t.layout.geometry(area);
                    geo.order.into_iter().filter_map(|p| geo.panes.get(&p).map(|r| (p, *r))).collect()
                })
                .unwrap_or_default(),
        };

        for (pane, cell) in assignments {
            let content = if cfg.pane_titles { cell.inset(1) } else { cell };
            if let Some(p) = self.panes.get_mut(&pane) {
                let _ = p.resize(content.w, content.h);
            }
        }
    }

    /// Cell and content rects for the panes currently on screen.
    pub fn visible_rects(&self, cfg: &Config) -> Vec<(PaneId, Rect, Rect)> {
        let Some(tab_id) = self.focused_tab() else { return Vec::new() };
        let area = self.chrome.panes;
        let cells: Vec<(PaneId, Rect)> = match self.view.zoom {
            Some(z) if self.tab(tab_id).is_some_and(|t| t.layout.contains(z)) => vec![(z, area)],
            _ => self
                .tab(tab_id)
                .map(|t| {
                    let geo = t.layout.geometry(area);
                    geo.order.into_iter().filter_map(|p| geo.panes.get(&p).map(|r| (p, *r))).collect()
                })
                .unwrap_or_default(),
        };
        cells
            .into_iter()
            .map(|(p, cell)| {
                let content = if cfg.pane_titles { cell.inset(1) } else { cell };
                (p, cell, content)
            })
            .collect()
    }

    // -- live handoff -----------------------------------------------------

    /// Rebuild this session from a predecessor's manifest and its transferred PTY masters.
    ///
    /// Panes are adopted rather than spawned: their processes never learn the daemon changed.
    pub fn import(
        &mut self,
        cfg: &Config,
        theme: &crate::theme::Theme,
        manifest: super::handoff::Manifest,
        fds: Vec<std::os::fd::OwnedFd>,
    ) -> Result<usize> {
        // Descriptors arrive in the manifest's pane order, so pair them up first.
        let mut fds: Vec<Option<std::os::fd::OwnedFd>> = fds.into_iter().map(Some).collect();
        // Manifest index to the pane id we give it here.
        let mut ids: Vec<Option<PaneId>> = vec![None; manifest.panes.len()];

        self.view = manifest.view;
        self.client_cols = manifest.client_cols;
        self.client_rows = manifest.client_rows;

        let mut space_ids = Vec::new();
        for hspace in &manifest.spaces {
            let space_id = self.next_space;
            self.next_space += 1;
            let name = self.unique_space_name(&hspace.name);
            self.spaces.push(Space {
                id: space_id,
                name,
                cwd: PathBuf::from(&hspace.cwd),
                tabs: Vec::new(),
                focused_tab: None,
            });
            space_ids.push(space_id);

            let mut tab_ids = Vec::new();
            for htab in &hspace.tabs {
                let tab_id = self.next_tab;
                self.next_tab += 1;
                self.tabs.push(Tab {
                    id: tab_id,
                    space: space_id,
                    name: htab.name.clone(),
                    layout: Layout::new(),
                    focused_pane: None,
                });
                if let Some(s) = self.space_mut(space_id) {
                    s.tabs.push(tab_id);
                }
                tab_ids.push(tab_id);

                // Adopt every pane this tab's tree refers to, then rebuild the tree.
                let mut leaves = Vec::new();
                collect_hleaves(&htab.tree, &mut leaves);
                for mi in leaves {
                    let Some(hpane) = manifest.panes.get(mi) else {
                        return Err(anyhow!("tree references pane {mi} which is not in the manifest"));
                    };
                    let Some(fd) = fds.get_mut(mi).and_then(|f| f.take()) else {
                        return Err(anyhow!("no descriptor for pane {mi}"));
                    };
                    let id = self.next_pane;
                    self.next_pane += 1;
                    let pane = Pane::adopt(
                        id,
                        tab_id,
                        space_id,
                        hpane,
                        fd,
                        cfg.scrollback,
                        theme,
                    )?;
                    self.panes.insert(id, pane);
                    ids[mi] = Some(id);
                }

                let tree = rebuild_hnode(&htab.tree, &ids)?;
                if let Some(t) = self.tab_mut(tab_id) {
                    t.layout = Layout::from_root(tree);
                    t.focused_pane = htab.focused_pane.and_then(|i| ids.get(i).copied().flatten());
                }
            }

            if let (Some(fi), Some(s)) = (hspace.focused_tab, self.space_mut(space_id)) {
                s.focused_tab = tab_ids.get(fi).copied().or(tab_ids.first().copied());
            } else if let Some(s) = self.space_mut(space_id) {
                s.focused_tab = tab_ids.first().copied();
            }
        }

        self.focused_space = manifest
            .focused_space
            .and_then(|i| space_ids.get(i).copied())
            .or_else(|| space_ids.first().copied());

        self.relayout(cfg);
        Ok(self.panes.len())
    }

    // -- snapshot ---------------------------------------------------------

    pub fn snapshot(&self, cfg: &Config) -> Snapshot {
        let rects: HashMap<PaneId, (Rect, Rect)> = self
            .visible_rects(cfg)
            .into_iter()
            .map(|(p, cell, content)| (p, (cell, content)))
            .collect();

        let panes = self
            .panes
            .values()
            .map(|p| {
                let (cell, content) = rects.get(&p.id).copied().unwrap_or_default();
                PaneInfo {
                    id: p.id,
                    tab: p.tab,
                    space: p.space,
                    title: p.display_name(),
                    cwd: p.cwd.to_string_lossy().to_string(),
                    cell,
                    content,
                    cols: p.cols,
                    rows: p.rows,
                    agent: p.agent.as_ref().map(|a| a.info()),
                    spawned_by: p.spawned_by,
                    exited: p.exited.is_some(),
                    scroll_offset: p.scroll_offset(),
                    wants_mouse: p.wants_mouse(),
                    bracketed_paste: p.bracketed_paste(),
                }
            })
            .collect();

        let tabs = self
            .tabs
            .iter()
            .map(|t| TabInfo {
                id: t.id,
                space: t.space,
                name: t.name.clone(),
                panes: t.layout.panes(),
                focused_pane: t.focused_pane,
            })
            .collect();

        let spaces = self
            .spaces
            .iter()
            .map(|s| {
                let mut agent_count = 0;
                let mut attention_count = 0;
                for &t in &s.tabs {
                    if let Some(tab) = self.tab(t) {
                        for p in tab.layout.panes() {
                            if let Some(a) = self.panes.get(&p).and_then(|p| p.agent.as_ref()) {
                                agent_count += 1;
                                if a.state.needs_attention() {
                                    attention_count += 1;
                                }
                            }
                        }
                    }
                }
                SpaceInfo {
                    id: s.id,
                    name: s.name.clone(),
                    cwd: s.cwd.to_string_lossy().to_string(),
                    tabs: s.tabs.clone(),
                    focused_tab: s.focused_tab,
                    agent_count,
                    attention_count,
                }
            })
            .collect();

        Snapshot {
            protocol: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            spaces,
            tabs,
            panes,
            focused_space: self.focused_space,
            focused_tab: self.focused_tab(),
            focused_pane: self.focused_pane(),
            view: self.view,
            sidebar: self.chrome.sidebar,
            bus: self.chrome.bus,
            status: self.chrome.status,
            tabbar: self.chrome.tabbar,
            // Filled in by the engine, which owns the board and the trigger set.
            tasks_open: 0,
            tasks_claimed: 0,
            triggers_armed: 0,
        }
    }
}

fn collect_hleaves(n: &super::handoff::HNode, out: &mut Vec<usize>) {
    use super::handoff::HNode;
    match n {
        HNode::Leaf(i) => out.push(*i),
        HNode::Split { a, b, .. } => {
            collect_hleaves(a, out);
            collect_hleaves(b, out);
        }
    }
}

fn rebuild_hnode(
    n: &super::handoff::HNode,
    ids: &[Option<PaneId>],
) -> Result<super::layout::Node> {
    use super::handoff::HNode;
    use super::layout::{Axis, Node};
    Ok(match n {
        HNode::Leaf(i) => Node::Leaf(
            ids.get(*i)
                .copied()
                .flatten()
                .ok_or_else(|| anyhow!("pane {i} was not adopted"))?,
        ),
        HNode::Split { horizontal, ratio, a, b } => Node::Split {
            // Reassigned by Layout::from_root.
            id: 0,
            axis: if *horizontal { Axis::Horizontal } else { Axis::Vertical },
            ratio: *ratio,
            a: Box::new(rebuild_hnode(a, ids)?),
            b: Box::new(rebuild_hnode(b, ids)?),
        },
    })
}

/// Agent state attached to a pane. Populated in phase 2; declared here because `Pane` and
/// the snapshot both refer to it.
#[derive(Debug, Clone)]
pub struct AgentRuntime {
    pub kind: String,
    pub name: String,
    pub state: crate::proto::AgentState,
    pub since: std::time::Instant,
    pub authority: String,
    pub reason: String,
    /// True once the user has looked at this pane since it finished.
    pub seen: bool,
    /// Native session id reported by an integration, used to resume after a restart.
    pub session_id: Option<String>,
    /// Messages held back because the agent was mid-stream.
    pub queued: Vec<crate::proto::Message>,
    /// Counted from lifecycle hooks.
    pub activity: crate::proto::Activity,
    /// Files touched this turn. Kept as a set so a file edited five times counts once.
    pub touched: std::collections::HashSet<String>,
    /// The `since` value at which this agent was last told about board work.
    ///
    /// Keyed on `since` rather than a timestamp so each idle period earns exactly one nudge:
    /// the value stops matching the moment the agent changes state, which is precisely when
    /// telling it again would be useful rather than noise.
    pub nudged_since: Option<std::time::Instant>,
    /// The `since` value at which an alert about this agent was last sent outside horde.
    ///
    /// Same key as `nudged_since`, for the same reason: one wait earns one notification. An
    /// agent still blocked an hour later is not news a second time, and an agent that blocks,
    /// gets answered, and blocks again is — which a plain "already told you about this one"
    /// flag would get backwards.
    pub alerted_since: Option<std::time::Instant>,
}

impl AgentRuntime {
    pub fn info(&self) -> crate::proto::AgentInfo {
        crate::proto::AgentInfo {
            kind: self.kind.clone(),
            name: self.name.clone(),
            state: self.state,
            elapsed: self.since.elapsed().as_secs(),
            authority: self.authority.clone(),
            reason: self.reason.clone(),
            activity: self.activity.clone(),
        }
    }

    /// Start of a new turn: the per-turn counters describe one turn, not a session.
    pub fn begin_turn(&mut self) {
        self.activity.turns += 1;
        self.activity.tools = 0;
        self.activity.files = 0;
        self.activity.errors = 0;
        self.activity.last_tool = None;
        self.touched.clear();
    }

    /// A tool call started. `file` is whatever path the tool was given, when it had one.
    pub fn record_tool(&mut self, tool: Option<&str>, file: Option<&str>) {
        self.activity.tools += 1;
        if let Some(t) = tool {
            self.activity.last_tool = Some(t.to_string());
        }
        if let Some(f) = file {
            if self.touched.insert(f.to_string()) {
                self.activity.files = self.touched.len() as u32;
            }
        }
    }

    pub fn record_error(&mut self) {
        self.activity.errors += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (Config, Session) {
        let mut cfg = Config::default();
        // `cat` starts instantly and produces nothing, keeping these tests quick.
        cfg.shell = "cat".into();
        let session = Session::new(&cfg);
        (cfg, session)
    }

    fn kill_all(s: &mut Session) {
        for p in s.panes.values_mut() {
            p.kill();
        }
    }

    /// Creating a space must take you there. Otherwise the next split or spawned agent goes
    /// into whichever space you were already in, and the new one sits empty.
    #[test]
    fn creating_a_space_focuses_it() {
        let (cfg, mut s) = session();
        let first = s.create_space(&cfg, Some("first"), &std::env::temp_dir()).unwrap();
        assert_eq!(s.focused_space, Some(first));

        let second = s.create_space(&cfg, Some("second"), &std::env::temp_dir()).unwrap();
        assert_eq!(s.focused_space, Some(second), "the new space should be focused");

        // And a split now lands in the new space rather than the old one.
        let pane = s.split(&cfg, None, Dir::Right, None).unwrap();
        assert_eq!(s.panes[&pane].space, second);
        kill_all(&mut s);
    }

    /// Every pane's pty must end up the size of the rect it is drawn in, or the program inside
    /// paints into a region that does not match its box — which is what a stale-looking pane in
    /// the corner of a big one actually is.
    #[test]
    fn every_pane_pty_matches_the_rect_it_is_drawn_in() {
        let (cfg, mut s) = session();
        s.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        s.split(&cfg, None, Dir::Right, None).unwrap();
        s.split(&cfg, None, Dir::Down, None).unwrap();

        for (cols, rows) in [(120u16, 40u16), (200, 60), (80, 24), (100, 30)] {
            s.set_client_size(&cfg, cols, rows);
            for (id, _cell, content) in s.visible_rects(&cfg) {
                let p = &s.panes[&id];
                assert_eq!(
                    (p.cols, p.rows),
                    (content.w, content.h),
                    "pane {id} at terminal {cols}x{rows}"
                );
            }
        }
        kill_all(&mut s);
    }

    /// The wobble that forces a program to repaint has to land back where it started. A redraw
    /// that left every pane one row short would be worse than the problem it solves.
    #[test]
    fn forcing_a_redraw_leaves_the_size_exactly_as_it_was() {
        let (cfg, mut s) = session();
        s.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        s.split(&cfg, None, Dir::Right, None).unwrap();
        s.set_client_size(&cfg, 140, 44);

        let before: Vec<(PaneId, u16, u16)> =
            s.panes.iter().map(|(id, p)| (*id, p.cols, p.rows)).collect();
        for p in s.panes.values_mut() {
            p.force_redraw().unwrap();
        }
        for (id, cols, rows) in before {
            let p = &s.panes[&id];
            assert_eq!((p.cols, p.rows), (cols, rows), "pane {id} came back a different size");
        }
        kill_all(&mut s);
    }

    /// Creating a tab focuses it too, for the same reason.
    #[test]
    fn creating_a_tab_focuses_it() {
        let (cfg, mut s) = session();
        let space = s.create_space(&cfg, Some("t"), &std::env::temp_dir()).unwrap();
        let tab = s.create_tab(&cfg, space, Some("logs")).unwrap();
        assert_eq!(s.focused_tab(), Some(tab));
        kill_all(&mut s);
    }

    #[test]
    fn space_names_stay_unique_so_they_remain_addressable() {
        let (cfg, mut s) = session();
        s.create_space(&cfg, Some("api"), &std::env::temp_dir()).unwrap();
        let b = s.create_space(&cfg, Some("api"), &std::env::temp_dir()).unwrap();
        assert_eq!(s.space(b).unwrap().name, "api-2");

        // Renaming onto a taken name is uniquified rather than creating a collision.
        let c = s.create_space(&cfg, Some("other"), &std::env::temp_dir()).unwrap();
        s.rename_space(c, "api");
        assert_eq!(s.space(c).unwrap().name, "api-3");
        // An empty rename is refused rather than leaving an unaddressable space.
        assert!(!s.rename_space(c, "   "));
        kill_all(&mut s);
    }
}
