//! The left panel: three independent sections.
//!
//! **SPACES** lists projects only — no nested panes. **AGENTS** lists every agent in the
//! session, wherever it lives, grouped under the project it belongs to. **SERVICES** does
//! the same for the dev servers, watchers and tunnels, and exists because putting them in
//! the agent list made a project running one agent and three `npm run dev` panes read as a
//! project running four agents. They are not the same kind of thing and you do not scan them
//! for the same reason: you read the agent list to find what needs you, and you read the
//! service list once, to check the things that should be up are up.
//!
//! Splitting them raises the question the split created — *which* servers belong to the work
//! you are looking at — so both lists draw a connector down their indent column in their
//! project's own accent, the same hue as that project's dot in SPACES. One project is one
//! coloured strand running the height of the panel, through two section rules, and the
//! servers on that strand are the ones serving those agents.
//!
//! The agent list used to be flat, on the argument that a single scannable list beats one you
//! assemble by reading down a tree. That holds for a handful of agents and stops holding at
//! the scale horde is for: with six projects running one to three agents each, a flat list of
//! a dozen names cannot answer "what is running for the API repo" at all — the only thing
//! separating one project's rows from another's was the dimming on rows outside the focused
//! space. Grouping restores that answer while keeping every agent visible wherever it lives,
//! which is the property the flat list was really protecting.
//!
//! *What* to show is decided in `client::roster`; this only draws it. That separation is what
//! lets the renderer and the click handler agree on what row 7 is without computing it twice.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::pane_widget::fmt_elapsed;
use super::{color, fill, logo, put_line, state_look, truncate, width};
use crate::client::roster::{filtered_rows, Density, Rail, Roll, RowKind, Section, Tally};
use crate::proto::{AgentState, LspState, PaneId, Rgb, Snapshot, SpaceId};
use crate::theme::Theme;

/// A clickable row, so the client can map a mouse position back to what it represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Space(SpaceId),
    Pane(PaneId),
    /// A group header in the AGENTS section. Distinct from `Space` because clicking it folds
    /// the group rather than switching to it.
    Group(SpaceId),
    /// A saved note. Clicking it picks it up; releasing over an agent hands it over.
    Memory(crate::proto::MemoryId),
}

/// Space rows to preserve when vertical room runs short.
const MIN_SPACE_ROWS: u16 = 2;
/// Agent rows to preserve before the space list gets any of what it asked for.
///
/// The agent list is the thing you actually watch, so it is the one that wins a squeeze.
const MIN_AGENT_ROWS: u16 = 3;
/// The rule and the label a section costs before any of its rows.
const AGENT_CHROME: u16 = 2;
/// Service rows to show before the rest need scrolling to.
///
/// Not a hard cap on the section — the cursor still walks into whatever is past it — but the
/// most standing room it may claim. A project with eight watchers up is a real thing and must
/// not be allowed to push the agent list, which is what you are actually watching, off the
/// panel to announce itself.
const MAX_SERVICE_ROWS: u16 = 6;
/// Memory rows to show before the rest need scrolling to.
///
/// Tighter than the service cap, and for the opposite reason. Services are a set you want
/// whole — four servers up means four rows, or the section is not answering "is everything
/// up". Notes accumulate for the life of a project and are listed newest first, so the tail
/// of the list is by construction the part you are least likely to want; a section that grew
/// with it would be a project's entire history holding the panel open.
const MAX_MEMORY_ROWS: u16 = 5;

pub struct Sidebar<'a> {
    pub snap: &'a Snapshot,
    /// Open and claimed counts from the task board, when there is any work on it.
    pub board: Option<(usize, usize)>,
    pub theme: &'a Theme,
    pub tick: usize,
    pub animate: bool,
    /// Cursor and scroll. Taken by `&mut` because both are clamped during render — how far
    /// the list can scroll, and whether the cursor is on screen, depend on a height only the
    /// renderer knows.
    pub state: &'a mut crate::client::roster::SidebarState,
    /// Whether the sidebar currently has the keyboard.
    pub focused: bool,
    /// Filled in during render so clicks can be resolved without recomputing layout.
    pub hits: &'a mut Vec<(u16, Hit)>,
}

