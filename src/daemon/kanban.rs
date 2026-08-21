//! A task board of your own, which is the other kind of board.
//!
//! [`super::tasks`] is a board agents pull work from, and every rule in it is written for
//! them: a claim is a compare-and-set, an open task stops being offered after a day, a
//! crashed agent's work goes back in the pool. Those rules are right there and wrong here.
//! Work you are keeping track of yourself sits for weeks without that meaning anything, moves
//! between columns you named, and is nobody's to claim.
//!
//! So this is a second board rather than a mode of the first one. Different file, different
//! ids, different words — the agent board has **tasks**, this has **cards** in **columns** —
//! and nothing in `tasks.rs` had to change to make room for it. The alternative was one board
//! with a flag on every rule, which is how both halves end up subtly wrong.
//!
//! # Where the two meet
//!
//! At one seam, and only when asked. A card can be *armed*: hand this to the agents when its
//! due date is this close. When that fires the daemon puts a real task on the agent board,
//! scoped to the card's project and linked back here, and whatever the agent reports comes
//! home as a comment. The card does not move itself — deciding a thing is done is the part
//! you wanted the board for.
//!
//! Arming needs both a due date and a project, and refuses rather than guessing at either. A
//! window with no date can never fire, and a task with no project is exactly the failure
//! `tasks.rs` documents at length: work handed to an idle agent sitting in the wrong tree.
//!
//! # Columns
//!
//! A column is a name, kept on the card. Not an id, and not an index: both would make
//! renaming or reordering the configured list quietly rewrite what every card means. The
//! consequence is that a card can name a column that is no longer configured — after a rename
//! or a delete — and the rule is that such a card is never lost. It keeps its name, and the
//! view shows it in a column of its own at the end rather than dropping it on the floor.
//!
//! # Order
//!
//! `pos` is dense within a column, renumbered on every move. Fractional ranking buys nothing
//! here: there is one writer, no optimistic UI, and columns hold tens of cards. What it does
//! buy is a class of bug where two cards drift onto the same key and their order starts
//! depending on the sort's stability.
//!
//! A move names the card to land *behind* rather than an index, because the client may be
//! looking at a filtered board. An index into "the three cards I can see" is not an index
//! into the eleven that are there, and resolving that on the client would mean the client
//! knowing about cards it deliberately is not showing.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::proto::{Card, CardPatch, Comment};

/// Cards kept in memory. Beyond this the oldest archived ones are forgotten.
const CAP: usize = 2000;

/// The longest a card's title may be.
///
/// A title is the thing that has to fit on a card in a column, and a column is twenty-odd
/// cells wide. Past this it is a description wearing a title's clothes, and the card view has
/// a field for that.
pub const MAX_TITLE: usize = 200;

/// The most any one text field may carry — description, or a single comment.
pub const MAX_BODY: usize = 16 * 1024;

impl Card {
    /// Whether arming this card could ever fire.
    ///
    /// Returned as the reason it cannot rather than a bare `bool`, because the card view has
    /// to say *why* an armed card is sitting there doing nothing. An armed card that silently
    /// never fires is worse than one that was never armed.
    pub fn assist_blocker(&self) -> Option<&'static str> {
        match self {
            c if c.assist.is_none() => None,
            c if c.due.is_none() => Some("needs a due date"),
            c if c.project.is_none() => Some("needs a project"),
            _ => None,
        }
    }

    /// Ready to be handed over as of `now`.
    pub fn assist_due(&self, now: u64) -> bool {
        let (Some(window), Some(due)) = (self.assist, self.due) else { return false };
        !self.archived
            && self.handed.is_none()
            && self.project.is_some()
            && due.saturating_sub(now) <= window
    }

}

pub struct Kanban {
    log: super::logfile::AppendLog,
    cards: Vec<Card>,
    next_id: u64,
}

impl Kanban {
    pub fn new(path: PathBuf) -> Kanban {
        let cards = read_log(&path);
        let next_id = cards.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        Kanban { log: super::logfile::AppendLog::new(path), cards, next_id }
    }

    pub fn all(&self) -> &[Card] {
        &self.cards
    }

