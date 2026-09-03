//! Your own board, drawn two ways.
//!
//! The same shape as the roster and the dashboard — a pure content function that can be
//! asserted as text, then a renderer — with one addition the other views did not need.
//!
//! # Geometry is a function, not a recording
//!
//! Every other view in horde records where it drew things: the sidebar fills `sidebar_hits`
//! while rendering, the dashboard returns a list of boxes. That works for a list, where a
//! click is "which row", and it stops working for a board you can *drag* on, for two reasons.
//! A recorded hit list is one frame stale by construction, and — worse — it cannot be tested
//! without standing up a terminal, so the interaction that most needs a test is the one that
//! cannot have one.
//!
//! So [`layout`] is a pure function of the columns and the area. The renderer calls it, and
//! the mouse handler calls it again on the next event against the area the renderer recorded.
//! One computation, two callers, no copy to drift. It also means a drag can be tested by
//! synthesising a press and a release at rects this function worked out — which is exactly
//! what `a_card_dropped_below_the_last_one_lands_at_the_end` does.
//!
//! # Why a drop names a card rather than a row
//!
//! The board can be filtered. An index into the three cards you can see is not an index into
//! the eleven that are there, so a drop reports the card it landed *behind* and the daemon
//! resolves that against the real order. The alternative is the client keeping track of cards
//! it is deliberately not showing, which is the client owning state twice.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect as TRect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{color, fill, put_line, truncate, width};
use crate::proto::{Card, KanbanReply};
use crate::theme::Theme;

/// The narrowest a column may be before the board gives up and shows one at a time.
///
/// Twenty-six leaves twenty-two cells inside the border, which is about six words — enough
/// for a title to be a title rather than a stub. Below this a board is worse than a list, and
/// the board says so by becoming one column wide rather than by shrinking into noise.
pub const MIN_COL_W: u16 = 26;

/// A gap between columns, so two borders never touch and read as one box.
const GAP: u16 = 1;

/// Rows per card: a border, two lines of title, a line of facts, a border.
///
/// Fixed rather than fitted to the title. A variable height would mean the layout depending
/// on the text, and every hit test depending on it too — and the payoff would be a board
/// whose rows move whenever you rename something.
pub const CARD_H: u16 = 5;

/// Inside a card, the rows the title gets.
const TITLE_ROWS: usize = 2;

/// Which rendering of the same cards is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Board,
    List,
}

impl View {
    pub fn flip(self) -> View {
        match self {
            View::Board => View::List,
            View::List => View::Board,
        }
    }

    pub fn chip(self) -> &'static str {
        match self {
            View::Board => " KANBAN ",
            View::List => " LIST ",
        }
    }
}

/// A field of the card view: what the cursor is on, and what a key edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Body,
    Due,
    Tags,
    Project,
    /// The armed window — see `daemon::kanban::Card::assist`.
    Assist,
    Comments,
}

impl Field {
    pub fn all() -> [Field; 7] {
        [
            Field::Title,
            Field::Body,
            Field::Due,
            Field::Tags,
            Field::Project,
            Field::Assist,
            Field::Comments,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Field::Title => "TITLE",
            Field::Body => "DESCRIPTION",
            Field::Due => "DUE",
            Field::Tags => "TAGS",
            Field::Project => "PROJECT",
            Field::Assist => "AGENTS",
            Field::Comments => "COMMENTS",
        }
    }

    /// The key that jumps straight here, so the hint row and the handler cannot disagree.
    pub fn key(&self) -> char {
        match self {
            Field::Title => 'r',
            Field::Body => 'e',
            Field::Due => 'd',
            Field::Tags => 't',
            Field::Project => 'p',
            Field::Assist => 'a',
            Field::Comments => 'c',
        }
    }

    pub fn from_key(c: char) -> Option<Field> {
        Field::all().into_iter().find(|f| f.key() == c)
    }

    /// Whether typing into this field may contain newlines.
    pub fn multiline(&self) -> bool {
        matches!(self, Field::Body | Field::Comments)
    }

    pub fn step(self, by: isize) -> Field {
        let all = Field::all();
        let i = all.iter().position(|f| *f == self).unwrap_or(0) as isize;
        all[(i + by).rem_euclid(all.len() as isize) as usize]
    }
}

/// A field being typed into, and what has been typed so far.
#[derive(Debug, Clone, PartialEq)]
pub struct Editing {
    pub field: Field,
    pub text: TextArea,
}

/// Something the board is asking you for, one line at a time.
///
/// Its own rather than [`crate::client::menu::Prompt`], which returns you to the terminal on
/// both enter and escape. Answering "what should this column be called" by dumping you out of
/// the board is not a prompt, it is an exit — and threading a "go back to" through the shared
/// prompt would put the board's business in a type six other views also use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Filter,
    NewCard { column: String },
    NewColumn,
    RenameColumn { from: String },
}

impl Ask {
    pub fn title(&self) -> String {
        match self {
            Ask::Filter => "filter".into(),
            Ask::NewCard { column } => format!("new card in {column}"),
            Ask::NewColumn => "new column".into(),
            Ask::RenameColumn { from } => format!("rename {from} to"),
        }
    }
}

/// A question the board is asking, and the answer so far.
#[derive(Debug, Clone, PartialEq)]
pub struct Asking {
    pub ask: Ask,
    pub text: TextArea,
}

/// The question, over the hint row it replaces.
pub fn draw_ask(buf: &mut Buffer, body: TRect, theme: &Theme, asking: &Asking) {
    let y = body.y + body.height;
    put_line(
        buf,
        body.x,
        y,
        body.width,
        Line::from(vec![
            Span::styled(
                format!(" {} ", asking.ask.title()),
                Style::default()
                    .bg(color(theme.ui.working))
                    .fg(color(theme.ui.bg))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", asking.text.text()),
                Style::default().fg(color(theme.ui.text)),
            ),
            Span::styled("▌", Style::default().fg(color(theme.ui.accent))),
            Span::styled(
                "   enter saves · esc cancels",
                Style::default().fg(color(theme.ui.text_faint)),
            ),
        ]),
    );
}

/// What the pointer is doing to a card, while it is doing it.
///
/// `grab` is the offset from the card's corner to the pointer, which is the difference
/// between a card that follows your hand and one that snaps its corner under the cursor the
/// moment you touch it. `moved` is what tells a click from a drag on release, so a press and
/// release in the same place opens the card instead of moving it nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardDrag {
    pub id: u64,
    pub from_col: usize,
    pub grab: (u16, u16),
    pub at: (u16, u16),
    pub hover_col: Option<usize>,
    pub moved: bool,
}

/// One column of the board as it is being shown: its name, and the cards in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub cards: Vec<Card>,
    /// True for a column no longer in the configured list.
    ///
    /// Cards hold a column name, so editing the configured list can leave cards pointing at a
    /// column that is not there any more. Those cards are never dropped — the column is shown
    /// at the end, marked, so the work is visible and can be dragged somewhere that exists.
    pub extra: bool,
}

/// The board's cards, filtered and grouped, in the order they are shown.
///
/// Pure so it can be asserted as text: what the board *says* is worth a test, and standing up
/// a terminal to find out is not.
pub fn columns(
    reply: Option<&KanbanReply>,
    project: Option<&str>,
    archived: bool,
    query: &str,
) -> Vec<Column> {
    let Some(r) = reply else { return Vec::new() };
    let q = query.trim().to_lowercase();
    let shown: Vec<&Card> = r
        .cards
        .iter()
        .filter(|c| archived || !c.archived)
        .filter(|c| match project {
            None => true,
            Some(p) => c.project.as_deref() == Some(p),
        })
        .filter(|c| {
            q.is_empty()
                || c.title.to_lowercase().contains(&q)
                || c.body.to_lowercase().contains(&q)
                || c.tags.iter().any(|t| t.contains(&q))
        })
        .collect();

    // The configured columns first, in their order, then anything cards still name. Built
    // from both so an empty configured column still draws — a board that hides its own empty
    // columns has nowhere to drop the first card.
    let mut names: Vec<(String, bool)> = r.columns.iter().map(|c| (c.clone(), false)).collect();
    for c in &shown {
        if !names.iter().any(|(n, _)| n.eq_ignore_ascii_case(&c.column)) {
            names.push((c.column.clone(), true));
        }
    }

    names
        .into_iter()
        .map(|(name, extra)| {
            let mut cards: Vec<Card> = shown
                .iter()
                .filter(|c| c.column.eq_ignore_ascii_case(&name))
                .map(|c| (*c).clone())
                .collect();
            // Id breaks a tie, so two cards that somehow share a position still have one
            // order rather than whichever the sort left them in.
            cards.sort_by_key(|c| (c.pos, c.id));
            Column { name, cards, extra }
        })
        .collect()
}

/// Where one column and its cards were placed.
#[derive(Debug, Clone, PartialEq)]
pub struct ColLayout {
    /// The whole column including its header.
    pub rect: TRect,
    /// Where cards go — the column less its header row.
    pub body: TRect,
    /// Every card with a box on screen, top to bottom.
    pub cards: Vec<(u64, TRect)>,
    /// How many cards are above the first one shown.
    pub skipped: usize,
    /// How many cards there are in total, shown or not.
    pub total: usize,
}

/// Where the whole board was placed.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub area: TRect,
    /// One entry per *shown* column, in screen order.
    pub cols: Vec<ColLayout>,
    /// Index into the full column list of the first one shown.
    pub first: usize,
    /// How many columns there are in total.
    pub total: usize,
}

impl Layout {
    /// The column index — into the full list — of the nth shown column.
    pub fn column_at(&self, shown: usize) -> usize {
        self.first + shown
    }

    /// Which shown column, if any, is under this point.
    pub fn hit_column(&self, p: Position) -> Option<usize> {
        self.cols.iter().position(|c| c.rect.contains(p))
    }

    /// Which card is under this point, as (shown column, card id).
    pub fn hit_card(&self, p: Position) -> Option<(usize, u64)> {
        let col = self.hit_column(p)?;
        self.cols[col].cards.iter().find(|(_, r)| r.contains(p)).map(|(id, _)| (col, *id))
    }