impl Widget for Sidebar<'_> {
    fn render(self, area: TRect, buf: &mut Buffer) {
        if area.width < 8 || area.height < 6 {
            return;
        }
        let t = self.theme;
        fill(buf, area, t.ui.panel_bg);
        self.hits.clear();

        let inner_w = area.width.saturating_sub(2);
        let d = Density::of(inner_w);
        // Built the same way the cursor walks them, from the same function, so `j` can never
        // step onto a row the panel did not draw.
        let agents = filtered_rows(self.snap, d, &self.state.lens, Section::Agents);
        let memories = filtered_rows(self.snap, d, &self.state.lens, Section::Memory);
        let services = filtered_rows(self.snap, d, &self.state.lens, Section::Services);
        let total = agents.iter().filter(|r| matches!(r.kind, RowKind::Agent { .. })).count();
        // The order the cursor walks, which has to be the order `roster::cursor_rows` builds
        // or `j` steps onto rows the panel never drew.
        let combined: Vec<_> =
            agents.iter().chain(memories.iter()).chain(services.iter()).cloned().collect();

        let mut summary = summary_lines(self.snap);
        // The board belongs with the other standing counts: it is the same question, "is
        // there anything outstanding". Unclaimed work is the number that matters — once
        // everything is claimed there is nothing for you to hand out, so say so instead.
        if let Some((open, claimed)) = self.board {
            match (open, claimed) {
                (0, 0) => {}
                (0, c) => summary.push(("◇", c, "tasks claimed", AgentState::Unknown)),
                (o, _) => summary.push(("◇", o, "tasks open", AgentState::Unknown)),
            }
        }
        // Your own board's deadline, beside the agents'. Only what is due or already late —
        // a count of everything on it would be a number that never changes and so never
        // means anything, which is the failure of every badge that counts a whole inbox.
        if self.snap.cards_due > 0 {
            summary.push(("◈", self.snap.cards_due, "cards due", AgentState::Blocked));
        }
        // That horde is allowed to act on its own should never be something you have to
        // remember rather than see. Counted, not a bare word, because "which rules" is the
        // immediate next question and `horde trigger list` is the answer.
        if self.snap.triggers_armed > 0 {
            summary.push(("◈", self.snap.triggers_armed, "triggers armed", AgentState::Working));
        }

        // -- vertical budget -----------------------------------------------
        // The footer is reserved from the bottom, then the pinned blocks above it, and the
        // rest is split between the two top-down sections with the agent list taking its
        // minimum before spaces take any.
        //
        // MEMORY and SERVICES are reserved from the bottom rather than sharing the top-down
        // split for the same reason the footer is: they are standing facts you want to be able
        // to glance at without scrolling, and a block that only appears once you have scrolled
        // past a long agent list would be a block you forget exists.
        let bottom = area.y + area.height;
        let footer_h = if summary.is_empty() { 0 } else { summary.len() as u16 + 1 };
        let logo_h = logo::height(area.width, area.height);
        let top = area.y + logo_h + 1; // wordmark + rule

        let gross = bottom.saturating_sub(top).saturating_sub(footer_h);
        // The agent list wins a squeeze, but only over rows it would actually use. Its floor
        // is three rows *or however many it has*, whichever is fewer — a list holding a single
        // "none yet" row must not keep two blank ones open in front of it while the servers
        // below are being truncated to fit.
        let agent_floor = MIN_AGENT_ROWS.min(agents.len() as u16);
        let mut spare =
            gross.saturating_sub(AGENT_CHROME + agent_floor + MIN_SPACE_ROWS + 1);

        // The pinned blocks share one pool, and **SERVICES claims from it first** — which is
        // the opposite of the order they are drawn in, and deliberate.
        //
        // The two sections grow differently. A project's servers are a bounded set you want
        // whole: three of them is three rows, and the section either answers "is everything
        // up" or it does not. Notes accumulate for the life of a repository. Letting the
        // unbounded one claim first means a project with a year of notes squeezes the servers
        // off the panel entirely, taking a blocked dev server with them — so the bounded
        // section takes its two or three rows, and memory, which is listed newest-first and
        // degrades gracefully to its most recent, takes what is left.
        let mut height_of = |section: Section, rows: &[crate::client::roster::Row], cap: u16| {
            let want = if rows.is_empty() {
                0
            } else {
                (rows.len() as u16).min(cap) + AGENT_CHROME
            };
            // Below its own chrome plus a row there is nothing it could say, so it says
            // nothing at all rather than drawing a rule and a label over an empty block.
            let height = match want.min(spare) {
                n if n > AGENT_CHROME => n,
                _ => 0,
            };
            spare -= height;
            let _ = section;
            height
        };
        let service_h = height_of(Section::Services, &services, MAX_SERVICE_ROWS);
        let memory_h = height_of(Section::Memory, &memories, MAX_MEMORY_ROWS);

        // Bases are positions in the combined list, which is always agents → memory →
        // services whatever order the room was handed out in.
        let pinned: Vec<Pinned> = [
            (Section::Memory, &memories, agents.len(), memory_h),
            (Section::Services, &services, agents.len() + memories.len(), service_h),
        ]
        .into_iter()
        .filter(|(_, _, _, h)| *h > 0)
        .map(|(section, rows, base, height)| Pinned { section, rows, base, height })
        .collect();

        let taken: u16 = pinned.iter().map(|b| b.height).sum();
        let available = gross.saturating_sub(taken);
        let body = available.saturating_sub(AGENT_CHROME);
        let space_want = self.snap.spaces.len() as u16 + 1; // label + rows
        // Squeeze spaces first, but never below a couple of rows.
        let min_space = (MIN_SPACE_ROWS + 1).min(space_want);
        let space_h = space_want.min(body.saturating_sub(agent_floor).max(min_space)).min(body);
        let agent_h = body.saturating_sub(space_h);

        // -- header --------------------------------------------------------
        let mut y = area.y;
        y += logo::draw(buf, area.x, y, area.width, area.height, t);
        rule(buf, area.x, y, area.width, t);
        y += 1;

        // -- SPACES --------------------------------------------------------
        if space_h > 0 {
            let end = y + space_h;
            section_label(buf, area.x + 1, y, inner_w, "SPACES", None, t);
            y += 1;
            for space in &self.snap.spaces {
                if y >= end {
                    break;
                }
                let focused = self.snap.focused_space == Some(space.id);
                // Urgency outranks identity: a space that needs you says so before it says
                // which space it is. Otherwise the dot carries the project's own colour,
                // dimmed when it is not the one you are in.
                let dot = if space.attention_count > 0 {
                    t.ui.blocked
                } else if focused {
                    t.space_accent(space.accent)
                } else {
                    crate::theme::mix(t.space_accent(space.accent), t.ui.panel_bg, 0.45)
                };
                // The branch, and the agent count only where both fit.
                //
                // The branch wins the narrow case because the count is already said twice
                // over — the AGENTS group header below carries a rollup for this same space —
                // whereas which branch a project is on is said nowhere else, and is the one
                // thing here that changes without anyone touching horde.
                //
                // Budgeted to a third of the panel because `row` pays for the detail out of
                // the label: an unbounded branch name would eat the project's own name, which
                // is the one thing on the row that must always be readable.
                let badge = match (&space.repo, space.agent_count) {
                    (Some(r), n) if d != Density::Tight => {
                        let budget = (inner_w as usize / 3).clamp(4, 14);
                        let dirty = if r.dirty { "*" } else { "" };
                        let count =
                            if n > 0 && d == Density::Wide { format!(" {n}") } else { String::new() };
                        format!("{}{dirty}{count}", truncate(&r.branch, budget))
                    }
                    (_, n) if n > 0 => n.to_string(),
                    _ => String::new(),
                };
                row(
                    buf,
                    area.x,
                    y,
                    area.width,
                    RowSpec {
                        marker: if focused { "▎" } else { " " },
                        indent: 0,
                        rail: Rail::None,
                        rail_color: t.ui.text_faint,
                        glyph: if space.attention_count > 0 { "●" } else { "○" },
                        glyph_color: dot,
                        label: &space.name,
                        label_color: if focused { t.ui.text } else { t.ui.text_dim },
                        bold: focused,
                        detail: plain(&badge, t.ui.text_faint),
                        bg: t.ui.panel_bg,
                    },
                    t,
                );
                self.hits.push((y, Hit::Space(space.id)));
                y += 1;

                // Language servers, under the project they serve.
                //
                // Here rather than folded into the badge because this is a child process
                // holding hundreds of megabytes that horde started on your behalf, and the
                // rule is that nothing horde runs is invisible. A diamond for the same reason
                // a dev server gets one: it is up and holding rather than mid-turn.
                for l in &space.lsp {
                    if y >= end {
                        break;
                    }
                    let (glyph, colour) = match l.state {
                        LspState::Ready => ("◆", t.ui.serving),
                        LspState::Starting => ("◌", t.ui.working),
                        LspState::Waiting => ("◍", t.ui.blocked),
                        LspState::Failed => ("✕", t.ui.blocked),
                    };
                    // Counts when it is working, the reason when it is not. Both at once
                    // would not fit, and a server that is down has no counts worth reading.
                    let badge = match l.state {
                        LspState::Ready if l.errors > 0 || l.warnings > 0 => {
                            let mut b = String::new();
                            if l.errors > 0 {
                                b.push_str(&format!("{}◍", l.errors));
                            }
                            if l.warnings > 0 {
                                b.push_str(&format!(" {}△", l.warnings));
                            }
                            b
                        }
                        LspState::Ready => String::new(),
                        _ => {
                            let budget = (inner_w as usize / 2).clamp(4, 18);
                            truncate(l.detail.as_deref().unwrap_or("down"), budget)
                        }
                    };
                    row(
                        buf,
                        area.x,
                        y,
                        area.width,
                        RowSpec {
                            marker: " ",
                            indent: 1,
                            rail: Rail::None,
                            rail_color: t.ui.text_faint,
                            glyph,
                            glyph_color: colour,
                            label: &l.lang,
                            label_color: t.ui.text_faint,
                            bold: false,
                            detail: plain(&badge, t.ui.text_faint),
                            bg: t.ui.panel_bg,
                        },
                        t,
                    );
                    // Clicking it selects the project, which is the only thing there is to do
                    // with a language server from here.
                    self.hits.push((y, Hit::Space(space.id)));
                    y += 1;
                }
            }
            y = end;
        }

        // Keep the cursor on screen. Done here rather than in the key handler because the
        // handler has no idea how many rows fit — the same reason scroll is clamped here.
        //
        // Only once there *is* a cursor: until the panel has been given the keyboard, the
        // wheel owns the scroll, and a cursor nobody asked for would drag it back to the top
        // on the next frame.
        let cursor_at =
            self.state.cursor.is_some().then(|| self.state.resolve(&combined)).flatten();

        let draw = ListDraw {
            snap: self.snap,
            theme: t,
            tick: self.tick,
            animate: self.animate,
            cursor: if self.focused { cursor_at } else { None },
        };

        // -- AGENTS --------------------------------------------------------
        if available >= AGENT_CHROME {
            rule(buf, area.x, y, area.width, t);
            y += 1;

            // Window the list. Nothing is dropped without saying so: what does not fit is
            // still reachable by scrolling, and the label carries how much of it you can see.
            let room = agent_h as usize;
            if let Some(at) = cursor_at.filter(|at| *at < agents.len()) {
                if at < self.state.scroll {
                    self.state.scroll = at;
                } else if room > 0 && at >= self.state.scroll + room {
                    self.state.scroll = at + 1 - room;
                }
            }
            self.state.scroll = self.state.scroll.min(agents.len().saturating_sub(room));
            let from = self.state.scroll.min(agents.len());
            let window = &agents[from..(from + room).min(agents.len())];
            let shown = window.iter().filter(|r| matches!(r.kind, RowKind::Agent { .. })).count();
            let counter = (d.shows_counter() && shown < total).then(|| format!("{shown}/{total}"));
            // The lens outranks the counter for the columns available: a list that does not
            // say it is filtered reads as a broken one, whereas a missing fraction only costs
            // you knowing how much is off screen.
            let lens = self.state.lens.label();
            let note = match (d, lens.is_empty()) {
                (_, false) if d == Density::Wide => Some(match &counter {
                    Some(c) => format!("{lens} {c}"),
                    None => lens.clone(),
                }),
                (_, false) => Some(lens.clone()),
                _ => counter.clone(),
            };

            section_label(buf, area.x + 1, y, inner_w, "AGENTS", note.as_deref(), t);
            y += 1;
            draw.list(buf, area, y, window, from, self.hits);
        }

        // -- MEMORY, then SERVICES -----------------------------------------
        //
        // Anchored to the bottom rather than drawn where the agent list happened to stop, so
        // the sections do not slide up and down the panel as agents come and go. A list you
        // have to re-find every time you look at it is one you stop looking at.
        //
        // Laid out upwards from the footer so the *last* section keeps the same seat whatever
        // the one above it does: SERVICES sits on the footer, MEMORY rests on SERVICES.
        let mut floor = bottom - footer_h;
        for b in pinned.iter().rev() {
            let mut by = floor - b.height;
            floor = by;
            rule(buf, area.x, by, area.width, t);
            by += 1;

            let room = b.height.saturating_sub(AGENT_CHROME) as usize;
            // No stored scroll of its own: these blocks are short by construction, so the only
            // reason to be looking past their first rows is that the cursor is down there, and
            // that is enough to derive the offset from. State nobody has to invalidate.
            let inside = cursor_at.filter(|at| *at >= b.base).map(|at| at - b.base);
            let from = match inside {
                Some(i) if room > 0 && i >= room => i + 1 - room,
                _ => 0,
            };
            let window = &b.rows[from..(from + room).min(b.rows.len())];
            let live = b.rows.iter().filter(|r| r.focus().is_some()).count();
            let shown = window.iter().filter(|r| r.focus().is_some()).count();
            // The count either way: a section that is showing everything still benefits from
            // saying how many, and one that is not has to say so or it reads as a short list
            // rather than a truncated one.
            let note = d.shows_counter().then(|| {
                if shown < live {
                    format!("{shown}/{live}")
                } else {
                    live.to_string()
                }
            });

            section_label(buf, area.x + 1, by, inner_w, b.section.label(), note.as_deref(), t);
            by += 1;
            draw.list(buf, area, by, window, b.base + from, self.hits);
        }

        // -- footer --------------------------------------------------------
        if footer_h > 0 {
            let mut fy = bottom - footer_h;
            rule(buf, area.x, fy, area.width, t);
            fy += 1;
            for (glyph, count, label, which) in summary {
                let c = match which {
                    AgentState::Blocked => t.ui.blocked,
                    AgentState::Done => t.ui.done,
                    AgentState::Working => t.ui.working,
                    AgentState::Serving => t.ui.serving,
                    _ => t.ui.idle,
                };
                put_line(
                    buf,
                    area.x + 1,
                    fy,
                    inner_w,
                    Line::from(vec![
                        Span::styled(
                            format!("{glyph} "),
                            Style::default().fg(color(c)).bg(color(t.ui.panel_bg)),
                        ),
                        Span::styled(
                            format!("{count} {label}"),
                            Style::default()
                                .fg(color(t.ui.text_dim))
                                .bg(color(t.ui.panel_bg)),
                        ),
                    ]),
                );
                fy += 1;
            }
        }
    }
}