    pub fn get(&self, id: u64) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == id)
    }

    /// Cards due by `through`, for the count the sidebar carries.
    ///
    /// The caller passes the end of today rather than "now" so that a card due this afternoon
    /// counts from the morning. A count that only notices a deadline once it is missed is a
    /// count you learn to ignore.
    pub fn due_count(&self, through: u64) -> usize {
        self.cards.iter().filter(|c| !c.archived && c.due.is_some_and(|d| d <= through)).count()
    }

    pub fn add(&mut self, title: &str, column: &str, project: Option<&str>) -> Result<Card> {
        let title = clip_title(title)?;
        let column = column.trim();
        if column.is_empty() {
            return Err(anyhow!("a card needs a column"));
        }
        let now = super::now_millis();
        // At the end of its column: a board you are filling is a queue, and a new card
        // appearing above work you already ordered would undo the ordering you just did.
        let pos = self.end_of(column);
        let card = Card {
            id: self.next_id,
            title,
            column: column.to_string(),
            pos,
            body: String::new(),
            due: None,
            tags: Vec::new(),
            project: project.map(|p| p.to_string()),
            created: now,
            updated: now,
            archived: false,
            comments: Vec::new(),
            assist: None,
            handed: None,
        };
        self.next_id += 1;
        self.cards.push(card.clone());
        self.append(&card);
        self.forget_oldest_archived();
        Ok(card)
    }

    /// Change whatever the patch names and leave the rest alone.
    pub fn edit(&mut self, id: u64, patch: &CardPatch) -> Result<Card> {
        // Validate before touching anything, so a rejected patch leaves no half-applied card.
        let title = patch.title.as_deref().map(clip_title).transpose()?;
        if let Some(body) = &patch.body {
            if body.len() > MAX_BODY {
                return Err(anyhow!("a description is at most {MAX_BODY} bytes"));
            }
        }
        let idx = self.index_of(id)?;
        let c = &mut self.cards[idx];
        if let Some(t) = title {
            c.title = t;
        }
        if let Some(b) = &patch.body {
            c.body = b.clone();
        }
        if let Some(d) = patch.due {
            c.due = d;
        }
        if let Some(t) = &patch.tags {
            c.tags = clean_tags(t);
        }
        if let Some(p) = &patch.project {
            c.project = p.clone().filter(|s| !s.trim().is_empty());
        }
        if let Some(a) = patch.assist {
            c.assist = a;
        }
        c.updated = super::now_millis();
        let out = c.clone();
        self.append(&out);
        Ok(out)
    }

    /// Move a card into `column`, landing behind `after`.
    ///
    /// `after` is a card id rather than an index because the caller may be looking at a
    /// filtered board — see the module docs. `None` means the top of the column. An `after`
    /// that is not in the destination column is treated as `None` rather than as an error:
    /// it means the board moved under a drag that was already in flight, and dropping the
    /// card at the top is a better answer than refusing the gesture.
    pub fn place(&mut self, id: u64, column: &str, after: Option<u64>) -> Result<Card> {
        let column = column.trim();
        if column.is_empty() {
            return Err(anyhow!("a card needs a column"));
        }
        let idx = self.index_of(id)?;
        let from = self.cards[idx].column.clone();
        self.cards[idx].column = column.to_string();
        self.cards[idx].updated = super::now_millis();

        // Ids of the destination column in their current order, this card removed.
        let mut order: Vec<u64> = self.ordered(column).into_iter().filter(|c| *c != id).collect();
        let at = match after {
            Some(a) => order.iter().position(|c| *c == a).map(|i| i + 1).unwrap_or(0),
            None => 0,
        };
        order.insert(at, id);
        self.renumber(&order);
        // The column it left has a hole in its numbering now. Harmless to read, but it makes
        // every later `after` resolution depend on a gap that means nothing.
        if !from.eq_ignore_ascii_case(column) {
            let old = self.ordered(&from);
            self.renumber(&old);
        }
        let out = self.cards[self.index_of(id)?].clone();
        Ok(out)
    }

    pub fn comment(&mut self, id: u64, by: &str, body: &str) -> Result<Card> {
        let body = body.trim();
        if body.is_empty() {
            return Err(anyhow!("an empty comment is not a comment"));
        }
        if body.len() > MAX_BODY {
            return Err(anyhow!("a comment is at most {MAX_BODY} bytes"));
        }
        let idx = self.index_of(id)?;
        let now = super::now_millis();
        let c = &mut self.cards[idx];
        c.comments.push(Comment { by: by.to_string(), at: now, body: body.to_string() });
        c.updated = now;
        let out = c.clone();
        self.append(&out);
        Ok(out)
    }

    pub fn archive(&mut self, id: u64, on: bool) -> Result<Card> {
        let idx = self.index_of(id)?;
        let c = &mut self.cards[idx];
        c.archived = on;
        c.updated = super::now_millis();
        let out = c.clone();
        self.append(&out);
        self.forget_oldest_archived();
        Ok(out)
    }

    /// Rename a column, carrying its cards with it.
    ///
    /// The cards hold the name, so this is the write that keeps them from being orphaned by
    /// an edit to the configured list. Renaming onto an existing column merges the two, which
    /// is what naming a column something that already exists asks for.
    pub fn rename_column(&mut self, from: &str, to: &str) -> Vec<Card> {
        let to = to.trim().to_string();
        if to.is_empty() || from.eq_ignore_ascii_case(&to) {
            return Vec::new();
        }
        let mut base = self.end_of(&to);
        let now = super::now_millis();
        let mut moved = Vec::new();
        // In their existing order, so a rename does not shuffle a column you had arranged.
        for id in self.ordered(from) {
            let Ok(i) = self.index_of(id) else { continue };
            let c = &mut self.cards[i];
            c.column = to.clone();
            c.pos = base;
            c.updated = now;
            base += 1;
            moved.push(c.clone());
        }
        for c in &moved {
            self.append(c);
        }
        moved
    }

    /// Cards whose armed window has arrived, oldest first.
    ///
    /// Read-only: handing over is two writes in two different places — a task on the agent
    /// board and a mark here — and doing half of it inside a getter is how the two end up
    /// disagreeing after a failure.
    pub fn ready_to_hand_over(&self, now: u64) -> Vec<u64> {
        let mut ids: Vec<&Card> = self.cards.iter().filter(|c| c.assist_due(now)).collect();
        ids.sort_by_key(|c| c.due.unwrap_or(u64::MAX));
        ids.into_iter().map(|c| c.id).collect()
    }

    /// Record that a card is now the agent board's problem too.
    pub fn mark_handed(&mut self, id: u64, task: u64) -> Result<Card> {
        let idx = self.index_of(id)?;
        let now = super::now_millis();
        let c = &mut self.cards[idx];
        c.handed = Some(task);
        c.updated = now;
        c.comments.push(Comment {
            by: "horde".into(),
            at: now,
            body: format!("handed to the agents as task #{task}"),
        });
        let out = c.clone();
        self.append(&out);
        Ok(out)
    }

    /// A board task this card was handed to has finished, one way or another.
    ///
    /// The card gains a comment in the agent's own name and stays exactly where it is.
    /// Moving it would be horde deciding a column means "an agent touched this", which is a
    /// thing only the person who named the columns can know.
    ///
    /// A task that was dropped rather than finished releases the link, so the card can be
    /// armed again. A finished one keeps it: the link is the record of what was done.
    pub fn on_task_settled(
        &mut self,
        task: u64,
        by: &str,
        result: Option<&str>,
        dropped: bool,
    ) -> Option<Card> {
        let idx = self.cards.iter().position(|c| c.handed == Some(task))?;
        let now = super::now_millis();
        let c = &mut self.cards[idx];
        let body = match (dropped, result) {
            (true, _) => "gave up on it — the card is armable again".to_string(),
            (false, Some(r)) if !r.trim().is_empty() => format!("done — {}", r.trim()),
            (false, _) => "done, with nothing to add".to_string(),
        };
        c.comments.push(Comment { by: by.to_string(), at: now, body });
        if dropped {
            c.handed = None;
        }
        c.updated = now;
        let out = c.clone();
        self.append(&out);
        Some(out)
    }

    // -- internals ---------------------------------------------------------

    fn index_of(&self, id: u64) -> Result<usize> {
        self.cards.iter().position(|c| c.id == id).ok_or_else(|| anyhow!("no card #{id}"))
    }

    /// Ids in a column, in the order they are shown.
    fn ordered(&self, column: &str) -> Vec<u64> {
        let mut in_col: Vec<&Card> =
            self.cards.iter().filter(|c| c.column.eq_ignore_ascii_case(column)).collect();
        // Id breaks a tie, so two cards that somehow share a position still have one order
        // rather than whichever the sort happened to leave them in.
        in_col.sort_by_key(|c| (c.pos, c.id));
        in_col.into_iter().map(|c| c.id).collect()
    }

    fn end_of(&self, column: &str) -> u32 {
        self.cards
            .iter()
            .filter(|c| c.column.eq_ignore_ascii_case(column))
            .map(|c| c.pos + 1)
            .max()
            .unwrap_or(0)
    }

    /// Write `0..n` over the given order and log every card whose position actually moved.
    fn renumber(&mut self, order: &[u64]) {
        let mut changed = Vec::new();
        for (i, id) in order.iter().enumerate() {
            let Ok(idx) = self.index_of(*id) else { continue };
            if self.cards[idx].pos != i as u32 {
                self.cards[idx].pos = i as u32;
                changed.push(self.cards[idx].clone());
            }
        }
        // Always log the card that was asked to move, even when it landed on the number it
        // already had — its column may have changed, and that is the write that matters.
        for c in &changed {
            self.append(c);
        }
    }

    fn forget_oldest_archived(&mut self) {
        while self.cards.len() > CAP {
            match self.cards.iter().position(|c| c.archived) {
                Some(i) => {
                    self.cards.remove(i);
                }
                None => break,
            }
        }
    }

    fn append(&mut self, card: &Card) {
        if let Ok(line) = serde_json::to_string(card) {
            self.log.append_line(&line);
        }
        if self.log.rotation_due() {
            let carry: Vec<String> =
                self.cards.iter().filter_map(|c| serde_json::to_string(c).ok()).collect();
            self.log.rotate(&carry);
        }
    }
}