    /// Whether this point is on a column's header rather than in its body.
    pub fn hit_header(&self, p: Position) -> Option<usize> {
        let col = self.hit_column(p)?;
        (p.y < self.cols[col].body.y).then_some(col)
    }

    /// The card a drop at this height should land behind, or `None` for the top.
    ///
    /// By the midpoint of each card rather than by which card was hit, because a drop is
    /// about a *gap* between two cards and every gap has to be reachable — including the one
    /// above the first card, which no card's own box covers.
    pub fn drop_after(&self, col: usize, y: u16) -> Option<u64> {
        let c = self.cols.get(col)?;
        let mut prev = None;
        for (id, rect) in &c.cards {
            if y < rect.y + rect.height / 2 {
                return prev;
            }
            prev = Some(*id);
        }
        // Below every card in the column: the end of what is showing.
        prev
    }
}

/// Work out where every column and card goes.
///
/// `focus` is the index of the column the cursor is in, which is what decides the horizontal
/// scroll when not all of them fit. `scroll` is indexed by a column's place in `cols` — its
/// real index, not its position on screen. Screen position would mean a column's scroll
/// following whichever slot it happened to be drawn in, so paging sideways would hand each
/// column the last one's offset.
pub fn layout(cols: &[Column], area: TRect, scroll: &[u16], focus: usize) -> Layout {
    let mut out =
        Layout { area, cols: Vec::new(), first: 0, total: cols.len() };
    if area.width < 8 || area.height < 4 || cols.is_empty() {
        return out;
    }

    // How many fit at the minimum width. At least one, always: a board too narrow for a
    // column shows one column rather than nothing, because one column of your work is still
    // your work and an apology is not.
    let fits = ((area.width + GAP) / (MIN_COL_W + GAP)).max(1) as usize;
    let shown = fits.min(cols.len());
    // Scroll the window so the focused column is in it, and no further.
    let focus = focus.min(cols.len().saturating_sub(1));
    let first = if focus < out.first { focus } else { focus.saturating_sub(shown - 1) };
    let first = first.min(cols.len() - shown);
    out.first = first;

    // Share the width out, remainder to the leftmost columns so the total is exact rather
    // than a column short.
    let total_gap = GAP * (shown as u16 - 1);
    let each = (area.width - total_gap) / shown as u16;
    let mut spare = (area.width - total_gap) % shown as u16;

    let mut x = area.x;
    for i in 0..shown {
        let mut w = each;
        if spare > 0 {
            w += 1;
            spare -= 1;
        }
        let rect = TRect { x, y: area.y, width: w, height: area.height };
        // Two header rows: the name and count, then a rule under it.
        let body = TRect {
            x,
            y: area.y + 2,
            width: w,
            height: area.height.saturating_sub(2),
        };
        let column = &cols[first + i];
        let capacity = (body.height / CARD_H) as usize;
        let skipped = (*scroll.get(first + i).unwrap_or(&0) as usize).min(
            column.cards.len().saturating_sub(capacity.max(1)),
        );
        let cards = column
            .cards
            .iter()
            .skip(skipped)
            .take(capacity)
            .enumerate()
            .map(|(n, c)| {
                (
                    c.id,
                    TRect {
                        x: body.x,
                        y: body.y + n as u16 * CARD_H,
                        width: body.width,
                        height: CARD_H,
                    },
                )
            })
            .collect();
        out.cols.push(ColLayout { rect, body, cards, skipped, total: column.cards.len() });
        x += w + GAP;
    }
    out
}

/// The part of the frame the board gets, once the chrome has had its rows.
///
/// The dashboard can ignore this because its content is centred and the status bar simply
/// draws over the bottom row. A board fills its area to the last cell, so it has to know:
/// one row at the top for the project and the filter, one at the bottom for the keys, and
/// whatever the status bar is standing on.
pub fn body_area(area: TRect, status: TRect) -> TRect {
    let mut body = area;
    body.y = area.y.saturating_add(1);
    body.height = area.height.saturating_sub(1);
    if status.height > 0 && status.y >= body.y && status.y < body.y + body.height {
        body.height = status.y - body.y;
    }
    // And the hint row, which lives just under the board.
    body.height = body.height.saturating_sub(1);
    body
}

/// Where a card floats when it was opened from the list.
///
/// Generous rather than snug: the description and the thread are the reason you opened it, and
/// a box sized to the fields above them turns the part you came for into a scroll. Inset by at
/// least three rows and four columns on every side so the list is visibly *behind* it — a panel
/// flush to the frame reads as a new screen, which is the thing this is not.
pub fn popup_area(body: TRect) -> TRect {
    let w = body.width.saturating_sub(8).clamp(1, 80);
    let h = body.height.saturating_sub(6).clamp(1, 24);
    super::centered(body, w, h)
}

/// The id of the card the cursor is on, if there is one.
///
/// `sel` indexes the cards of the *shown* column, so a filter that empties the column under
/// the cursor answers `None` rather than pointing at whatever slid into that slot.
pub fn selected(cols: &[Column], col: usize, sel: usize) -> Option<u64> {
    cols.get(col)?.cards.get(sel).map(|c| c.id)
}

/// The keys, under the board.
///
/// Printed rather than left to the help overlay because a board is a view you use with both
/// hands, and the two keys that move a card between columns are not guessable.
pub fn draw_hints(buf: &mut Buffer, body: TRect, theme: &Theme, view: View) {
    let y = body.y + body.height;
    let keys: &[(&str, &str)] = match view {
        View::Board => &[
            ("hjkl", "move"),
            ("HL", "shove"),
            ("JK", "reorder"),
            ("n", "new"),
            ("enter", "open"),
            ("/", "filter"),
            ("p", "project"),
            ("v", "list"),
            ("esc", "back"),
        ],
        View::List => &[
            ("jk", "move"),
            ("n", "new"),
            ("enter", "open"),
            ("/", "filter"),
            ("p", "project"),
            ("x", "archived"),
            ("v", "board"),
            ("esc", "back"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(
            key.to_string(),
            Style::default().fg(color(theme.ui.accent)).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {what}   "),
            Style::default().fg(color(theme.ui.text_faint)),
        ));
    }
    put_line(buf, body.x, y, body.width, Line::from(spans));
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// How a due date should read, and how alarming it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Overdue,
    Today,
    Soon,
    Later,
    None,
}

/// Whole days between now and a due date, rounded.
///
/// Rounded rather than truncated because a due date is stored at local *noon* — so "due
/// yesterday" is about twenty-one hours ago at nine this morning, and truncation would call
/// that zero days and print "today" for a date that has passed.
pub fn days_until(due: u64, now: u64) -> i64 {
    let delta = due as i64 - now as i64;
    (delta as f64 / 86_400_000.0).round() as i64
}

/// `overdue 2d`, `today`, `in 3d` — the shortest form that is still true.
pub fn due_label(due: Option<u64>, now: u64) -> (String, Tone) {
    let Some(due) = due else { return (String::new(), Tone::None) };
    match days_until(due, now) {
        d if d < -1 => (format!("overdue {}d", -d), Tone::Overdue),
        -1 => ("yesterday".into(), Tone::Overdue),
        0 => ("today".into(), Tone::Today),
        1 => ("tomorrow".into(), Tone::Soon),
        d if d <= 7 => (format!("in {d}d"), Tone::Soon),
        _ => (crate::daemon::triggers::local_date(due), Tone::Later),
    }
}

fn tone_color(tone: Tone, t: &Theme) -> crate::proto::Rgb {
    match tone {
        Tone::Overdue => t.ui.error,
        Tone::Today => t.ui.warn,
        Tone::Soon => t.ui.text,
        Tone::Later => t.ui.text_dim,
        Tone::None => t.ui.text_faint,
    }
}

// ---------------------------------------------------------------------------
// Drawing the board
// ---------------------------------------------------------------------------

/// What a card's state adds to its meta line: armed, handed over, or archived.
fn flags(card: &Card) -> Vec<(&'static str, Flag)> {
    let mut out = Vec::new();
    if card.archived {
        out.push(("archived", Flag::Faint));
    }
    match (card.assist, card.handed) {
        (_, Some(_)) => out.push(("⚑ agents", Flag::Done)),
        (Some(_), None) => out.push(("⚑ armed", Flag::Armed)),
        _ => {}
    }
    if !card.comments.is_empty() {
        out.push(("💬", Flag::Dim));
    }
    out
}

enum Flag {
    Armed,
    Done,
    Dim,
    Faint,
}

impl Flag {
    fn color(&self, t: &Theme) -> crate::proto::Rgb {
        match self {
            Flag::Armed => t.ui.working,
            Flag::Done => t.ui.done,
            Flag::Dim => t.ui.text_dim,
            Flag::Faint => t.ui.text_faint,
        }
    }
}

/// The board, in columns.
///
/// Takes the layout rather than computing one, so that what was drawn and what the mouse will
/// hit are the same object rather than two runs of the same arithmetic.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    buf: &mut Buffer,
    theme: &Theme,
    cols: &[Column],
    lay: &Layout,
    focus: usize,
    sel: Option<u64>,
    drag: Option<&CardDrag>,
    now: u64,
) {
    for (n, cl) in lay.cols.iter().enumerate() {
        let idx = lay.column_at(n);
        let Some(column) = cols.get(idx) else { continue };
        let focused = idx == focus;
        let hovered = drag.is_some_and(|d| d.hover_col == Some(n));

        // Header: the name, then how many are in it.
        let head_fg = match (focused, hovered) {
            (_, true) => theme.ui.accent_alt,
            (true, _) => theme.ui.accent,
            _ => theme.ui.text_dim,
        };
        let mut head = vec![Span::styled(
            truncate(&column.name.to_uppercase(), cl.rect.width.saturating_sub(6) as usize),
            Style::default().fg(color(head_fg)).add_modifier(Modifier::BOLD),
        )];
        head.push(Span::styled(
            format!(" {}", column.cards.len()),
            Style::default().fg(color(theme.ui.text_faint)),
        ));
        if column.extra {
            // A column nobody configured. Said out loud, because the alternative is work
            // sitting in a column you cannot explain the existence of.
            head.push(Span::styled(" ·", Style::default().fg(color(theme.ui.warn))));
        }
        put_line(buf, cl.rect.x, cl.rect.y, cl.rect.width, Line::from(head));
        let rule: String = "─".repeat(cl.rect.width as usize);
        put_line(
            buf,
            cl.rect.x,
            cl.rect.y + 1,
            cl.rect.width,
            Line::from(Span::styled(
                rule,
                Style::default().fg(color(if hovered { theme.ui.accent_alt } else { theme.ui.border })),
            )),
        );

        for (id, rect) in &cl.cards {
            let Some(card) = column.cards.iter().find(|c| c.id == *id) else { continue };
            // The card being dragged leaves a hole rather than a copy: seeing it in two
            // places at once is what makes a drag look like a duplicate.
            if drag.is_some_and(|d| d.id == *id) {
                continue;
            }
            draw_card_box(buf, *rect, theme, card, Some(*id) == sel, now);
        }

        // Say what is off the top and bottom, rather than letting a column look finished.
        if cl.skipped > 0 {
            put_line(
                buf,
                cl.body.x,
                cl.body.y,
                cl.body.width,
                Line::from(Span::styled(
                    format!("  ↑ {} more", cl.skipped),
                    Style::default().fg(color(theme.ui.text_faint)),
                )),
            );
        }
        let below = cl.total.saturating_sub(cl.skipped + cl.cards.len());
        if below > 0 && cl.body.height > 0 {
            put_line(
                buf,
                cl.body.x,
                cl.body.y + cl.body.height - 1,
                cl.body.width,
                Line::from(Span::styled(
                    format!("  ↓ {below} more"),
                    Style::default().fg(color(theme.ui.text_faint)),
                )),
            );
        }
    }

    // Columns that did not fit. Without this a narrow board looks like a board with two
    // columns rather than a board showing two of four, and work sits somewhere you have
    // stopped believing in.
    let hidden_left = lay.first;
    let hidden_right = lay.total.saturating_sub(lay.first + lay.cols.len());
    let mark = Style::default().fg(color(theme.ui.accent));
    if hidden_left > 0 {
        put_line(
            buf,
            lay.area.x,
            lay.area.y + 1,
            4,
            Line::from(Span::styled(format!("◂{hidden_left} "), mark)),
        );
    }
    if hidden_right > 0 {
        let label = format!(" {hidden_right}▸");
        let w = width(&label) as u16;
        put_line(
            buf,
            lay.area.x + lay.area.width.saturating_sub(w),
            lay.area.y + 1,
            w,
            Line::from(Span::styled(label, mark)),
        );
    }

    // The dragged card last, so it floats over everything, offset by where it was grabbed.
    if let Some(d) = drag {
        if let Some(card) = cols.iter().flat_map(|c| c.cards.iter()).find(|c| c.id == d.id) {
            let w = lay.cols.first().map(|c| c.rect.width).unwrap_or(MIN_COL_W);
            let ghost = TRect {
                x: d.at.0.saturating_sub(d.grab.0),
                y: d.at.1.saturating_sub(d.grab.1),
                width: w,
                height: CARD_H,
            }
            .clamp(lay.area);
            fill(buf, ghost, theme.ui.panel_bg);
            draw_card_box(buf, ghost, theme, card, true, now);
        }
    }
}