/// A section reserved from the bottom of the panel, with the room it was granted.
struct Pinned<'a> {
    section: Section,
    rows: &'a [crate::client::roster::Row],
    /// Combined-list index of `rows[0]`, so the cursor can be located in it.
    base: usize,
    /// Its rule and label included.
    height: u16,
}

/// Draws one windowed list of rows: the AGENTS body, or one of the pinned blocks below it.
///
/// One copy, three callers. The sections differ in where they sit and what their heading
/// says, and in nothing else — same row shapes, same rails, same hit records — so a second
/// copy of this would be a second thing to keep in step with `roster::filtered_rows`.
struct ListDraw<'a> {
    snap: &'a Snapshot,
    theme: &'a Theme,
    tick: usize,
    animate: bool,
    /// Index *in the combined list* of the row the cursor is on, when the panel has the
    /// keyboard. Combined rather than per-section because that is the list the cursor walks,
    /// and converting per-section here would mean two places agreeing on the split.
    cursor: Option<usize>,
}

impl ListDraw<'_> {
    /// `base` is the combined-list index of `window[0]`. Returns the first row below the list.
    fn list(
        &self,
        buf: &mut Buffer,
        area: TRect,
        mut y: u16,
        window: &[crate::client::roster::Row],
        base: usize,
        hits: &mut Vec<(u16, Hit)>,
    ) -> u16 {
        let t = self.theme;
        let inner_w = area.width.saturating_sub(2);
        for (i, r) in window.iter().enumerate() {
            let on_cursor = self.cursor == Some(base + i);
            // Every row on a project's strand is drawn in that project's own accent, dimmed
            // hard: the connector has to be legible as a *group* out of the corner of your
            // eye and invisible when you are reading a name. A rail at full accent would
            // compete with the state glyph, which is the one thing on the row that is
            // allowed to shout.
            let rail_color = self.tint(r.tint);
            match &r.kind {
                RowKind::Group { space, tally, collapsed } => {
                    let Some(sp) = self.snap.spaces.iter().find(|s| s.id == *space) else {
                        continue;
                    };
                    let focused = self.snap.focused_space == Some(*space);
                    row(
                        buf,
                        area.x,
                        y,
                        area.width,
                        RowSpec {
                            marker: if on_cursor { "▎" } else { " " },
                            indent: r.indent,
                            rail: r.rail,
                            rail_color,
                            // A disclosure marker, so a folded group reads as folded rather
                            // than as a project that lost its agents — carrying the project's
                            // accent, because this is the head of the strand its rows hang
                            // from and the colour is what ties the two sections together.
                            glyph: if *collapsed { "▸" } else { "▾" },
                            glyph_color: rail_color,
                            label: &sp.name,
                            label_color: if focused { t.ui.text } else { t.ui.text_dim },
                            bold: true,
                            detail: rollup(*tally, Density::of(inner_w), t),
                            bg: if on_cursor { t.ui.title_bg } else { t.ui.panel_bg },
                        },
                        t,
                    );
                    hits.push((y, Hit::Group(*space)));
                }
                RowKind::Agent { pane, space, pinned } => {
                    let Some(a) = self
                        .snap
                        .panes
                        .iter()
                        .find(|p| p.id == *pane)
                        .and_then(|p| p.agent.as_ref())
                    else {
                        continue;
                    };
                    let (glyph, c) = state_look(a.state, t, self.tick, self.animate);
                    let is_focused = self.snap.focused_pane == Some(*pane);
                    // A pinned row sits outside its group, so it has to say why it is there —
                    // otherwise it reads as an agent in the wrong place.
                    let marker = if *pinned { "▪" } else { " " };
                    let here = self.snap.focused_space == Some(*space);
                    // What the row has to add beyond its glyph. A service's is its address:
                    // "serving" is a fact the glyph already carried, and `:5173` is the thing
                    // you actually wanted. It falls back to the state word when the screen
                    // never said — see `daemon::endpoint`, which declines rather than guesses.
                    let detail = match (a.state, a.endpoint.as_deref()) {
                        // `blocked` outranks the address, and it is the only thing that does.
                        // A port that cannot be bound is still a port, so the row would read
                        // as perfectly healthy in the one state where it is not — and the
                        // word is the whole call to action.
                        (AgentState::Blocked, _) => AgentState::Blocked.label().to_string(),
                        (_, Some(e)) => e.to_string(),
                        (AgentState::Working, _) => fmt_elapsed(a.elapsed),
                        (state, _) => state.label().to_string(),
                    };
                    row(
                        buf,
                        area.x,
                        y,
                        area.width,
                        RowSpec {
                            marker: if is_focused || on_cursor { "▎" } else { marker },
                            indent: r.indent,
                            rail: r.rail,
                            rail_color,
                            glyph: &glyph,
                            glyph_color: c,
                            label: &a.name,
                            // An agent elsewhere in the session reads dimmer, but is still here.
                            label_color: if !here {
                                t.ui.text_faint
                            } else if is_focused {
                                t.ui.text
                            } else {
                                t.ui.text_dim
                            },
                            bold: is_focused,
                            detail: plain(&detail, c),
                            bg: if is_focused || on_cursor {
                                t.ui.title_bg
                            } else {
                                t.ui.panel_bg
                            },
                        },
                        t,
                    );
                    hits.push((y, Hit::Pane(*pane)));
                }
                RowKind::Memory { id, space } => {
                    let Some(m) = self.snap.memories.iter().find(|m| m.id == *id) else {
                        continue;
                    };
                    let here = self.snap.focused_space == Some(*space);
                    // The name, not the title. A note is *addressed* by its name — that is
                    // what you type at `horde memory show` and what an agent will see in the
                    // path you hand it — and a row showing prose you cannot type would leave
                    // you hunting for the filename it stood for. The title has room in the
                    // roster and in the drag banner, neither of which is twenty columns wide.
                    row(
                        buf,
                        area.x,
                        y,
                        area.width,
                        RowSpec {
                            marker: if on_cursor { "▎" } else { " " },
                            indent: r.indent,
                            rail: r.rail,
                            rail_color,
                            // Neither a circle nor a diamond: a note is not in the agents'
                            // state cycle and it is not a process either.
                            glyph: "◈",
                            glyph_color: t.ui.text_dim,
                            label: &m.name,
                            label_color: if here { t.ui.text_dim } else { t.ui.text_faint },
                            bold: false,
                            // How long ago it was written, which is the one thing that tells
                            // you whether it is still true.
                            detail: plain(&fmt_elapsed(m.age), t.ui.text_faint),
                            bg: if on_cursor { t.ui.title_bg } else { t.ui.panel_bg },
                        },
                        t,
                    );
                    hits.push((y, Hit::Memory(*id)));
                }
                // What it is doing, indented under the name. Hooks only — screen detection
                // cannot see tool calls.
                RowKind::Activity(pane) => {
                    let Some(act) = self
                        .snap
                        .panes
                        .iter()
                        .find(|p| p.id == *pane)
                        .and_then(|p| p.agent.as_ref())
                        .and_then(|a| a.activity.summary())
                    else {
                        continue;
                    };
                    // The strand runs past this row rather than stopping at it: an agent with
                    // a second line must not break the connector to everything below it.
                    let rail = r.rail.glyph();
                    let pad = (r.indent as usize).saturating_sub(width(rail));
                    put_line(
                        buf,
                        area.x,
                        y,
                        area.width,
                        Line::from(vec![
                            Span::styled(
                                rail.to_string(),
                                Style::default().fg(color(rail_color)).bg(color(t.ui.panel_bg)),
                            ),
                            Span::styled(
                                " ".repeat(pad),
                                Style::default().bg(color(t.ui.panel_bg)),
                            ),
                            Span::styled(
                                truncate(&act, inner_w.saturating_sub(r.indent) as usize),
                                Style::default()
                                    .fg(color(t.ui.text_faint))
                                    .bg(color(t.ui.panel_bg)),
                            ),
                        ]),
                    );
                }
                RowKind::Empty(msg) => {
                    // Say how to get one rather than leaving an unexplained empty panel.
                    put_line(
                        buf,
                        area.x + 2,
                        y,
                        inner_w,
                        Line::from(vec![Span::styled(
                            *msg,
                            Style::default().fg(color(t.ui.text_faint)).bg(color(t.ui.panel_bg)),
                        )]),
                    );
                }
            }
            y += 1;
        }
        y
    }

    /// The colour of a project's strand: its own accent, dimmed towards the panel.
    ///
    /// Two depths, for the same reason the SPACES dot has two: the project you are in is the
    /// one whose connectors you are actually tracing, and the other five projects' strands
    /// have to be present without being read.
    fn tint(&self, space: Option<crate::proto::SpaceId>) -> Rgb {
        let t = self.theme;
        let Some(sp) = space.and_then(|id| self.snap.spaces.iter().find(|s| s.id == id)) else {
            return t.ui.text_faint;
        };
        let k = if self.snap.focused_space == Some(sp.id) { 0.35 } else { 0.62 };
        crate::theme::mix(t.space_accent(sp.accent), t.ui.panel_bg, k)
    }
}