/// Replay the log. Later entries for an id supersede earlier ones.
fn read_log(path: &PathBuf) -> Vec<Card> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<Card> = Vec::new();
    for line in text.lines() {
        let Ok(c) = serde_json::from_str::<Card>(line) else { continue };
        match out.iter_mut().find(|x| x.id == c.id) {
            Some(slot) => *slot = c,
            None => out.push(c),
        }
    }
    out.sort_by_key(|c| c.id);
    out
}

fn clip_title(title: &str) -> Result<String> {
    let t = title.trim();
    if t.is_empty() {
        return Err(anyhow!("a card needs a title"));
    }
    if t.chars().count() > MAX_TITLE {
        return Err(anyhow!("a title is at most {MAX_TITLE} characters"));
    }
    // A newline in a title would draw as a card that is one line taller than the layout
    // budgeted for, which is a corrupt frame rather than a long title.
    Ok(t.replace(['\n', '\r'], " "))
}

/// Trim, lowercase, drop a leading `#`, and drop duplicates and blanks.
///
/// Lowercased because `#API` and `#api` are one tag that would otherwise filter as two, and
/// a board where the filter misses half its own cards is worse than one with no filter.
fn clean_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim().trim_start_matches('#').trim().to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(name: &str) -> Kanban {
        let p = std::env::temp_dir()
            .join(format!("horde-kanban-{name}-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Kanban::new(p)
    }

    fn patch() -> CardPatch {
        CardPatch::default()
    }

    /// Ids in a column, top to bottom — the thing every ordering test wants to assert.
    fn order(k: &Kanban, column: &str) -> Vec<u64> {
        k.ordered(column)
    }

    #[test]
    fn cards_are_added_to_the_end_of_their_column_and_numbered() {
        let mut k = board("add");
        let a = k.add("first", "Todo", Some("horde")).unwrap();
        let b = k.add("second", "Todo", Some("horde")).unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert_eq!((a.pos, b.pos), (0, 1));
        assert_eq!(order(&k, "Todo"), [1, 2], "a new card does not jump the queue");
        assert!(k.add("  ", "Todo", None).is_err(), "an empty title is not a title");
        assert!(k.add("x", "  ", None).is_err(), "a card needs a column");
    }

    /// A title that wrapped onto a second line would draw one row taller than the layout
    /// budgeted for, which is a corrupt frame rather than a long title.
    #[test]
    fn a_title_cannot_carry_a_newline() {
        let mut k = board("newline");
        let c = k.add("two\nlines", "Todo", None).unwrap();
        assert_eq!(c.title, "two lines");
    }

    /// The move that a filtered board makes dangerous: an index into what you can see is not
    /// an index into what is there, so a move names the card to land behind instead.
    #[test]
    fn a_card_lands_behind_the_one_it_was_dropped_on() {
        let mut k = board("place");
        for t in ["a", "b", "c", "d"] {
            k.add(t, "Todo", None).unwrap();
        }
        // Drop d behind a.
        k.place(4, "Todo", Some(1)).unwrap();
        assert_eq!(order(&k, "Todo"), [1, 4, 2, 3]);
        // And to the top.
        k.place(3, "Todo", None).unwrap();
        assert_eq!(order(&k, "Todo"), [3, 1, 4, 2]);
    }

    /// A drag in flight while the board changed underneath it. Dropping the card at the top
    /// beats refusing the gesture and leaving it where it was.
    #[test]
    fn dropping_behind_a_card_that_is_not_there_lands_at_the_top() {
        let mut k = board("place-missing");
        k.add("a", "Todo", None).unwrap();
        k.add("b", "Todo", None).unwrap();
        k.place(2, "Todo", Some(999)).unwrap();
        assert_eq!(order(&k, "Todo"), [2, 1]);
    }

    #[test]
    fn moving_between_columns_renumbers_both_of_them() {
        let mut k = board("cross");
        for t in ["a", "b", "c"] {
            k.add(t, "Todo", None).unwrap();
        }
        k.add("x", "Doing", None).unwrap();
        // b crosses over, behind x.
        k.place(2, "Doing", Some(4)).unwrap();
        assert_eq!(order(&k, "Doing"), [4, 2]);
        assert_eq!(order(&k, "Todo"), [1, 3]);
        // The column it left is dense again, not 0 and 2.
        let left: Vec<u32> =
            [1, 3].iter().map(|id| k.get(*id).unwrap().pos).collect();
        assert_eq!(left, [0, 1], "the hole it left is closed");
    }

    #[test]
    fn a_column_is_matched_without_regard_to_case() {
        let mut k = board("case");
        k.add("a", "Todo", None).unwrap();
        k.add("b", "todo", None).unwrap();
        assert_eq!(order(&k, "TODO").len(), 2, "one column, however it was typed");
    }

    /// The cards hold the column name, so renaming the configured list has to carry them or
    /// they are orphaned by an edit to a config file.
    #[test]
    fn renaming_a_column_carries_its_cards_in_order() {
        let mut k = board("rename");
        for t in ["a", "b", "c"] {
            k.add(t, "Todo", None).unwrap();
        }
        k.place(3, "Todo", None).unwrap(); // c to the top
        assert_eq!(order(&k, "Todo"), [3, 1, 2]);

        let moved = k.rename_column("Todo", "Next");
        assert_eq!(moved.len(), 3);
        assert_eq!(order(&k, "Next"), [3, 1, 2], "the arrangement survives the rename");
        assert!(order(&k, "Todo").is_empty());
    }

    #[test]
    fn renaming_onto_an_existing_column_merges_and_appends() {
        let mut k = board("merge");
        k.add("a", "Doing", None).unwrap();
        k.add("b", "Todo", None).unwrap();
        k.rename_column("Todo", "Doing");
        assert_eq!(order(&k, "Doing"), [1, 2], "the merged cards land after, not among");
    }

    #[test]
    fn editing_changes_only_what_the_patch_names() {
        let mut k = board("edit");
        k.add("a", "Todo", Some("horde")).unwrap();
        let p = CardPatch { body: Some("the long version".into()), ..patch() };
        let c = k.edit(1, &p).unwrap();
        assert_eq!(c.title, "a", "an unnamed field is left alone");
        assert_eq!(c.body, "the long version");
        assert_eq!(c.project.as_deref(), Some("horde"));

        // A double option is how "clear it" is said apart from "leave it".
        let c = k.edit(1, &CardPatch { due: Some(Some(1_000)), ..patch() }).unwrap();
        assert_eq!(c.due, Some(1_000));
        let c = k.edit(1, &CardPatch { due: Some(None), ..patch() }).unwrap();
        assert_eq!(c.due, None, "Some(None) clears it");
        let c = k.edit(1, &patch()).unwrap();
        assert_eq!(c.body, "the long version", "None leaves everything alone");
    }

    /// `#API` and `#api` are one tag. Two would mean a filter that misses half its own cards.
    #[test]
    fn tags_are_normalised_and_deduplicated() {
        let mut k = board("tags");
        k.add("a", "Todo", None).unwrap();
        let p = CardPatch {
            tags: Some(vec!["#API".into(), "api".into(), "  ".into(), " P1 ".into()]),
            ..patch()
        };
        assert_eq!(k.edit(1, &p).unwrap().tags, ["api", "p1"]);
    }

    #[test]
    fn a_rejected_patch_leaves_the_card_untouched() {
        let mut k = board("reject");
        k.add("a", "Todo", None).unwrap();
        let p = CardPatch {
            title: Some("  ".into()),
            body: Some("would have applied".into()),
            ..patch()
        };
        assert!(k.edit(1, &p).is_err());
        let c = k.get(1).unwrap();
        assert_eq!(c.title, "a");
        assert_eq!(c.body, "", "nothing was half-applied");
    }

    #[test]
    fn comments_are_authored_timestamped_and_kept_in_order() {
        let mut k = board("comments");
        k.add("a", "Todo", None).unwrap();
        k.comment(1, "josh@joshmacbook", " parked until the schema lands ").unwrap();
        let c = k.comment(1, "builder", "picking this up").unwrap();
        assert_eq!(c.comments.len(), 2);
        assert_eq!(c.comments[0].by, "josh@joshmacbook");
        assert_eq!(c.comments[0].body, "parked until the schema lands");
        assert_eq!(c.comments[1].by, "builder");
        assert!(c.comments[0].at <= c.comments[1].at);
        assert!(k.comment(1, "x", "   ").is_err(), "an empty comment is not a comment");
    }

    /// An armed card that can never fire has to say so, or it sits there looking handled.
    #[test]
    fn arming_says_what_it_is_still_missing() {
        let mut k = board("blocker");
        k.add("a", "Todo", None).unwrap();
        assert_eq!(k.get(1).unwrap().assist_blocker(), None, "unarmed blocks nothing");

        k.edit(1, &CardPatch { assist: Some(Some(86_400_000)), ..patch() }).unwrap();
        assert_eq!(k.get(1).unwrap().assist_blocker(), Some("needs a due date"));

        k.edit(1, &CardPatch { due: Some(Some(super::super::now_millis())), ..patch() }).unwrap();
        assert_eq!(k.get(1).unwrap().assist_blocker(), Some("needs a project"));

        k.edit(1, &CardPatch { project: Some(Some("horde".into())), ..patch() }).unwrap();
        assert_eq!(k.get(1).unwrap().assist_blocker(), None);
    }

    /// The failure `tasks.rs` documents at length, in this direction: work handed to an agent
    /// sitting in the wrong tree. A card with no project is never handed over.
    #[test]
    fn only_a_fully_armed_card_is_ever_handed_over() {
        let now = 1_000_000_000;
        let day = 86_400_000u64;
        let mut k = board("ready");

        // Armed, due tomorrow, window of two days, has a project: ready.
        k.add("ready", "Todo", Some("horde")).unwrap();
        k.edit(1, &CardPatch { due: Some(Some(now + day)), assist: Some(Some(2 * day)), ..patch() })
            .unwrap();

        // Same, but the due date is a week out: not yet.
        k.add("later", "Todo", Some("horde")).unwrap();
        k.edit(2, &CardPatch { due: Some(Some(now + 7 * day)), assist: Some(Some(2 * day)), ..patch() })
            .unwrap();

        // Armed and due, but nobody said which project.
        k.add("homeless", "Todo", None).unwrap();
        k.edit(3, &CardPatch { due: Some(Some(now)), assist: Some(Some(2 * day)), ..patch() })
            .unwrap();

        // Due and scoped, but never armed.
        k.add("unarmed", "Todo", Some("horde")).unwrap();
        k.edit(4, &CardPatch { due: Some(Some(now)), ..patch() }).unwrap();

        assert_eq!(k.ready_to_hand_over(now), [1]);
    }

    #[test]
    fn a_card_is_only_handed_over_once() {
        let now = 1_000_000_000;
        let mut k = board("once");
        k.add("a", "Todo", Some("horde")).unwrap();
        k.edit(1, &CardPatch { due: Some(Some(now)), assist: Some(Some(1000)), ..patch() })
            .unwrap();
        assert_eq!(k.ready_to_hand_over(now), [1]);
        k.mark_handed(1, 47).unwrap();
        assert!(k.ready_to_hand_over(now).is_empty(), "the mark is what stops it repeating");
        assert!(k.get(1).unwrap().comments.iter().any(|c| c.body.contains("#47")));
    }

    /// The card does not move itself. Deciding a thing is done is the part the board is for.
    #[test]
    fn a_finished_task_comments_on_its_card_without_moving_it() {
        let mut k = board("settled");
        k.add("a", "Todo", Some("horde")).unwrap();
        k.mark_handed(1, 47).unwrap();

        let c = k.on_task_settled(47, "builder", Some("chunked reader, tests green"), false)
            .expect("the card holding that task");
        assert_eq!(c.column, "Todo", "the column is yours to change, not horde's");
        let last = c.comments.last().unwrap();
        assert_eq!(last.by, "builder");
        assert!(last.body.contains("chunked reader"));
        assert_eq!(c.handed, Some(47), "a finished link is the record of what was done");
    }

    #[test]
    fn a_dropped_task_releases_its_card_to_be_armed_again() {
        let mut k = board("dropped");
        k.add("a", "Todo", Some("horde")).unwrap();
        k.mark_handed(1, 47).unwrap();
        let c = k.on_task_settled(47, "builder", None, true).unwrap();
        assert_eq!(c.handed, None);
        assert!(c.comments.last().unwrap().body.contains("armable again"));
    }

    #[test]
    fn a_task_no_card_was_handed_is_nobodys_business() {
        let mut k = board("unrelated");
        k.add("a", "Todo", None).unwrap();
        assert!(k.on_task_settled(99, "builder", None, false).is_none());
        assert!(k.get(1).unwrap().comments.is_empty());
    }

    #[test]
    fn the_board_survives_a_restart() {
        let p = std::env::temp_dir()
            .join(format!("horde-kanban-persist-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        {
            let mut k = Kanban::new(p.clone());
            k.add("first", "Todo", Some("horde")).unwrap();
            k.add("second", "Todo", None).unwrap();
            k.place(2, "Doing", None).unwrap();
            k.comment(1, "josh@joshmacbook", "still thinking about it").unwrap();
            k.edit(1, &CardPatch { due: Some(Some(1_700_000_000_000)), ..patch() }).unwrap();
        }
        let mut k = Kanban::new(p.clone());
        assert_eq!(k.all().len(), 2);
        assert_eq!(k.get(1).unwrap().due, Some(1_700_000_000_000));
        assert_eq!(k.get(1).unwrap().comments.len(), 1);
        assert_eq!(k.get(2).unwrap().column, "Doing");
        // Ids do not restart, so a replayed log cannot collide with new work.
        assert_eq!(k.add("third", "Todo", None).unwrap().id, 3);
        let _ = std::fs::remove_file(&p);
    }

    /// A card written before a field existed still loads. Every field added after the first
    /// release carries `#[serde(default)]` for exactly this.
    #[test]
    fn a_card_from_before_a_field_existed_still_loads() {
        let p = std::env::temp_dir()
            .join(format!("horde-kanban-old-{}.jsonl", std::process::id()));
        std::fs::write(
            &p,
            "{\"id\":1,\"title\":\"early\",\"column\":\"Todo\",\"pos\":0,\"created\":5}\n",
        )
        .unwrap();
        let k = Kanban::new(p.clone());
        let c = k.get(1).expect("it loaded");
        assert_eq!(c.title, "early");
        assert!(c.comments.is_empty());
        assert_eq!(c.due, None);
        assert!(!c.archived);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_malformed_log_line_is_skipped_rather_than_fatal() {
        let p = std::env::temp_dir()
            .join(format!("horde-kanban-broken-{}.jsonl", std::process::id()));
        std::fs::write(&p, "not json\n{\"partial\":true}\n").unwrap();
        assert!(Kanban::new(p.clone()).all().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    /// The trap in bounding a log that gets replayed: the ordinary "rename and start empty"
    /// would drop every card on the floor.
    #[test]
    fn rotating_the_log_keeps_the_cards_and_their_threads() {
        let p = std::env::temp_dir()
            .join(format!("horde-kanban-rotate-{}.jsonl", std::process::id()));
        let archive = p.with_file_name(format!(
            "{}.1",
            p.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&archive);
        {
            let mut k = Kanban::new(p.clone());
            k.log = crate::daemon::logfile::AppendLog::with_max(p.clone(), 1);
            k.add("kept", "Todo", None).unwrap();
            k.comment(1, "josh@joshmacbook", "the one that matters").unwrap();
            for i in 0..300 {
                k.add(&format!("filler {i}"), "Todo", None).unwrap();
            }
        }
        assert!(archive.exists(), "history should have been archived");

        let k = Kanban::new(p.clone());
        let c = k.get(1).expect("the card survived rotation");
        assert_eq!(c.title, "kept");
        assert_eq!(c.comments.len(), 1, "and its thread with it");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&archive);
    }
}