/// One card, boxed. The id rides in the top border so both title rows stay title.
fn draw_card_box(
    buf: &mut Buffer,
    rect: TRect,
    theme: &Theme,
    card: &Card,
    selected: bool,
    now: u64,
) {
    if rect.width < 6 || rect.height < 3 {
        return;
    }
    let border = if selected { theme.ui.border_focus } else { theme.ui.border };
    let bstyle = Style::default().fg(color(border));
    let inner = rect.width.saturating_sub(2) as usize;

    let tag = format!("#{}", card.id);
    // `╭` + ` ` + the id + ` ` + rule + `╮` is the whole width. Getting this one cell wrong
    // costs the top-right corner, which `put_line` clips off the end without complaining —
    // see `every_card_box_is_closed_on_all_four_sides`.
    let rest = (rect.width as usize).saturating_sub(4 + width(&tag));
    put_line(
        buf,
        rect.x,
        rect.y,
        rect.width,
        Line::from(vec![
            Span::styled("╭ ".to_string(), bstyle),
            Span::styled(
                tag,
                Style::default().fg(color(if selected { theme.ui.accent } else { theme.ui.text_faint })),
            ),
            Span::styled(format!(" {}", "─".repeat(rest)), bstyle),
            Span::styled("╮".to_string(), bstyle),
        ]),
    );

    let title_fg = if card.archived { theme.ui.text_faint } else { theme.ui.text };
    let wrapped = super::wrap_text(&card.title, inner.saturating_sub(2));
    for row in 0..TITLE_ROWS {
        let y = rect.y + 1 + row as u16;
        if y >= rect.y + rect.height - 1 {
            break;
        }
        let text = wrapped.get(row).cloned().unwrap_or_default();
        // A title too long for both rows says so, rather than stopping mid-word as though
        // that were all of it.
        let text = if row == TITLE_ROWS - 1 && wrapped.len() > TITLE_ROWS {
            format!("{}…", truncate(&text, inner.saturating_sub(3)))
        } else {
            text
        };
        put_line(
            buf,
            rect.x,
            y,
            rect.width,
            Line::from(vec![
                Span::styled("│ ".to_string(), bstyle),
                Span::styled(
                    truncate(&text, inner.saturating_sub(2)),
                    Style::default().fg(color(title_fg)).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
            ]),
        );
        put_line(
            buf,
            rect.x + rect.width - 1,
            y,
            1,
            Line::from(Span::styled("│".to_string(), bstyle)),
        );
    }

    // The facts line: when it is due, what it is tagged, and whether the agents have it.
    let meta_y = rect.y + rect.height - 2;
    let mut spans = vec![Span::styled("│ ".to_string(), bstyle)];
    let (label, tone) = due_label(card.due, now);
    if !label.is_empty() {
        spans.push(Span::styled(
            format!("{label} "),
            Style::default().fg(color(tone_color(tone, theme))),
        ));
    }
    for t in card.tags.iter().take(2) {
        spans.push(Span::styled(
            format!("#{t} "),
            Style::default().fg(color(theme.ui.accent_alt)),
        ));
    }
    for (text, flag) in flags(card) {
        spans.push(Span::styled(
            format!("{text} "),
            Style::default().fg(color(flag.color(theme))),
        ));
    }
    put_line(buf, rect.x, meta_y, rect.width, Line::from(spans));
    put_line(
        buf,
        rect.x + rect.width - 1,
        meta_y,
        1,
        Line::from(Span::styled("│".to_string(), bstyle)),
    );

    put_line(
        buf,
        rect.x,
        rect.y + rect.height - 1,
        rect.width,
        Line::from(Span::styled(
            format!("╰{}╯", "─".repeat(inner)),
            bstyle,
        )),
    );
}

/// The header row over the board: which project, what is filtered, what is hidden.
pub fn draw_header(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    project: Option<&str>,
    query: &str,
    archived: bool,
    due: usize,
) {
    let mut spans = vec![Span::styled(
        " KANBAN ",
        Style::default()
            .bg(color(theme.ui.accent))
            .fg(color(theme.ui.bg))
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        format!("  {}", project.unwrap_or("all projects")),
        Style::default().fg(color(theme.ui.text)),
    ));
    if due > 0 {
        spans.push(Span::styled(
            format!("  {due} due"),
            Style::default().fg(color(theme.ui.warn)),
        ));
    }
    if !query.is_empty() {
        spans.push(Span::styled(
            format!("  /{query}"),
            Style::default().fg(color(theme.ui.accent_alt)),
        ));
    }
    if archived {
        spans.push(Span::styled(
            "  +archived",
            Style::default().fg(color(theme.ui.text_faint)),
        ));
    }
    put_line(buf, area.x, area.y, area.width, Line::from(spans));
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

/// Every card as one flat, sorted table — the same cards, read rather than arranged.
///
/// The board answers "what is where"; this answers "what is next", which is a different
/// question and one that columns are actively bad at: work due tomorrow in four columns is
/// four places you have to look.
pub fn list_rows(cols: &[Column], now: u64) -> Vec<(Card, String)> {
    let mut all: Vec<(Card, String)> =
        cols.iter().flat_map(|c| c.cards.iter().map(|k| (k.clone(), c.name.clone()))).collect();
    // Dated work first and soonest first, then everything undated by id. A list sorted by
    // column would be the board again, with less of it on screen.
    all.sort_by_key(|(c, _)| (c.due.unwrap_or(u64::MAX), c.id));
    let _ = now;
    all
}

/// Where the list was placed, and which row is where.
///
/// The same bargain [`Layout`] strikes, for the same reason: the renderer calls this and the
/// mouse handler calls it again against the area the renderer recorded, so a click resolves
/// against the arithmetic that actually drew the row rather than a second copy of it. The
/// discarded hit list this replaces was the copy — `draw_list` returned one and nothing ever
/// read it, so clicking the list did nothing at all.
#[derive(Debug, Clone, PartialEq)]
pub struct ListLayout {
    pub area: TRect,
    /// The header row, which is not a card and must not select one.
    pub header_y: u16,
    /// Where the first shown row sits.
    pub first_y: u16,
    /// How many rows fit under the header.
    pub shown: usize,
    /// How many rows are above the first one shown.
    pub scroll: usize,
    /// How many rows there are in total.
    pub total: usize,
}

impl ListLayout {
    /// Which row index — into the full, sorted list — is under this point.
    ///
    /// `None` for the header, for empty space past the last row, and for anywhere outside the
    /// area, so a click below a short list cannot select its last card by accident.
    pub fn row_at(&self, p: Position) -> Option<usize> {
        if !self.area.contains(p) || p.y < self.first_y {
            return None;
        }
        let n = (p.y - self.first_y) as usize;
        let row = self.scroll + n;
        (n < self.shown && row < self.total).then_some(row)
    }
}

/// Place the list: how far it is scrolled, and how many rows that leaves visible.
///
/// Scroll follows the cursor rather than being state of its own. A stored scroll offset and a
/// stored selection are two facts that can disagree, and the way they disagree is a selected
/// row that is off screen — so this derives the one from the other and there is nothing to
/// keep in step.
pub fn list_layout(area: TRect, total: usize, sel: usize) -> ListLayout {
    let shown = (area.height as usize).saturating_sub(1);
    // Pin the cursor to the last visible row once it would fall past it. `saturating_sub`
    // leaves this at zero for as long as everything fits, which is the common case.
    let scroll = (sel + 1).saturating_sub(shown);
    ListLayout {
        area,
        header_y: area.y,
        first_y: area.y + 1,
        shown: shown.min(total.saturating_sub(scroll)),
        scroll,
        total,
    }
}

/// What the list has room for. Dropped narrowest-first, so a thin pane keeps the columns that
/// answer "what is next" and loses the ones that merely add context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListCols {
    column: u16,
    due: u16,
    project: u16,
}

impl ListCols {
    /// `ID` and `DUE` and the title are never dropped: an undated row with no title is not a
    /// list of work, it is a list of numbers.
    fn fit(width: u16) -> Self {
        let due = 11;
        // Each of these is only worth showing at a width where it is not a stub. Below that
        // the cells hold two letters and an ellipsis, which reads as damage rather than data.
        let column = if width >= 62 { 12 } else { 0 };
        let project = if width >= 78 { 12 } else { 0 };
        ListCols { column, due, project }
    }
}

/// The description, the tags and the state, in the order a row is read.
///
/// The board shows this on a card's facts line; the list has to show the same things or it is
/// a worse view of the same cards rather than a different one. `≡` is the one addition — the
/// board has a whole box to make a description visible in and a row has not.
fn list_marks(card: &Card) -> Vec<(String, Flag)> {
    let mut out = Vec::new();
    if !card.body.trim().is_empty() {
        out.push(("≡".to_string(), Flag::Faint));
    }
    for (text, flag) in flags(card) {
        out.push((text.to_string(), flag));
    }
    out
}

/// Every card as one flat, sorted table — the same cards, read rather than arranged.
///
/// Shows everything a card can be given, which is the point: the board is arranged by the one
/// field you drag and hides the rest behind opening a card, and a due date you have to open
/// four cards to compare is a due date you do not really have.
pub fn draw_list(
    buf: &mut Buffer,
    lay: &ListLayout,
    theme: &Theme,
    rows: &[(Card, String)],
    sel: Option<u64>,
    now: u64,
) {
    let area = lay.area;
    if area.width < 20 || area.height < 2 {
        return;
    }
    let w = ListCols::fit(area.width);

    let mut head = format!("  {:<5}", "ID");
    if w.column > 0 {
        head.push_str(&format!("{:<w$}", "COLUMN", w = w.column as usize));
    }
    head.push_str(&format!("{:<w$}", "DUE", w = w.due as usize));
    if w.project > 0 {
        head.push_str(&format!("{:<w$}", "PROJECT", w = w.project as usize));
    }
    head.push_str("TITLE");
    put_line(
        buf,
        area.x,
        lay.header_y,
        area.width,
        Line::from(Span::styled(
            head,
            Style::default().fg(color(theme.ui.text_faint)).add_modifier(Modifier::BOLD),
        )),
    );

    for (n, (card, column)) in rows.iter().skip(lay.scroll).take(lay.shown).enumerate() {
        let y = lay.first_y + n as u16;
        let picked = Some(card.id) == sel;
        if picked {
            fill(buf, TRect { x: area.x, y, width: area.width, height: 1 }, theme.ui.selection);
        }
        let (label, tone) = due_label(card.due, now);
        let title_fg = if card.archived { theme.ui.text_faint } else { theme.ui.text };
        let mut spans = vec![
            Span::styled(
                if picked { "▸ " } else { "  " },
                Style::default().fg(color(theme.ui.accent)),
            ),
            Span::styled(
                format!("{:<5}", format!("#{}", card.id)),
                Style::default().fg(color(theme.ui.text_faint)),
            ),
        ];
        if w.column > 0 {
            spans.push(Span::styled(
                format!("{:<w$}", truncate(column, w.column as usize - 1), w = w.column as usize),
                Style::default().fg(color(theme.ui.text_dim)),
            ));
        }
        spans.push(Span::styled(
            format!("{:<w$}", truncate(&label, w.due as usize - 1), w = w.due as usize),
            Style::default().fg(color(tone_color(tone, theme))),
        ));
        if w.project > 0 {
            // An em dash rather than a blank: a personal board has plenty of work that is not
            // about a repository, and a gap there reads as a field nobody filled in.
            let (text, fg) = match card.project.as_deref() {
                Some(p) => (p.to_string(), theme.ui.text_dim),
                None => ("—".to_string(), theme.ui.text_faint),
            };
            spans.push(Span::styled(
                format!("{:<w$}", truncate(&text, w.project as usize - 1), w = w.project as usize),
                Style::default().fg(color(fg)),
            ));
        }
        spans.push(Span::styled(card.title.clone(), Style::default().fg(color(title_fg))));
        for t in card.tags.iter().take(3) {
            spans.push(Span::styled(
                format!("  #{t}"),
                Style::default().fg(color(theme.ui.accent_alt)),
            ));
        }
        for (text, flag) in list_marks(card) {
            spans.push(Span::styled(
                format!("  {text}"),
                Style::default().fg(color(flag.color(theme))),
            ));
        }
        put_line(buf, area.x, y, area.width, Line::from(spans));
    }
}

// ---------------------------------------------------------------------------
// One card, full screen
// ---------------------------------------------------------------------------

/// The card view: everything about one card, and the thread under it.
#[allow(clippy::too_many_arguments)]
/// Returns how many lines it had to draw, so the key handler knows what it may scroll to.
/// Recorded rather than recomputed, for the reason `graph_plot` is: two copies of the same
/// arithmetic agree only until one of them changes.
pub fn draw_detail(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    card: &Card,
    focus: Field,
    editing: Option<&Editing>,
    scroll: usize,
    now: u64,
) -> usize {
    fill(buf, area, theme.ui.bg);
    if area.width < 30 || area.height < 8 {
        put_line(
            buf,
            area.x,
            area.y,
            area.width,
            Line::from(Span::styled(
                "the window is too small for a card",
                Style::default().fg(color(theme.ui.text_dim)),
            )),
        );
        return 0;
    }
    // A reading column rather than the whole width. A description set across 200 cells is a
    // description nobody reads.
    let w = area.width.min(96);
    let x = area.x + (area.width - w) / 2;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let head = |text: String, on: bool| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                text,
                Style::default()
                    .fg(color(if on { theme.ui.accent } else { theme.ui.text_faint }))
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    };
    let field = |label: &'static str, value: String, on: bool, fg: crate::proto::Rgb| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                if on { "▸ " } else { "  " },
                Style::default().fg(color(theme.ui.accent)),
            ),
            Span::styled(
                format!("{label:<9}"),
                Style::default().fg(color(theme.ui.text_faint)),
            ),
            Span::styled(value, Style::default().fg(color(fg))),
        ])
    };

    // Title.
    lines.push(Line::from(vec![
        Span::styled(
            format!("#{}  ", card.id),
            Style::default().fg(color(theme.ui.text_faint)),
        ),
        Span::styled(
            card.title.clone(),
            Style::default().fg(color(theme.ui.text)).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("{}  ·  added {}", card.column, ago(now, card.created)),
        Style::default().fg(color(theme.ui.text_dim)),
    )));
    lines.push(Line::from(""));

    let (due, tone) = due_label(card.due, now);
    lines.push(field(
        "due",
        if due.is_empty() { "—".into() } else { format!("{} · {}", crate::daemon::triggers::local_date(card.due.unwrap_or(0)), due) },
        focus == Field::Due,
        tone_color(tone, theme),
    ));
    lines.push(field(
        "tags",
        if card.tags.is_empty() {
            "—".into()
        } else {
            card.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ")
        },
        focus == Field::Tags,
        theme.ui.accent_alt,
    ));
    lines.push(field(
        "project",
        card.project.clone().unwrap_or_else(|| "—".into()),
        focus == Field::Project,
        theme.ui.text,
    ));
    // The one line where the two boards meet, and it says exactly what will happen.
    let (assist, assist_fg) = match (card.handed, card.assist, card.assist_blocker()) {
        (Some(task), _, _) => (format!("handed over as task #{task}"), theme.ui.done),
        (None, Some(w), None) => (
            format!("hand over when due within {}", short_duration(w)),
            theme.ui.working,
        ),
        (None, Some(w), Some(why)) => (
            format!("armed for {} — {why}", short_duration(w)),
            theme.ui.warn,
        ),
        (None, None, _) => ("not armed — this one is yours".into(), theme.ui.text_dim),
    };
    lines.push(field("agents", assist, focus == Field::Assist, assist_fg));
    lines.push(Line::from(""));

    lines.push(head("DESCRIPTION".into(), focus == Field::Body));
    let body = match editing {
        Some(e) if e.field == Field::Body => e.text.text(),
        _ => card.body.clone(),
    };
    match body.trim().is_empty() {
        true => lines.push(Line::from(Span::styled(
            "  nothing written yet — e to write some",
            Style::default().fg(color(theme.ui.text_faint)),
        ))),
        false => {
            // `wrap_text` already yields one empty line for an empty paragraph, so a blank
            // line between paragraphs comes out as itself rather than as two.
            for raw in body.lines() {
                for wrapped in super::wrap_text(raw, w.saturating_sub(4) as usize) {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(wrapped, Style::default().fg(color(theme.ui.text))),
                    ]));
                }
            }
        }
    }
    lines.push(Line::from(""));

    lines.push(head(format!("COMMENTS  {}", card.comments.len()), focus == Field::Comments));
    if card.comments.is_empty() {
        lines.push(Line::from(Span::styled(
            "  nothing said yet — c to say something",
            Style::default().fg(color(theme.ui.text_faint)),
        )));
    }
    for c in &card.comments {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                c.by.clone(),
                Style::default().fg(color(theme.ui.accent)).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   {}", ago(now, c.at)),
                Style::default().fg(color(theme.ui.text_faint)),
            ),
        ]));
        for raw in c.body.lines() {
            for wrapped in super::wrap_text(raw, w.saturating_sub(6) as usize) {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(wrapped, Style::default().fg(color(theme.ui.text_dim))),
                ]));
            }
        }
        lines.push(Line::from(""));
    }

    // What is being typed goes last and in full, because it is the only part of this screen
    // that is not a record of something — it is the thing your hands are doing.
    if let Some(e) = editing {
        lines.push(Line::from(Span::styled(
            format!("── {} ──", e.field.label()),
            Style::default().fg(color(theme.ui.working)),
        )));
        for (n, l) in e.text.lines.iter().enumerate() {
            let cursor = n == e.text.line;
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(l.clone(), Style::default().fg(color(theme.ui.text))),
                Span::styled(
                    if cursor { "▌" } else { "" },
                    Style::default().fg(color(theme.ui.accent)),
                ),
            ]));
        }
    }

    for (n, line) in lines.iter().skip(scroll).take(area.height as usize).enumerate() {
        put_line(buf, x, area.y + n as u16, w, line.clone());
    }
    lines.len()
}