/// A single-colour detail, which is what most rows have.
fn plain(text: &str, c: Rgb) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    vec![Span::styled(text.to_string(), Style::default().fg(color(c)))]
}

/// A group header's tally.
///
/// Multi-span rather than one string because the whole point is that `◍1` and `◐2` are
/// different colours; flattening them to a single style would lose the only thing that makes
/// the rollup readable at a glance. A plain count has nothing to colour by and takes the
/// faint text the other right-aligned details use.
fn rollup(tally: Tally, d: Density, t: &Theme) -> Vec<Span<'static>> {
    let roll = match tally {
        Tally::States(r) => r,
        Tally::Count(n) => return plain(&n.to_string(), t.ui.text_faint),
    };
    let mut out = Vec::new();
    for (state, count) in roll.compact(d) {
        let (_, c) = state_look(state, t, 0, false);
        if !out.is_empty() {
            out.push(Span::styled(" ".to_string(), Style::default()));
        }
        out.push(Span::styled(
            format!("{}{}", state.glyph(), count),
            Style::default().fg(color(c)),
        ));
    }
    out
}

struct RowSpec<'a> {
    /// The one-cell gutter: focus bar, pin mark, or nothing.
    marker: &'a str,
    /// Columns of indent after the marker, so a grouped agent sits under its header.
    indent: u16,
    /// The connector drawn into the first two columns of that indent.
    rail: Rail,
    /// Its project's accent. Separate from `glyph_color` because the two say different
    /// things: the rail says which project, the glyph says what state.
    rail_color: Rgb,
    glyph: &'a str,
    glyph_color: Rgb,
    label: &'a str,
    label_color: Rgb,
    bold: bool,
    /// Right-aligned, already styled apart from its background.
    detail: Vec<Span<'static>>,
    bg: Rgb,
}

/// One sidebar row: focus marker, indent, glyph, label, right-aligned detail.
///
/// The only place row arithmetic lives. Every row in the panel comes through here, so a
/// column budget that is right once is right everywhere.
fn row(buf: &mut Buffer, x: u16, y: u16, w: u16, spec: RowSpec<'_>, t: &Theme) {
    let inner = w.saturating_sub(2);
    let detail_w: u16 = spec.detail.iter().map(|s| width(&s.content) as u16).sum();
    // 1 marker + indent + 1 glyph + 1 space, then the label, then the detail flush right.
    let label_room = inner.saturating_sub(3 + spec.indent + detail_w + 1);
    let label = truncate(spec.label, label_room as usize);
    let pad = inner.saturating_sub(3 + spec.indent + width(&label) as u16 + detail_w);

    let bg = Style::default().bg(color(spec.bg));
    let mut label_style = bg.fg(color(spec.label_color));
    if spec.bold {
        label_style = label_style.add_modifier(Modifier::BOLD);
    }

    // The rail is drawn *into* the indent rather than before it, so a row's name starts in
    // the same column whether it has a connector or not. A tree whose labels do not line up
    // is harder to scan than no tree at all.
    let rail = spec.rail.glyph();
    let rail_pad = (spec.indent as usize).saturating_sub(width(rail));
    let mut spans = vec![
        Span::styled(spec.marker.to_string(), bg.fg(color(t.ui.accent))),
        Span::styled(rail.to_string(), bg.fg(color(spec.rail_color))),
        Span::styled(" ".repeat(rail_pad), bg),
        Span::styled(format!("{} ", spec.glyph), bg.fg(color(spec.glyph_color))),
        Span::styled(label, label_style),
        Span::styled(" ".repeat(pad as usize), bg),
    ];
    // The detail carries its own colour but inherits the row's background, so a focused row
    // stays one unbroken band.
    spans.extend(spec.detail.into_iter().map(|s| {
        let fg = s.style.fg;
        Span::styled(s.content, Style::default().patch(bg).fg(fg.unwrap_or(color(t.ui.text_dim))))
    }));

    put_line(buf, x, y, w, Line::from(spans));
}

/// A section heading, with an optional right-aligned note.
fn section_label(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    w: u16,
    text: &str,
    note: Option<&str>,
    t: &Theme,
) {
    let base = Style::default().bg(color(t.ui.panel_bg));
    let mut spans = vec![Span::styled(
        text.to_string(),
        base.fg(color(t.ui.text_faint)).add_modifier(Modifier::BOLD),
    )];
    // How much of the list you can see rides on the heading rather than costing a row of its
    // own — the panel's scarcest resource is rows, and this is the least important thing in it.
    if let Some(note) = note {
        let pad = (w as usize).saturating_sub(width(text) + width(note));
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), base));
            spans.push(Span::styled(note.to_string(), base.fg(color(t.ui.text_faint))));
        }
    }
    put_line(buf, x, y, w, Line::from(spans));
}

/// Counts worth standing space at the bottom. Only non-zero rows appear, so a quiet session
/// shows nothing rather than three zeroes.
fn summary_lines(snap: &Snapshot) -> Vec<(&'static str, usize, &'static str, AgentState)> {
    let mut blocked = 0;
    let mut done = 0;
    let mut working = 0;
    for p in &snap.panes {
        match p.agent.as_ref().map(|a| a.state) {
            Some(AgentState::Blocked) => blocked += 1,
            Some(AgentState::Done) => done += 1,
            Some(AgentState::Working) => working += 1,
            _ => {}
        }
    }
    let mut out = Vec::new();
    if blocked > 0 {
        out.push((AgentState::Blocked.glyph(), blocked, "needs you", AgentState::Blocked));
    }
    if done > 0 {
        out.push((AgentState::Done.glyph(), done, "done", AgentState::Done));
    }
    if working > 0 {
        out.push((AgentState::Working.glyph(), working, "working", AgentState::Working));
    }
    out
}

fn rule(buf: &mut Buffer, x: u16, y: u16, w: u16, t: &Theme) {
    let style = Style::default().fg(color(t.ui.border)).bg(color(t.ui.panel_bg));
    for i in 0..w {
        if let Some(c) = buf.cell_mut((x + i, y)) {
            c.set_symbol("─");
            c.set_style(style);
        }
    }
}

#[cfg(test)]
mod tests {

    /// The project's own name must survive a long branch. `row` pays for the detail out of
    /// the label, so an unbudgeted branch is a project called `Hal`.
    #[test]
    fn a_long_branch_never_eats_the_project_name() {
        let mut snap = crate::client::roster::tests::snap();
        snap.spaces[0].name = "Halo Suite".into();
        snap.spaces[0].repo = Some(crate::proto::RepoInfo {
            branch: "horde/a-very-long-branch-name".into(),
            dirty: true,
            worktree: false,
        });
        let (out, _) = render(&snap, 24, 30);
        assert!(out.contains("Halo Suite"), "{out}");
    }
    use super::*;
    use crate::client::roster::tests::{pane, snap};
    use crate::proto::PaneInfo;

    fn render(s: &Snapshot, w: u16, h: u16) -> (String, Vec<(u16, Hit)>) {
        render_at(s, w, h, None, 0)
    }

    fn render_board(
        s: &Snapshot,
        w: u16,
        h: u16,
        board: Option<(usize, usize)>,
    ) -> (String, Vec<(u16, Hit)>) {
        render_at(s, w, h, board, 0)
    }

    fn render_at(
        s: &Snapshot,
        w: u16,
        h: u16,
        board: Option<(usize, usize)>,
        scroll: usize,
    ) -> (String, Vec<(u16, Hit)>) {
        let mut st = crate::client::roster::SidebarState { scroll, ..Default::default() };
        render_state(s, w, h, board, &mut st, false)
    }

    /// The drawn buffer, for the assertions that are about colour rather than text.
    fn render_buffer(s: &Snapshot, w: u16, h: u16) -> Buffer {
        let area = TRect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        let mut hits = Vec::new();
        let mut st = crate::client::roster::SidebarState::default();
        Sidebar {
            snap: s,
            theme: &theme,
            tick: 0,
            animate: false,
            hits: &mut hits,
            state: &mut st,
            focused: false,
            board: None,
        }
        .render(area, &mut buf);
        buf
    }

    /// The row a name was drawn on.
    fn row_of(buf: &Buffer, w: u16, h: u16, name: &str) -> u16 {
        (0..h)
            .find(|y| {
                let line: String =
                    (0..w).map(|x| buf.cell((x, *y)).unwrap().symbol()).collect();
                line.contains(name)
            })
            .unwrap_or_else(|| panic!("no row for {name:?}"))
    }

    fn render_state(
        s: &Snapshot,
        w: u16,
        h: u16,
        board: Option<(usize, usize)>,
        state: &mut crate::client::roster::SidebarState,
        focused: bool,
    ) -> (String, Vec<(u16, Hit)>) {
        let area = TRect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        let mut hits = Vec::new();
        Sidebar {
            snap: s,
            theme: &theme,
            tick: 0,
            animate: false,
            hits: &mut hits,
            state,
            focused,
            board,
        }
        .render(area, &mut buf);
        let text = (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (text, hits)
    }

    /// A session with servers up, so the strand can be looked at rather than reasoned about.
    fn served() -> Snapshot {
        let mut s = snap();
        let mut vite = pane(5, 1, 1, Some(("vite", AgentState::Serving)));
        service(&mut vite, Some(":5173"));
        let mut tsc = pane(6, 1, 1, Some(("tsc", AgentState::Serving)));
        service(&mut tsc, None);
        let mut hugo = pane(7, 2, 2, Some(("hugo", AgentState::Blocked)));
        service(&mut hugo, Some(":1313"));
        s.tabs[0].panes.extend([5, 6]);
        s.tabs[1].panes.push(7);
        s.panes.extend([vite, tsc, hugo]);
        s
    }

    fn service(p: &mut PaneInfo, endpoint: Option<&str>) {
        let a = p.agent.as_mut().unwrap();
        a.class = crate::proto::AgentClass::Service;
        a.endpoint = endpoint.map(str::to_string);
    }

    /// The whole reason the section exists: a project running one agent and three
    /// `npm run dev` panes must not read as a project running four agents.
    #[test]
    fn dev_servers_get_their_own_section_below_the_agents() {
        let (out, _) = render(&served(), 26, 26);
        println!("\n{out}\n");
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("no {s:?} in\n{out}"));
        assert!(at("AGENTS") < at("SERVICES"), "{out}");
        assert!(at("SERVICES") < at("vite"), "{out}");
        // The agent list is above the SERVICES rule and holds none of them.
        let (agents, services) = out.split_at(at("SERVICES"));
        assert!(agents.contains("builder") && agents.contains("reviewer"), "{out}");
        assert!(!agents.contains("vite") && !agents.contains("tsc"), "{out}");
        assert!(services.contains("hugo"), "{out}");
    }

    /// The agent list wins a squeeze, but only over rows it would actually use. A session
    /// of nothing but dev servers must not keep three rows open in front of "none yet"
    /// while the servers below are truncated to fit.
    #[test]
    fn an_empty_agent_list_does_not_hold_rows_open_it_cannot_use() {
        let mut only = served();
        only.panes.retain(|p| p.agent.is_none() || p.id >= 5);
        let (out, _) = render(&only, 26, 22);
        assert!(out.contains("none yet"), "{out}");
        for name in ["vite", "tsc", "hugo"] {
            assert!(out.contains(name), "{name} was squeezed out of\n{out}");
        }
    }

    /// One project, one collapse decision. Folding it in either section folds it in both,
    /// because "I am not interested in this project right now" is one thing to have said.
    #[test]
    fn folding_a_project_folds_it_in_both_sections() {
        let mut s = served();
        s.spaces[0].collapsed = true;
        let (out, _) = render(&s, 26, 26);
        assert!(!out.contains("builder"), "{out}");
        assert!(!out.contains("vite"), "{out}");
        // Both headers stay, and both say folded rather than emptied.
        assert_eq!(out.matches("▸ api-refactor").count(), 2, "{out}");
        assert!(out.contains("writer") && out.contains("hugo"), "docs is untouched:\n{out}");
    }

    /// The session used for the memory assertions: servers up, and three saved notes.
    fn noted() -> Snapshot {
        let mut s = served();
        let note = |id, space, name: &str| crate::proto::MemoryInfo {
            id,
            space,
            name: name.into(),
            title: format!("about {name}"),
            bytes: 100,
            age: id * 600,
        };
        s.memories = vec![note(1, 1, "api-shape"), note(2, 1, "ruled-out"), note(3, 2, "voice")];
        s
    }