/// How long ago, in the roughest useful units.
///
/// Its own rather than the dashboard's private one: promoting that to a shared helper would
/// mean editing the block of `ui` helpers another change is already sitting in, and this is
/// four lines.
fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

// ---------------------------------------------------------------------------
// Reading a date somebody typed
// ---------------------------------------------------------------------------

/// Local year, month, day and weekday (Sunday zero) of an instant.
fn local_ymd(ms: u64) -> (i32, u32, u32, u32) {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes only the `tm` handed to it.
    unsafe { libc::localtime_r(&t, &mut tm) };
    (tm.tm_year + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32, tm.tm_wday as u32)
}

/// Local noon of a calendar date, as unix millis.
///
/// Noon, because that is what a due date *is*: a day, not an instant. Anchored in the middle
/// of it, no timezone change and no daylight-saving boundary can drag the date onto the day
/// before or after — which is the whole reason dates stored as midnight go wrong twice a year.
fn local_noon(y: i32, m: u32, d: u32) -> Option<u64> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = y - 1900;
    tm.tm_mon = m as i32 - 1;
    tm.tm_mday = d as i32;
    tm.tm_hour = 12;
    // Let the C library decide whether this date was in daylight saving. Guessing is how a
    // date lands an hour out, twice a year, in one direction.
    tm.tm_isdst = -1;
    // SAFETY: `mktime` reads and normalises the `tm` handed to it and returns a time_t.
    let t = unsafe { libc::mktime(&mut tm) };
    (t > 0).then(|| t as u64 * 1000)
}

/// Noon of the day `days` from now.
fn noon_in(days: i64, now: u64) -> Option<u64> {
    let then = (now as i64 + days * 86_400_000).max(0) as u64;
    let (y, m, d, _) = local_ymd(then);
    local_noon(y, m, d)
}

const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// Read a due date the way somebody would write one.
///
/// `Ok(None)` is "no due date", which an empty box means — the same contract every other text
/// prompt in horde has. Anything it cannot read is an error rather than a silent `None`,
/// because a typo that quietly cleared the date you were setting is the worst of both.
pub fn parse_due(spec: &str, now: u64) -> Result<Option<u64>, String> {
    let s = spec.trim().to_lowercase();
    if s.is_empty() || s == "none" || s == "-" {
        return Ok(None);
    }
    let some = |o: Option<u64>| o.map(Some).ok_or_else(|| format!("{spec:?} is not a date"));
    match s.as_str() {
        "today" | "now" => return some(noon_in(0, now)),
        "tomorrow" => return some(noon_in(1, now)),
        "yesterday" => return some(noon_in(-1, now)),
        _ => {}
    }
    // A weekday means the next one of those, and never today: "friday" said on a Friday is
    // about the Friday coming, or you would have said today.
    if let Some(want) = DAY_NAMES.iter().position(|d| s == *d || (s.len() > 3 && s.starts_with(d))) {
        for ahead in 1..=7i64 {
            let then = (now as i64 + ahead * 86_400_000) as u64;
            if local_ymd(then).3 as usize == want {
                return some(noon_in(ahead, now));
            }
        }
    }
    // `+3d`, `3d`, `2w` — a distance rather than a date.
    let rel = s.strip_prefix('+').unwrap_or(&s);
    if let Some(n) = rel.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
        return some(noon_in(n, now));
    }
    if let Some(n) = rel.strip_suffix('w').and_then(|n| n.parse::<i64>().ok()) {
        return some(noon_in(n * 7, now));
    }
    // `2026-08-20`, or `08-20` for this year. Slashes too, since half the world types those.
    let parts: Vec<&str> = s.split(['-', '/']).collect();
    let num = |p: &str| p.trim().parse::<i64>().ok();
    let (y, m, d) = match parts.len() {
        3 => (num(parts[0]), num(parts[1]), num(parts[2])),
        2 => (Some(local_ymd(now).0 as i64), num(parts[0]), num(parts[1])),
        _ => (None, None, None),
    };
    match (y, m, d) {
        (Some(y), Some(m), Some(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            some(local_noon(y as i32, m as u32, d as u32))
        }
        _ => Err(format!(
            "cannot read {spec:?} as a date — try 2026-08-20, tomorrow, friday, or +3d"
        )),
    }
}