    #[test]
    fn notes_get_their_own_section_between_the_agents_and_the_servers() {
        let (out, _) = render(&noted(), 26, 32);
        println!("\n{out}\n");
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("no {s:?} in\n{out}"));
        assert!(at("AGENTS") < at("MEMORY"), "{out}");
        assert!(at("MEMORY") < at("SERVICES"), "{out}");
        assert!(out.contains("api-shape") && out.contains("voice"), "{out}");
    }

    /// The name, not the title: a note is *addressed* by its name, and a row showing prose you
    /// cannot type would leave you hunting for the filename it stood for.
    #[test]
    fn a_note_row_shows_the_name_you_would_type() {
        let (out, _) = render(&noted(), 26, 32);
        assert!(out.contains("api-shape"), "{out}");
        assert!(!out.contains("about api-shape"), "the title took the row: {out}");
    }

    #[test]
    fn a_session_with_no_notes_spends_no_rows_saying_so() {
        let (out, _) = render(&served(), 26, 30);
        assert!(!out.contains("MEMORY"), "{out}");
    }

    /// Notes accumulate for the life of a project and servers do not, so letting the unbounded
    /// section claim first would eventually push a blocked dev server off the panel.
    #[test]
    fn a_squeeze_drops_the_notes_before_it_drops_the_servers() {
        let (out, _) = render(&noted(), 26, 22);
        println!("\n{out}\n");
        assert!(out.contains("SERVICES"), "{out}");
        assert!(out.contains("vite"), "{out}");
        assert!(!out.contains("MEMORY"), "{out}");
    }

    /// A note is a pane-less row, so the hit table has to carry its own kind of target or the
    /// drag has nothing to pick up.
    #[test]
    fn a_note_row_is_clickable() {
        let (out, hits) = render(&noted(), 26, 32);
        let y = out.lines().position(|l| l.contains("api-shape")).unwrap() as u16;
        assert_eq!(hits.iter().find(|(hy, _)| *hy == y).map(|(_, h)| *h), Some(Hit::Memory(1)));
    }

    /// Most sessions never run a dev server, and a rule and a label announcing that would be
    /// three rows spent on a non-event.
    #[test]
    fn a_session_with_no_servers_spends_no_rows_saying_so() {
        let (out, _) = render(&snap(), 26, 26);
        assert!(!out.contains("SERVICES"), "{out}");
    }

    /// "serving" is a fact the glyph already carried; the port is the thing you wanted.
    #[test]
    fn a_service_row_says_where_it_is_answering() {
        let (out, _) = render(&served(), 26, 26);
        assert!(out.contains(":5173"), "{out}");
        // And falls back to the state word when the screen never said, rather than to blank.
        assert!(out.contains("serving"), "{out}");
    }

    /// A port that cannot be bound is still a port, so the row would read as perfectly
    /// healthy in the one state where it is not.
    #[test]
    fn a_blocked_service_says_blocked_rather_than_its_port() {
        let mut s = served();
        s.panes.iter_mut().find(|p| p.id == 5).unwrap().agent.as_mut().unwrap().state =
            AgentState::Blocked;
        let (out, _) = render(&s, 26, 26);
        assert!(!out.contains(":5173"), "{out}");
        assert!(out.contains("blocked"), "{out}");
    }

    /// The connector is a colour first and a glyph second. Two projects' strands sharing a
    /// hue would say they were one project, which is exactly the thing the section split
    /// made it possible to get wrong.
    #[test]
    fn each_project_s_strand_is_drawn_in_its_own_accent() {
        let s = served();
        let buf = render_buffer(&s, 26, 26);
        let t = Theme::horde();
        // `builder` is api-refactor's and `hugo` is docs', in two different sections.
        let strand = |name: &str| {
            let y = row_of(&buf, 26, 26, name);
            // Column 1 is the first cell of the indent, which is where the rail is drawn.
            buf.cell((1, y)).unwrap().fg
        };
        let want = |space: usize| {
            let sp = &s.spaces[space];
            let k = if s.focused_space == Some(sp.id) { 0.35 } else { 0.62 };
            color(crate::theme::mix(t.space_accent(sp.accent), t.ui.panel_bg, k))
        };
        assert_eq!(strand("builder"), want(0));
        assert_eq!(strand("vite"), want(0), "a server on the same strand as its agents");
        assert_eq!(strand("writer"), want(1));
        assert_eq!(strand("hugo"), want(1));
        assert_ne!(want(0), want(1), "two projects must not share a strand");
    }

    /// The rail lives inside the indent, so a row's name starts in the same column whether it
    /// has a connector or not. A tree whose labels do not line up is worse than no tree.
    #[test]
    fn a_connector_never_shifts_the_name_beside_it() {
        let (out, _) = render(&served(), 26, 26);
        // By character, not by byte: every glyph on the connector is multi-byte.
        let col = |name: &str| {
            let l = out.lines().find(|l| l.contains(name)).unwrap();
            l.char_indices().position(|(i, _)| l[i..].starts_with(name)).unwrap()
        };
        assert_eq!(col("builder"), col("reviewer"));
        assert_eq!(col("builder"), col("vite"), "both sections indent alike");
    }

    /// The agent list is the thing you are actually watching, so it is the one that wins a
    /// squeeze — services yield their standing room rather than taking it.
    #[test]
    fn a_short_panel_drops_the_service_block_before_the_agent_list() {
        let (out, _) = render(&served(), 26, 14);
        println!("\n{out}\n");
        assert!(out.contains("AGENTS"), "{out}");
        assert!(out.contains("builder"), "{out}");
        assert!(!out.contains("SERVICES"), "{out}");
    }

    /// Anchored to the bottom of its block, so the section does not slide up and down the
    /// panel as agents come and go. A list you have to re-find is one you stop looking at.
    #[test]
    fn the_service_block_sits_at_the_same_height_whatever_the_agents_do() {
        let before = render(&served(), 26, 26).0;
        let mut s = served();
        s.tabs[0].panes.push(8);
        s.panes.push(pane(8, 1, 1, Some(("third", AgentState::Idle))));
        let after = render(&s, 26, 26).0;
        let at = |o: &str| o.lines().position(|l| l.contains("SERVICES")).unwrap();
        assert!(after.contains("third"), "{after}");
        assert_eq!(at(&before), at(&after), "\n{before}\n---\n{after}");
    }

    /// A server is still a pane you want to jump to, so its row has to answer a click.
    #[test]
    fn a_service_row_is_clickable() {
        let (out, hits) = render(&served(), 26, 26);
        let y = out.lines().position(|l| l.contains("vite")).unwrap() as u16;
        assert_eq!(hits.iter().find(|(hy, _)| *hy == y).map(|(_, h)| *h), Some(Hit::Pane(5)));
    }

    #[test]
    fn spaces_and_agents_are_separate_labelled_sections() {
        let (out, _) = render(&snap(), 24, 22);
        println!("\n{out}\n");
        assert!(out.contains("SPACES"), "{out}");
        assert!(out.contains("AGENTS"), "{out}");
        assert!(
            out.find("SPACES").unwrap() < out.find("AGENTS").unwrap(),
            "AGENTS must sit below SPACES"
        );
    }

    #[test]
    fn spaces_section_lists_only_spaces_never_panes() {
        let (out, _) = render(&snap(), 24, 22);
        let spaces_block = &out[out.find("SPACES").unwrap()..out.find("AGENTS").unwrap()];
        assert!(spaces_block.contains("api-refactor"));
        assert!(spaces_block.contains("docs"));
        // Agent and pane names belong to the other section.
        assert!(!spaces_block.contains("builder"), "{spaces_block}");
        assert!(!spaces_block.contains("pane3"), "{spaces_block}");
    }

    #[test]
    fn agents_section_lists_every_agent_with_its_state() {
        let (out, _) = render(&snap(), 24, 22);
        let agents_block = &out[out.find("AGENTS").unwrap()..];
        for name in ["builder", "reviewer", "writer"] {
            assert!(agents_block.contains(name), "{name} missing from:\n{agents_block}");
        }
        // Each row carries its state: elapsed while working, the label otherwise.
        assert!(agents_block.contains("2m18s"), "{agents_block}");
        assert!(agents_block.contains("blocked"), "{agents_block}");
        assert!(agents_block.contains("idle"), "{agents_block}");
    }

    /// The whole point of the change: which project an agent belongs to is readable without
    /// switching to that space and watching the dimming change.
    #[test]
    fn agents_are_grouped_under_a_header_for_their_space() {
        let (out, _) = render(&snap(), 26, 22);
        println!("\n{out}\n");
        let block = &out[out.find("AGENTS").unwrap()..];
        let header = block.find("api-refactor").expect("group header:\n{block}");
        let builder = block.find("builder").unwrap();
        let reviewer = block.find("reviewer").unwrap();
        let docs = block.find("docs").expect("second group header");
        let writer = block.find("writer").unwrap();
        assert!(header < builder && builder < reviewer, "{block}");
        assert!(reviewer < docs && docs < writer, "{block}");
    }

    #[test]
    fn a_group_header_rolls_up_the_states_of_its_agents() {
        // api-refactor holds one blocked and one working agent.
        let (out, _) = render(&snap(), 26, 22);
        let block = &out[out.find("AGENTS").unwrap()..];
        let line = block.lines().find(|l| l.contains("api-refactor")).unwrap();
        assert!(line.contains("◍1"), "blocked count on the header: {line:?}");
        assert!(line.contains("◐1"), "working count on the header: {line:?}");
    }

    #[test]
    fn agents_from_other_spaces_still_appear() {
        // `writer` lives in `docs` while `api-refactor` is focused.
        let (out, hits) = render(&snap(), 24, 22);
        assert!(out.contains("writer"));
        assert!(hits.iter().any(|(_, h)| *h == Hit::Pane(4)));
    }

    #[test]
    fn shell_panes_are_not_listed_as_agents() {
        let (out, hits) = render(&snap(), 24, 22);
        assert!(!out.contains("pane3"), "{out}");
        assert!(!hits.iter().any(|(_, h)| *h == Hit::Pane(3)));
    }

    /// A group header is not a click target yet, so it must not steal one either.
    #[test]
    fn hit_rows_are_unique_and_cover_both_sections() {
        let (_, hits) = render(&snap(), 24, 22);
        assert!(hits.iter().any(|(_, h)| *h == Hit::Space(1)));
        assert!(hits.iter().any(|(_, h)| *h == Hit::Pane(1)));
        let ys: Vec<u16> = hits.iter().map(|(y, _)| *y).collect();
        let mut u = ys.clone();
        u.sort_unstable();
        u.dedup();
        assert_eq!(ys.len(), u.len(), "overlapping rows: {hits:?}");
    }

    #[test]
    fn empty_agent_list_says_so_rather_than_showing_a_blank_panel() {
        let mut s = snap();
        s.panes.iter_mut().for_each(|p| p.agent = None);
        s.spaces.iter_mut().for_each(|sp| {
            sp.agent_count = 0;
            sp.attention_count = 0;
        });
        let (out, _) = render(&s, 24, 22);
        assert!(out.contains("AGENTS"), "{out}");
        assert!(out.contains("none yet"), "{out}");
    }

    /// Activity comes only from lifecycle hooks; screen detection cannot see a tool call.
    /// It is shown on a second line under the agent, and only while it is working — a
    /// finished turn's counts would be stale trivia.
    #[test]
    fn a_working_agent_shows_what_it_is_doing_underneath() {
        let mut s = snap();
        for p in s.panes.iter_mut() {
            if let Some(a) = p.agent.as_mut() {
                a.activity = crate::proto::Activity {
                    tools: 12,
                    files: 3,
                    errors: 1,
                    turns: 2,
                    last_tool: Some("Edit".into()),
                };
            }
        }
        // Two group headers cost two rows the flat list did not need.
        let (out, hits) = render(&s, 26, 26);
        println!("\n{out}\n");
        assert!(out.contains("12 tools"), "{out}");
        // Failures outrank the file count in a 20-column gutter: one is actionable.
        assert!(out.contains("1 failed"), "{out}");
        assert!(!out.contains("3 files"), "the file count should yield to the failure: {out}");

        // The detail line is not clickable; only the agent row itself is.
        let pane_hits = hits.iter().filter(|(_, h)| matches!(h, Hit::Pane(_))).count();
        assert_eq!(pane_hits, 3, "one hit row per agent, not per rendered line");

        // Only the working agent gets a line; the blocked and idle ones do not.
        let agents_block = &out[out.find("AGENTS").unwrap()..];
        assert_eq!(agents_block.matches("12 tools").count(), 1, "{agents_block}");
    }

    #[test]
    fn the_activity_summary_shows_files_when_nothing_failed() {
        use crate::proto::Activity;
        let a = Activity { tools: 9, files: 2, errors: 0, turns: 1, last_tool: None };
        assert_eq!(a.summary().as_deref(), Some("9 tools · 2 files"));
        let a = Activity { tools: 9, files: 2, errors: 3, turns: 1, last_tool: None };
        assert_eq!(a.summary().as_deref(), Some("9 tools · 3 failed"));
        let a = Activity { tools: 4, files: 0, errors: 0, turns: 1, last_tool: None };
        assert_eq!(a.summary().as_deref(), Some("4 tools"));
        // Nothing recorded means no line at all, rather than a row of zeroes.
        assert_eq!(Activity::default().summary(), None);
    }

    fn crowded() -> Snapshot {
        let mut s = snap();
        let panes: Vec<PaneInfo> =
            (0..40u32).map(|i| pane(100 + i, 1, 1, Some(("a", AgentState::Blocked)))).collect();
        s.tabs[0].panes = panes.iter().map(|p| p.id).collect();
        s.tabs[1].panes = vec![];
        s.panes = panes;
        s
    }

    #[test]
    fn footer_summary_survives_a_long_agent_list() {
        let (out, _) = render(&crowded(), 24, 22);
        assert!(out.contains("needs you"), "footer must survive:\n{out}");
    }

    /// The old list ended in "+3 more" and those agents were simply gone. Now the overflow is
    /// stated as a fraction on the heading — costing no row of its own — and the rest is still
    /// reachable.
    #[test]
    fn overflowing_agents_are_counted_on_the_heading_rather_than_dropped() {
        let (out, _) = render(&crowded(), 24, 22);
        println!("\n{out}\n");
        assert!(!out.contains("more"), "the overflow note is gone: {out}");
        let heading = out.lines().find(|l| l.contains("AGENTS")).unwrap();
        assert!(heading.contains("/40"), "how much of the list is visible: {heading:?}");
    }

    #[test]
    fn scrolling_reaches_agents_the_first_screen_could_not_show() {
        let mut s = crowded();
        // Give the last agent a name nothing else shares.
        let last = s.panes.last_mut().unwrap();
        last.agent.as_mut().unwrap().name = "zzlast".into();
        let (top, _) = render_at(&s, 24, 22, None, 0);
        assert!(!top.contains("zzlast"), "not on the first screen:\n{top}");
        let (down, _) = render_at(&s, 24, 22, None, 100);
        assert!(down.contains("zzlast"), "scrolling must reach it:\n{down}");
    }

    /// Scroll is clamped to the list, so a stale offset from a longer list cannot blank the
    /// section.
    #[test]
    fn an_over_scrolled_list_still_shows_its_tail() {
        let (out, hits) = render_at(&snap(), 24, 22, None, 900);
        assert!(out.contains("writer"), "{out}");
        assert!(!hits.is_empty());
    }

    #[test]
    fn nothing_ever_writes_past_the_panel_width() {
        let mut s = snap();
        s.spaces[0].name = "an-absurdly-long-space-name-that-cannot-fit".into();
        if let Some(a) = s.panes[0].agent.as_mut() {
            a.name = "an-absurdly-long-agent-name-that-cannot-fit".into();
        }
        // Both sides of every density threshold: inner is two less than the width, so the
        // rungs change at 20 and 32. A rung that goes untested ships broken at exactly one
        // sidebar width.
        for w in [10u16, 14, 16, 18, 20, 24, 28, 32, 40, 60] {
            for scroll in [0usize, 3] {
                let (out, _) = render_at(&s, w, 22, Some((3, 1)), scroll);
                for line in out.lines() {
                    assert_eq!(line.chars().count(), w as usize, "width {w}: {line:?}");
                }
            }
        }
    }

    #[test]
    fn tiny_areas_render_nothing_rather_than_panicking() {
        let (out, hits) = render(&snap(), 6, 20);
        assert_eq!(out.trim(), "");
        assert!(hits.is_empty());
        let (out, _) = render(&snap(), 24, 4);
        assert_eq!(out.trim(), "");
    }

    #[test]
    fn summary_counts_only_non_zero_states() {
        let lines = summary_lines(&snap());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].2, "needs you");
        assert_eq!(lines[1].2, "working");
    }

    #[test]
    fn outstanding_board_work_appears_in_the_footer() {
        let (out, _) = render_board(&snap(), 26, 24, Some((3, 1)));
        assert!(out.contains("3 tasks open"), "unclaimed work is the headline:\n{out}");
    }

    /// That horde may act on its own has to be visible, not remembered.
    #[test]
    fn armed_triggers_show_in_the_footer() {
        let mut s = snap();
        s.triggers_armed = 2;
        let (out, _) = render_board(&s, 26, 24, None);
        assert!(out.contains("2 triggers armed"), "{out}");
    }

    /// And with the master switch off the daemon reports zero, so the row costs nothing.
    #[test]
    fn a_disarmed_session_shows_no_trigger_row() {
        let (out, _) = render(&snap(), 26, 24);
        assert!(!out.contains("armed"), "{out}");
    }

    #[test]
    fn a_fully_claimed_board_says_claimed_rather_than_zero_open() {
        // "0 tasks open" would read as an empty board when three agents are mid-task.
        let (out, _) = render_board(&snap(), 26, 24, Some((0, 3)));
        assert!(out.contains("3 tasks claimed"), "{out}");
        assert!(!out.contains("0 tasks"), "{out}");
    }

    #[test]
    fn an_empty_board_costs_no_footer_line() {
        let (with, _) = render_board(&snap(), 26, 24, Some((0, 0)));
        let (without, _) = render_board(&snap(), 26, 24, None);
        assert_eq!(with, without);
        assert!(!with.contains("task"), "{with}");
    }

    #[test]
    fn a_collapsed_space_shows_only_its_header() {
        let mut s = snap();
        s.spaces[0].collapsed = true;
        let (out, hits) = render(&s, 26, 22);
        println!("\n{out}\n");
        let block = &out[out.find("AGENTS").unwrap()..];
        assert!(block.contains("api-refactor"), "the header stays: {block}");
        assert!(!block.contains("builder"), "{block}");
        assert!(!block.contains("reviewer"), "{block}");
        // Folded rows are not clickable, because they are not drawn.
        assert!(!hits.iter().any(|(_, h)| *h == Hit::Pane(1)));
        // A folded group still says what is inside it — that is the point of folding.
        let line = block.lines().find(|l| l.contains("api-refactor")).unwrap();
        assert!(line.contains("◍1"), "{line:?}");
        assert!(line.contains("▸"), "a folded group reads as folded: {line:?}");
    }

    #[test]
    fn a_pinned_agent_is_lifted_above_the_groups() {
        let mut s = snap();
        s.panes.iter_mut().find(|p| p.id == 4).unwrap().pinned = true;
        let (out, _) = render(&s, 26, 22);
        println!("\n{out}\n");
        let block = &out[out.find("AGENTS").unwrap()..];
        let writer = block.find("writer").unwrap();
        let first_group = block.find("api-refactor").unwrap();
        assert!(writer < first_group, "pinned rows come first:\n{block}");
        // And exactly once — pinning moves a row, it does not clone one.
        assert_eq!(block.matches("writer").count(), 1, "{block}");
    }

    /// Until the panel has been given the keyboard, the wheel owns the scroll — a cursor
    /// nobody asked for would drag the list back to the top on the very next frame.
    #[test]
    fn an_unfocused_sidebar_shows_no_cursor_and_leaves_the_scroll_alone() {
        let mut st = crate::client::roster::SidebarState { scroll: 4, ..Default::default() };
        let (_, _) = render_state(&crowded(), 24, 22, None, &mut st, false);
        assert_eq!(st.scroll, 4, "the wheel keeps its position");
        assert_eq!(st.cursor, None);
    }

    #[test]
    fn the_cursor_is_scrolled_into_view() {
        use crate::client::roster::{filtered_rows, Density, Focus, Lens, SidebarState};
        let s = crowded();
        // Name the last agent so it can be found on screen.
        let mut s = {
            let mut s = s;
            s.panes.last_mut().unwrap().agent.as_mut().unwrap().name = "zzlast".into();
            s
        };
        let rows = filtered_rows(&s, Density::of(22), &Lens::All, Section::Agents);
        let mut st = SidebarState::default();
        st.jump(&rows, true);
        assert!(matches!(st.cursor, Some(Focus::Agent(_))));

        let (out, _) = render_state(&mut s, 24, 22, None, &mut st, true);
        println!("\n{out}\n");
        assert!(out.contains("zzlast"), "the cursor must drag the window to it:\n{out}");
        assert!(st.scroll > 0, "and that means scrolling");
    }

    #[test]
    fn a_focused_sidebar_marks_the_row_the_cursor_is_on() {
        use crate::client::roster::{filtered_rows, Density, Lens, SidebarState};
        let s = snap();
        let rows = filtered_rows(&s, Density::of(24), &Lens::All, Section::Agents);
        let mut st = SidebarState::default();
        st.resolve(&rows);
        st.step(&rows, 1); // the first agent, `builder`
        let (out, _) = render_state(&s, 26, 22, None, &mut st, true);
        println!("\n{out}\n");
        let line = out.lines().find(|l| l.contains("builder")).unwrap();
        assert!(line.starts_with('▎'), "the cursor row carries the marker: {line:?}");
    }

    /// Clicking a header folds the group rather than switching to it — a disclosure triangle
    /// that teleports you is not a disclosure triangle. So it needs its own hit kind.
    #[test]
    fn a_group_header_is_a_distinct_click_target_from_its_space_row() {
        let (_, hits) = render(&snap(), 26, 22);
        assert!(hits.iter().any(|(_, h)| *h == Hit::Space(1)), "{hits:?}");
        assert!(hits.iter().any(|(_, h)| *h == Hit::Group(1)), "{hits:?}");
        // On different lines, and every row still unique.
        let ys: Vec<u16> = hits.iter().map(|(y, _)| *y).collect();
        let mut u = ys.clone();
        u.sort_unstable();
        u.dedup();
        assert_eq!(ys.len(), u.len(), "overlapping rows: {hits:?}");
    }

    /// A filtered list that does not say it is filtered reads as a broken one, so the lens
    /// name outranks the overflow counter for the columns available.
    #[test]
    fn an_active_lens_names_itself_on_the_heading() {
        use crate::client::roster::{Lens, SidebarState};
        let mut st = SidebarState { lens: Lens::NeedsYou, ..Default::default() };
        let (out, _) = render_state(&snap(), 40, 22, None, &mut st, false);
        println!("\n{out}\n");
        let heading = out.lines().find(|l| l.contains("AGENTS")).unwrap();
        assert!(heading.contains("needs you"), "{heading:?}");
        // And it actually filtered: only `reviewer` is blocked.
        let block = &out[out.find("AGENTS").unwrap()..];
        assert!(block.contains("reviewer"), "{block}");
        assert!(!block.contains("builder"), "{block}");
    }

    /// The footer stays session-wide on purpose: a lens that also silenced the counts would
    /// hide the very thing you filtered away.
    #[test]
    fn a_lens_does_not_change_the_footer_counts() {
        use crate::client::roster::{Lens, SidebarState};
        let mut plain = SidebarState::default();
        let mut lensed = SidebarState { lens: Lens::Working, ..Default::default() };
        let (a, _) = render_state(&snap(), 40, 22, None, &mut plain, false);
        let (b, _) = render_state(&snap(), 40, 22, None, &mut lensed, false);
        for out in [&a, &b] {
            assert!(out.contains("1 needs you"), "{out}");
            assert!(out.contains("1 working"), "{out}");
        }
    }

    #[test]
    fn a_lens_matching_nothing_says_so_rather_than_showing_a_blank_section() {
        use crate::client::roster::{Lens, SidebarState};
        let mut st = SidebarState { lens: Lens::Role("nobody".into()), ..Default::default() };
        let (out, _) = render_state(&snap(), 40, 22, None, &mut st, false);
        assert!(out.contains("no agents match"), "{out}");
    }
}