/// `2d`, `12h`, `30m` — a window, at the coarsest unit that is still exact.
pub fn short_duration(millis: u64) -> String {
    let secs = millis / 1000;
    match secs {
        s if s % 86_400 == 0 && s >= 86_400 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 && s >= 3_600 => format!("{}h", s / 3_600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

// ---------------------------------------------------------------------------
// Typing
// ---------------------------------------------------------------------------

/// A small multi-line text field.
///
/// Its own rather than the note editor's [`crate::client::editor::Buffer`], which is a modal
/// vim-shaped thing tied to a file on disk. A description is neither: it is a text box, and
/// typing in it should type. Columns are char indices throughout — a byte index into a line
/// with an emoji in it is a panic waiting for the right comment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextArea {
    pub lines: Vec<String>,
    pub line: usize,
    pub col: usize,
}

impl TextArea {
    pub fn new(text: &str) -> TextArea {
        let lines: Vec<String> = match text.is_empty() {
            true => vec![String::new()],
            false => text.lines().map(|l| l.to_string()).collect(),
        };
        let line = lines.len() - 1;
        let col = lines[line].chars().count();
        TextArea { lines, line, col }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn byte(&self, line: usize, col: usize) -> usize {
        self.lines[line]
            .char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(self.lines[line].len())
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte(self.line, self.col);
        self.lines[self.line].insert(at, c);
        self.col += 1;
    }

    pub fn newline(&mut self) {
        let at = self.byte(self.line, self.col);
        let rest = self.lines[self.line].split_off(at);
        self.lines.insert(self.line + 1, rest);
        self.line += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let at = self.byte(self.line, self.col - 1);
            self.lines[self.line].remove(at);
            self.col -= 1;
        } else if self.line > 0 {
            let gone = self.lines.remove(self.line);
            self.line -= 1;
            self.col = self.lines[self.line].chars().count();
            self.lines[self.line].push_str(&gone);
        }
    }

    pub fn left(&mut self) {
        match self.col {
            0 if self.line > 0 => {
                self.line -= 1;
                self.col = self.lines[self.line].chars().count();
            }
            0 => {}
            _ => self.col -= 1,
        }
    }

    pub fn right(&mut self) {
        let len = self.lines[self.line].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.line + 1 < self.lines.len() {
            self.line += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.line > 0 {
            self.line -= 1;
            self.col = self.col.min(self.lines[self.line].chars().count());
        }
    }

    pub fn down(&mut self) {
        if self.line + 1 < self.lines.len() {
            self.line += 1;
            self.col = self.col.min(self.lines[self.line].chars().count());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.line].chars().count();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Card, Comment, KanbanReply};

    fn card(id: u64, column: &str, pos: u32, title: &str) -> Card {
        Card {
            id,
            title: title.into(),
            column: column.into(),
            pos,
            body: String::new(),
            due: None,
            tags: Vec::new(),
            project: None,
            created: 0,
            updated: 0,
            archived: false,
            comments: Vec::new(),
            assist: None,
            handed: None,
        }
    }

    fn reply(cards: Vec<Card>) -> KanbanReply {
        KanbanReply {
            cards,
            columns: ["Backlog", "Todo", "Doing", "Done"].iter().map(|s| s.to_string()).collect(),
            project: None,
        }
    }

    /// An empty configured column still draws — otherwise the first card has nowhere to be
    /// dropped, and a board with three columns of work looks like a board with three columns.
    #[test]
    fn every_configured_column_shows_even_when_empty() {
        let cols = columns(Some(&reply(vec![card(1, "Todo", 0, "a")])), None, false, "");
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), [
            "Backlog", "Todo", "Doing", "Done"
        ]);
        assert_eq!(cols[1].cards.len(), 1);
        assert!(cols.iter().all(|c| !c.extra));
    }

    /// The rule that keeps an edit to the column list from being a way of losing work.
    #[test]
    fn a_card_in_an_unconfigured_column_gets_one_at_the_end() {
        let cols = columns(
            Some(&reply(vec![card(1, "Todo", 0, "a"), card(2, "Someday", 0, "b")])),
            None,
            false,
            "",
        );
        let last = cols.last().unwrap();
        assert_eq!(last.name, "Someday");
        assert!(last.extra, "and it is marked, rather than looking configured");
        assert_eq!(last.cards.len(), 1);
    }

    #[test]
    fn cards_sort_by_position_within_their_column() {
        let mut a = card(1, "Todo", 5, "later");
        let b = card(2, "Todo", 1, "sooner");
        a.pos = 5;
        let cols = columns(Some(&reply(vec![a, b])), None, false, "");
        let todo = cols.iter().find(|c| c.name == "Todo").unwrap();
        assert_eq!(todo.cards.iter().map(|c| c.id).collect::<Vec<_>>(), [2, 1]);
    }

    #[test]
    fn archived_cards_are_hidden_unless_asked_for() {
        let mut gone = card(2, "Todo", 1, "done with");
        gone.archived = true;
        let cards = vec![card(1, "Todo", 0, "live"), gone];
        let hidden = columns(Some(&reply(cards.clone())), None, false, "");
        assert_eq!(hidden.iter().map(|c| c.cards.len()).sum::<usize>(), 1);
        let shown = columns(Some(&reply(cards)), None, true, "");
        assert_eq!(shown.iter().map(|c| c.cards.len()).sum::<usize>(), 2);
    }

    #[test]
    fn a_filter_reads_the_title_the_description_and_the_tags() {
        let mut tagged = card(2, "Todo", 1, "unrelated");
        tagged.tags = vec!["api".into()];
        let mut described = card(3, "Todo", 2, "also unrelated");
        described.body = "mentions the importer".into();
        let cards = vec![card(1, "Todo", 0, "the importer"), tagged, described];
        let hit = |q: &str| -> Vec<u64> {
            columns(Some(&reply(cards.clone())), None, false, q)
                .iter()
                .flat_map(|c| c.cards.iter().map(|k| k.id))
                .collect()
        };
        assert_eq!(hit("importer"), [1, 3]);
        assert_eq!(hit("api"), [2]);
        assert_eq!(hit("IMPORTER"), [1, 3], "case does not decide it");
    }

    #[test]
    fn a_project_filter_leaves_other_projects_out() {
        let mut mine = card(1, "Todo", 0, "horde work");
        mine.project = Some("horde".into());
        let mut theirs = card(2, "Todo", 1, "other work");
        theirs.project = Some("elsewhere".into());
        let cards = vec![mine, theirs, card(3, "Todo", 2, "no project")];
        let scoped = columns(Some(&reply(cards.clone())), Some("horde"), false, "");
        assert_eq!(
            scoped.iter().flat_map(|c| c.cards.iter().map(|k| k.id)).collect::<Vec<_>>(),
            [1]
        );
        let all = columns(Some(&reply(cards)), None, false, "");
        assert_eq!(all.iter().map(|c| c.cards.len()).sum::<usize>(), 3);
    }

    // -- layout and hit testing ---------------------------------------------

    fn four_columns() -> Vec<Column> {
        columns(
            Some(&reply(vec![
                card(1, "Todo", 0, "one"),
                card(2, "Todo", 1, "two"),
                card(3, "Todo", 2, "three"),
                card(4, "Doing", 0, "four"),
            ])),
            None,
            false,
            "",
        )
    }

    fn area(w: u16, h: u16) -> TRect {
        TRect { x: 0, y: 0, width: w, height: h }
    }

    /// The claim the whole drag rests on: the mouse handler and the renderer resolve a cell
    /// the same way, because there is only one computation.
    #[test]
    fn every_card_the_layout_places_can_be_hit_at_its_own_rect() {
        for w in [30u16, 60, 90, 140, 200] {
            for h in [10u16, 24, 40] {
                let cols = four_columns();
                let lay = layout(&cols, area(w, h), &[0, 0, 0, 0], 0);
                for (n, cl) in lay.cols.iter().enumerate() {
                    for (id, rect) in &cl.cards {
                        let mid = Position {
                            x: rect.x + rect.width / 2,
                            y: rect.y + rect.height / 2,
                        };
                        assert_eq!(
                            lay.hit_card(mid),
                            Some((n, *id)),
                            "card #{id} at {w}x{h} is drawn where it cannot be clicked"
                        );
                    }
                }
            }
        }
    }

    /// The most common bug in this kind of code: an inclusive hit test where two neighbours
    /// both claim the cell on their shared boundary.
    #[test]
    fn columns_do_not_both_claim_the_cell_between_them() {
        let cols = four_columns();
        let lay = layout(&cols, area(120, 24), &[0, 0, 0, 0], 0);
        assert!(lay.cols.len() >= 2);
        for x in 0..120u16 {
            let hits: Vec<usize> = (0..lay.cols.len())
                .filter(|i| lay.cols[*i].rect.contains(Position { x, y: 5 }))
                .collect();
            assert!(hits.len() <= 1, "column boundary at x={x} is claimed by {hits:?}");
        }
    }

    /// A board too narrow for a column shows one column rather than nothing.
    #[test]
    fn a_narrow_board_shows_one_column_at_a_time() {
        let cols = four_columns();
        let lay = layout(&cols, area(30, 24), &[0, 0, 0, 0], 0);
        assert_eq!(lay.cols.len(), 1);
        assert_eq!(lay.cols[0].rect.width, 30, "and it uses the whole width");
        // And walking right scrolls the window rather than running off the end.
        let lay = layout(&cols, area(30, 24), &[0, 0, 0, 0], 3);
        assert_eq!(lay.first, 3);
        assert_eq!(lay.column_at(0), 3);
    }

    /// Every gap has to be reachable, including the one above the first card, which no card's
    /// own box covers.
    #[test]
    fn a_drop_names_the_card_it_landed_behind() {
        let cols = four_columns();
        let lay = layout(&cols, area(120, 30), &[0, 0, 0, 0], 0);
        let todo = 1;
        let cards = &lay.cols[todo].cards;
        assert_eq!(cards.len(), 3, "all three fit at this height");

        // Above the top card: the top of the column.
        assert_eq!(lay.drop_after(todo, cards[0].1.y), None);
        // Past the middle of the first: behind it.
        assert_eq!(lay.drop_after(todo, cards[0].1.y + CARD_H - 1), Some(1));
        // Just into the second: still behind the first, because the gap is above it.
        assert_eq!(lay.drop_after(todo, cards[1].1.y), Some(1));
        // Below every card: behind the last one.
        assert_eq!(lay.drop_after(todo, lay.cols[todo].body.y + lay.cols[todo].body.height - 1), Some(3));
    }

    /// Dropping onto empty space in a column still means something: the end of it.
    #[test]
    fn a_drop_into_an_empty_column_lands_at_the_top() {
        let cols = four_columns();
        let lay = layout(&cols, area(120, 30), &[0, 0, 0, 0], 0);
        let backlog = 0;
        assert!(lay.cols[backlog].cards.is_empty());
        assert_eq!(lay.drop_after(backlog, 10), None);
    }

    #[test]
    fn a_column_header_is_told_from_its_body() {
        let cols = four_columns();
        let lay = layout(&cols, area(120, 24), &[0, 0, 0, 0], 0);
        let c = &lay.cols[0];
        assert_eq!(lay.hit_header(Position { x: c.rect.x + 1, y: c.rect.y }), Some(0));
        assert_eq!(lay.hit_header(Position { x: c.rect.x + 1, y: c.body.y }), None);
    }

    /// A column with more cards than fit says so at both ends, rather than looking finished.
    #[test]
    fn a_scrolled_column_reports_what_is_out_of_sight() {
        let many: Vec<Card> =
            (1..=10).map(|i| card(i, "Todo", i as u32, &format!("card {i}"))).collect();
        let cols = columns(Some(&reply(many)), None, false, "");
        let lay = layout(&cols, area(120, 14), &[0, 2, 0, 0], 1);
        let todo = lay.cols.iter().position(|_| true).map(|_| 1).unwrap();
        assert_eq!(lay.cols[todo].skipped, 2);
        assert_eq!(lay.cols[todo].total, 10);
        assert!(lay.cols[todo].cards.len() < 10);
    }

    /// Scrolling past the end would leave a column looking empty when it is full.
    #[test]
    fn a_column_cannot_be_scrolled_past_its_last_card() {
        let many: Vec<Card> =
            (1..=4).map(|i| card(i, "Todo", i as u32, &format!("card {i}"))).collect();
        let cols = columns(Some(&reply(many)), None, false, "");
        let lay = layout(&cols, area(120, 30), &[0, 99, 0, 0], 1);
        assert!(!lay.cols[1].cards.is_empty(), "a wild scroll cannot empty the column");
    }

    // -- dates ---------------------------------------------------------------

    /// Due dates are stored at local noon, so "yesterday" is twenty-one hours ago at nine in
    /// the morning — and truncating that to zero days would print "today" for a date that has
    /// already passed.
    #[test]
    fn a_due_date_is_read_in_whole_days_from_noon() {
        let day = 86_400_000i64;
        let noon = 1_700_000_000_000i64;
        let nine_am = noon - 3 * 3_600_000;
        assert_eq!(days_until(noon as u64, nine_am as u64), 0);
        assert_eq!(days_until((noon - day) as u64, nine_am as u64), -1, "yesterday, not today");
        assert_eq!(days_until((noon + day) as u64, nine_am as u64), 1);
        assert_eq!(days_until((noon + 5 * day) as u64, nine_am as u64), 5);
    }

    #[test]
    fn a_due_date_reads_as_the_shortest_thing_that_is_still_true() {
        let day = 86_400_000u64;
        let now = 1_700_000_000_000u64;
        assert_eq!(due_label(None, now), (String::new(), Tone::None));
        assert_eq!(due_label(Some(now), now).0, "today");
        assert_eq!(due_label(Some(now + day), now).0, "tomorrow");
        assert_eq!(due_label(Some(now + 3 * day), now).0, "in 3d");
        assert_eq!(due_label(Some(now - day), now), ("yesterday".into(), Tone::Overdue));
        assert_eq!(due_label(Some(now - 4 * day), now), ("overdue 4d".into(), Tone::Overdue));
        // Past a week it is a date, because "in 23d" is not a thing anyone pictures.
        assert!(due_label(Some(now + 30 * day), now).0.contains('-'));
    }

    #[test]
    fn a_window_reads_at_the_coarsest_unit_that_is_exact() {
        assert_eq!(short_duration(2 * 86_400_000), "2d");
        assert_eq!(short_duration(12 * 3_600_000), "12h");
        assert_eq!(short_duration(90 * 60_000), "90m");
    }

    // -- the list ------------------------------------------------------------

    /// The list answers a question the board is bad at: what is next, across every column.
    #[test]
    fn the_list_puts_dated_work_first_and_soonest_first() {
        let day = 86_400_000u64;
        let now = 1_700_000_000_000u64;
        let mut soon = card(1, "Backlog", 0, "soon");
        soon.due = Some(now + day);
        let mut later = card(2, "Doing", 0, "later");
        later.due = Some(now + 9 * day);
        let undated = card(3, "Todo", 0, "someday");
        let cols = columns(Some(&reply(vec![later, undated, soon])), None, false, "");
        let rows = list_rows(&cols, now);
        assert_eq!(rows.iter().map(|(c, _)| c.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(rows[0].1, "Backlog", "and each row remembers which column it came from");
    }

    // -- typing --------------------------------------------------------------

    #[test]
    fn typing_types() {
        let mut t = TextArea::new("");
        for c in "hello".chars() {
            t.insert(c);
        }
        t.newline();
        for c in "world".chars() {
            t.insert(c);
        }
        assert_eq!(t.text(), "hello\nworld");
        assert_eq!((t.line, t.col), (1, 5));
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_one_above() {
        let mut t = TextArea::new("ab\ncd");
        t.line = 1;
        t.col = 0;
        t.backspace();
        assert_eq!(t.text(), "abcd");
        assert_eq!((t.line, t.col), (0, 2));
    }

    /// A byte index into a line with an emoji in it is a panic waiting for the right comment.
    #[test]
    fn a_multibyte_line_can_be_edited_without_panicking() {
        let mut t = TextArea::new("héllo 🌍");
        t.end();
        t.backspace();
        assert_eq!(t.text(), "héllo ");
        t.home();
        t.insert('x');
        assert_eq!(t.text(), "xhéllo ");
        t.right();
        t.right();
        t.backspace();
        assert_eq!(t.text(), "xhllo ");
    }

    #[test]
    fn moving_between_lines_keeps_the_cursor_inside_the_shorter_one() {
        let mut t = TextArea::new("a very long line\nshort");
        t.line = 0;
        t.col = 14;
        t.down();
        assert_eq!((t.line, t.col), (1, 5), "clamped to the end of the shorter line");
    }

    #[test]
    fn an_existing_value_opens_with_the_cursor_at_its_end() {
        let t = TextArea::new("already written");
        assert_eq!((t.line, t.col), (0, 15));
    }

    // -- fields --------------------------------------------------------------

    /// The hint row and the key handler read the same table, so a key printed on screen
    /// cannot be one the handler does not know.
    #[test]
    fn every_field_is_reachable_by_the_key_it_prints() {
        for f in Field::all() {
            assert_eq!(Field::from_key(f.key()), Some(f), "{} is unreachable", f.label());
        }
        assert_eq!(Field::from_key('q'), None);
    }

    #[test]
    fn stepping_through_the_fields_wraps_both_ways() {
        assert_eq!(Field::Title.step(-1), Field::Comments);
        assert_eq!(Field::Comments.step(1), Field::Title);
    }

    /// The list as text, so what it shows can be asserted rather than eyeballed.
    fn render_list(cols: &[Column], w: u16, h: u16, sel: usize) -> String {
        let area = area(w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        let rows = list_rows(cols, 1_700_000_000_000);
        let lay = list_layout(area, rows.len(), sel);
        let picked = rows.get(sel).map(|(c, _)| c.id);
        draw_list(&mut buf, &lay, &theme, &rows, picked, 1_700_000_000_000);
        (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Everything a card can be given has to be on its row, or the list is a worse view of the
    /// same cards rather than a different one — and the due date you came to compare is the
    /// one thing you would still have to open four cards to see.
    #[test]
    fn the_list_shows_every_detail_a_card_was_given() {
        let mut c = card(7, "Todo", 0, "wire up the importer");
        c.due = Some(1_700_000_000_000 + 2 * 86_400_000);
        c.tags = vec!["api".into(), "p1".into()];
        c.project = Some("horde".into());
        c.body = "read it in chunks".into();
        c.assist = Some(2 * 86_400_000);
        c.comments = vec![Comment { by: "josh".into(), body: "parked".into(), at: 0 }];
        let cols = vec![Column { name: "Todo".into(), cards: vec![c], extra: false }];

        let out = render_list(&cols, 120, 6, 0);
        assert!(out.contains("#7"), "the id: {out}");
        assert!(out.contains("Todo"), "the column: {out}");
        assert!(out.contains("in 2d"), "the due date: {out}");
        assert!(out.contains("horde"), "the project: {out}");
        assert!(out.contains("wire up the importer"), "the title: {out}");
        assert!(out.contains("#api") && out.contains("#p1"), "the tags: {out}");
        assert!(out.contains('≡'), "that it has a description: {out}");
        assert!(out.contains("armed"), "that it is armed: {out}");
        assert!(out.contains('💬'), "that it has a thread: {out}");
        assert!(out.contains("ID") && out.contains("DUE") && out.contains("PROJECT"), "{out}");
    }

    /// A card with no project is not a card whose project nobody filled in, and a blank cell
    /// reads as the second thing.
    #[test]
    fn a_card_with_no_project_says_so() {
        let cols = vec![Column {
            name: "Todo".into(),
            cards: vec![card(1, "Todo", 0, "no repository involved")],
            extra: false,
        }];
        assert!(render_list(&cols, 120, 4, 0).contains('—'));
    }

    /// Narrow, the list drops the columns that add context and keeps the ones that answer the
    /// question. Two letters and an ellipsis is damage, not data.
    #[test]
    fn a_narrow_list_drops_context_columns_rather_than_stubbing_them() {
        let mut c = card(1, "Todo", 0, "chase the vendor");
        c.project = Some("horde".into());
        let cols = vec![Column { name: "Todo".into(), cards: vec![c], extra: false }];

        let wide = render_list(&cols, 120, 4, 0);
        assert!(wide.contains("COLUMN") && wide.contains("PROJECT"));

        let mid = render_list(&cols, 70, 4, 0);
        assert!(mid.contains("COLUMN"), "the column still fits: {mid}");
        assert!(!mid.contains("PROJECT"), "the project does not: {mid}");

        let thin = render_list(&cols, 44, 4, 0);
        assert!(!thin.contains("COLUMN") && !thin.contains("PROJECT"), "{thin}");
        assert!(thin.contains("chase the vendor"), "but the work is still legible: {thin}");
        assert!(thin.contains("ID") && thin.contains("DUE"), "{thin}");
    }

    // -- the list, under the pointer ------------------------------------------

    /// The whole point of the layout being a function: the row the pointer resolves to is the
    /// row that was drawn. `draw_list` used to return a hit list nothing read, which is how
    /// clicking the list came to do nothing at all.
    #[test]
    fn the_row_under_the_pointer_is_the_row_that_was_drawn() {
        let cols = four_columns();
        let rows = list_rows(&cols, 1_700_000_000_000);
        let lay = list_layout(area(120, 12), rows.len(), 0);
        assert!(rows.len() >= 4, "enough to be worth a test");

        for (n, _) in rows.iter().enumerate().take(lay.shown) {
            let y = lay.first_y + n as u16;
            assert_eq!(lay.row_at(Position { x: 4, y }), Some(n), "row {n} at y={y}");
        }
    }

    /// The header is not a card, and neither is the space under a short list. Selecting the
    /// last row because the click landed past it is the kind of wrong that looks deliberate.
    #[test]
    fn a_click_off_the_rows_selects_nothing() {
        let cols = vec![Column {
            name: "Todo".into(),
            cards: vec![card(1, "Todo", 0, "one"), card(2, "Todo", 1, "two")],
            extra: false,
        }];
        let rows = list_rows(&cols, 1_700_000_000_000);
        let lay = list_layout(area(120, 20), rows.len(), 0);

        assert_eq!(lay.row_at(Position { x: 4, y: lay.header_y }), None, "the header");
        assert_eq!(lay.row_at(Position { x: 4, y: lay.first_y + 2 }), None, "past the last row");
        assert_eq!(lay.row_at(Position { x: 4, y: 19 }), None, "the bottom of the pane");
        assert_eq!(lay.row_at(Position { x: 4, y: lay.first_y }), Some(0), "but a row still hits");
    }

    /// Scroll is derived from the cursor rather than kept beside it, because two facts that
    /// can disagree disagree by putting the selected row off screen.
    #[test]
    fn the_list_scrolls_to_keep_the_cursor_on_screen() {
        // Four rows of room, twenty rows of work.
        let lay = list_layout(area(120, 5), 20, 0);
        assert_eq!((lay.scroll, lay.shown), (0, 4), "nothing to scroll yet");

        let lay = list_layout(area(120, 5), 20, 3);
        assert_eq!(lay.scroll, 0, "the last row that still fits");

        let lay = list_layout(area(120, 5), 20, 4);
        assert_eq!(lay.scroll, 1, "and now it pins to the bottom");
        assert_eq!(lay.row_at(Position { x: 4, y: lay.first_y + 3 }), Some(4), "the cursor is on it");

        let lay = list_layout(area(120, 5), 20, 19);
        assert_eq!(lay.scroll, 16);
        assert_eq!(lay.row_at(Position { x: 4, y: lay.first_y + 3 }), Some(19));
    }

    // -- what it actually draws ----------------------------------------------

    /// The board as text, the way the dashboard's tests read theirs.
    fn render(cols: &[Column], w: u16, h: u16, sel: Option<u64>) -> String {
        let area = area(w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        let scroll = vec![0u16; cols.len().max(1)];
        let lay = layout(cols, area, &scroll, 0);
        draw(&mut buf, &theme, cols, &lay, 0, sel, None, 1_700_000_000_000);
        (0..h)
            .map(|y| {
                (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_board_draws_its_columns_its_counts_and_its_cards() {
        let cols = four_columns();
        let out = render(&cols, 120, 24, Some(1));
        assert!(out.contains("TODO"), "{out}");
        assert!(out.contains("DOING"), "{out}");
        assert!(out.contains("one"), "the cards are on it: {out}");
        assert!(out.contains("#1"), "and carry their id: {out}");
        assert!(out.contains('╭') && out.contains('╰'), "boxed: {out}");
    }

    /// A card's facts line is the whole reason the board beats a list of titles.
    #[test]
    fn a_card_shows_its_due_date_its_tags_and_who_has_it() {
        let now = 1_700_000_000_000u64;
        let mut c = card(1, "Todo", 0, "wire up the importer");
        c.due = Some(now + 86_400_000);
        c.tags = vec!["api".into()];
        c.assist = Some(2 * 86_400_000);
        let cols = columns(Some(&reply(vec![c])), None, false, "");
        let out = render(&cols, 120, 24, None);
        assert!(out.contains("tomorrow"), "{out}");
        assert!(out.contains("#api"), "{out}");
        assert!(out.contains("armed"), "{out}");
    }

    /// A card whose top border ran one cell long lost its corner, silently, because
    /// `put_line` clips the overflow rather than complaining. Found by printing the board and
    /// looking at it, which is what `the_board_prints` is for.
    #[test]
    fn every_card_box_is_closed_on_all_four_sides() {
        let cols = four_columns();
        for w in [30u16, 60, 120, 200] {
            let a = area(w, 30);
            let mut buf = Buffer::empty(a);
            let theme = Theme::horde();
            let scroll = vec![0u16; cols.len()];
            let lay = layout(&cols, a, &scroll, 0);
            draw(&mut buf, &theme, &cols, &lay, 0, None, None, 1_700_000_000_000);
            for cl in &lay.cols {
                for (id, r) in &cl.cards {
                    let at = |x: u16, y: u16| buf.cell((x, y)).unwrap().symbol().to_string();
                    let right = r.x + r.width - 1;
                    let bottom = r.y + r.height - 1;
                    assert_eq!(at(r.x, r.y), "╭", "card #{id} at {w} wide");
                    assert_eq!(at(right, r.y), "╮", "card #{id} at {w} wide");
                    assert_eq!(at(r.x, bottom), "╰", "card #{id} at {w} wide");
                    assert_eq!(at(right, bottom), "╯", "card #{id} at {w} wide");
                }
            }
        }
    }

    /// A title too long for the two rows it gets says so, rather than stopping mid-word as
    /// though that were all of it.
    #[test]
    fn an_over_long_title_is_marked_as_cut_off() {
        let long = "a title that goes on and on well past anything that could fit in a card";
        let cols = columns(Some(&reply(vec![card(1, "Todo", 0, long)])), None, false, "");
        let out = render(&cols, 120, 24, None);
        assert!(out.contains('…'), "{out}");
    }

    /// The screens people actually have. None of these may panic or draw outside the buffer.
    #[test]
    fn the_board_survives_every_size_worth_having() {
        let many: Vec<Card> =
            (1..=30).map(|i| card(i, "Todo", i as u32, &format!("card number {i}"))).collect();
        let cols = columns(Some(&reply(many)), None, false, "");
        for (w, h) in [(20u16, 6u16), (40, 10), (80, 24), (120, 40), (200, 60), (9, 3)] {
            let out = render(&cols, w, h, Some(1));
            assert_eq!(out.lines().count(), h as usize, "{w}x{h}");
        }
    }

    /// A narrow board showing two of four columns must not look like a board with two
    /// columns, or work sits somewhere you have stopped believing in.
    #[test]
    fn a_board_too_narrow_for_its_columns_says_how_many_are_off_screen() {
        let cols = four_columns();
        let a = area(72, 20);
        let mut buf = Buffer::empty(a);
        let theme = Theme::horde();
        let scroll = vec![0u16; cols.len()];
        // Focused on the third column, so there is one hidden either side.
        let lay = layout(&cols, a, &scroll, 2);
        draw(&mut buf, &theme, &cols, &lay, 2, None, None, 1_700_000_000_000);
        let out: String = (0..a.height)
            .map(|y| (0..a.width).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("◂1"), "{out}");
        assert!(out.contains("1▸"), "{out}");
    }

    #[test]
    fn an_empty_board_draws_its_columns_and_says_nothing_else() {
        let cols = columns(Some(&reply(Vec::new())), None, false, "");
        let out = render(&cols, 120, 24, None);
        assert!(out.contains("BACKLOG"), "the columns are there to drop the first card into");
        assert!(!out.contains('╭'), "and no card boxes: {out}");
    }

    /// Print the board in colour, the way the start screen's own test does.
    ///
    /// `cargo test the_board_prints -- --nocapture` is how the thing gets *looked* at — the
    /// column widths, the card boxes, the colour of a date that has passed — without
    /// rebuilding horde, starting a daemon and typing work into it.
    #[test]
    fn the_board_prints() {
        let now = 1_700_000_000_000u64;
        let day = 86_400_000u64;
        let mut cards = vec![
            card(1, "Backlog", 0, "read up on the CSV spec"),
            card(2, "Backlog", 1, "someday: rewrite the parser"),
            card(3, "Todo", 0, "wire up the importer so it reads in chunks"),
            card(4, "Todo", 1, "chase the vendor about the encoding"),
            card(5, "Doing", 0, "fix the auth refresh"),
            card(6, "Done", 0, "ship the migration"),
        ];
        cards[2].due = Some(now + 2 * day);
        cards[2].tags = vec!["api".into(), "p1".into()];
        cards[2].assist = Some(3 * day);
        cards[3].due = Some(now - day);
        cards[3].tags = vec!["vendor".into()];
        cards[4].handed = Some(47);
        cards[4].comments.push(Comment { by: "builder".into(), at: now, body: "on it".into() });
        cards[5].due = Some(now - 9 * day);

        let cols = columns(Some(&reply(cards)), None, false, "");
        for (w, h) in [(120u16, 26u16), (72, 20), (100, 30)] {
            let a = area(w, h);
            let mut buf = Buffer::empty(a);
            let theme = Theme::horde();
            // The last size is the card view, so both halves of the feature can be looked at
            // in one run — the board is where you work and the card is where you decide.
            if w == 100 {
                draw_detail(&mut buf, a, &theme, &a_full_card(now), Field::Body, None, 0, now);
            } else {
                let scroll = vec![0u16; cols.len()];
                let lay = layout(&cols, a, &scroll, 1);
                draw(&mut buf, &theme, &cols, &lay, 1, Some(3), None, now);
            }
            println!("\n  {w}x{h}");
            for y in 0..h {
                let mut line = String::new();
                for x in 0..w {
                    let st = buf[(x, y)].style();
                    let esc = |c: Option<ratatui::style::Color>, base: u8| match c {
                        Some(ratatui::style::Color::Rgb(r, g, b)) => {
                            format!("\x1b[{base};2;{r};{g};{b}m")
                        }
                        _ => String::new(),
                    };
                    line.push_str(&esc(st.fg, 38));
                    line.push_str(&esc(st.bg, 48));
                    line.push_str(buf[(x, y)].symbol());
                }
                println!("  {line}\x1b[0m");
            }
        }
    }

    // -- one card ------------------------------------------------------------

    fn a_full_card(now: u64) -> Card {
        let mut c = card(12, "Todo", 0, "wire up the importer");
        c.body = "Read the CSV in chunks so a 200MB file doesn't blow the heap.\n\nAsk the vendor about the encoding.".into();
        c.due = Some(now + 2 * 86_400_000);
        c.tags = vec!["api".into(), "p1".into()];
        c.project = Some("horde".into());
        c.assist = Some(3 * 86_400_000);
        c.comments = vec![
            Comment {
                by: "josh@joshmacbook".into(),
                at: now - 7_200_000,
                body: "parked until the schema lands".into(),
            },
            Comment {
                by: "builder".into(),
                at: now - 600_000,
                body: "schema landed, picking this up".into(),
            },
        ];
        c
    }

    fn detail(card: &Card, w: u16, h: u16, focus: Field, now: u64) -> String {
        let a = area(w, h);
        let mut buf = Buffer::empty(a);
        draw_detail(&mut buf, a, &Theme::horde(), card, focus, None, 0, now);
        (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Everything about a card, on one screen — including the two things only this view can
    /// say: what the agents will do about it, and who said what.
    #[test]
    fn the_card_view_shows_the_thread_and_the_agent_arrangement() {
        let now = 1_700_000_000_000u64;
        let out = detail(&a_full_card(now), 100, 40, Field::Title, now);
        assert!(out.contains("#12"), "{out}");
        assert!(out.contains("in 2d"), "the date reads as a distance: {out}");
        assert!(out.contains("#api"), "{out}");
        assert!(out.contains("horde"), "the project: {out}");
        assert!(out.contains("hand over when due within 3d"), "what the agents will do: {out}");
        assert!(out.contains("josh@joshmacbook"), "your own name on your own note: {out}");
        assert!(out.contains("builder"), "and the agent's on its: {out}");
        assert!(out.contains("blow the heap"), "the description: {out}");
    }

    /// An armed card that can never fire has to say why on the one screen that shows it.
    #[test]
    fn an_armed_card_with_nothing_to_fire_on_says_what_is_missing() {
        let now = 1_700_000_000_000u64;
        let mut c = a_full_card(now);
        c.project = None;
        assert!(detail(&c, 100, 40, Field::Assist, now).contains("needs a project"));
        c.due = None;
        assert!(detail(&c, 100, 40, Field::Assist, now).contains("needs a due date"));
        c.assist = None;
        assert!(detail(&c, 100, 40, Field::Assist, now).contains("this one is yours"));
    }

    /// Handed over is a fact and armed is a promise; the card view is where the difference
    /// has room to be spelled out.
    #[test]
    fn a_handed_over_card_names_the_task_it_became() {
        let now = 1_700_000_000_000u64;
        let mut c = a_full_card(now);
        c.handed = Some(47);
        assert!(detail(&c, 100, 40, Field::Title, now).contains("task #47"));
    }

    #[test]
    fn the_card_view_survives_a_window_too_small_for_it() {
        let now = 1_700_000_000_000u64;
        let c = a_full_card(now);
        for (w, h) in [(20u16, 5u16), (40, 10), (100, 40), (200, 60)] {
            let out = detail(&c, w, h, Field::Body, now);
            assert_eq!(out.lines().count(), h as usize, "{w}x{h}");
        }
    }

    // -- what a card says -----------------------------------------------------

    /// The flags are the only place the two boards are visible at once, so they have to be
    /// told apart: armed is a promise, handed over is a fact.
    #[test]
    fn a_card_says_whether_the_agents_have_it_or_merely_might() {
        let mut c = card(1, "Todo", 0, "a");
        assert!(flags(&c).is_empty());
        c.assist = Some(1000);
        assert!(flags(&c).iter().any(|(t, _)| t.contains("armed")));
        c.handed = Some(47);
        let f = flags(&c);
        assert!(f.iter().any(|(t, _)| t.contains("agents")));
        assert!(!f.iter().any(|(t, _)| t.contains("armed")), "one or the other, never both");
        c.comments.push(Comment { by: "builder".into(), at: 0, body: "done".into() });
        assert!(flags(&c).iter().any(|(t, _)| *t == "💬"));
    }
}
