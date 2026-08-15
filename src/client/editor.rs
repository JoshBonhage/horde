//! A plain text buffer for writing notes and code.
//!
//! **Modal, but it opens in insert.** horde is also the thing you write code in, and the keys
//! for that are the ones people already have in their hands. The compromise is which end it
//! starts at: a note you just made is a note you are about to write, so the editor opens
//! typing and `esc` is how you reach the commands — rather than opening in normal and making
//! every caught thought start with pressing `i`.
//!
//! An honest subset, not an emulation: motions, the handful of edits people reach for without
//! thinking, and the `:` line. No operator-pending grammar, no counts, no registers beyond one
//! unnamed line, no macros. A half-built `d2w` that silently does the wrong thing is worse
//! than one that plainly does not exist.
//!
//! Pure: no drawing, no daemon. It holds lines and a cursor and answers questions about
//! them, which is what makes it testable as text.

/// Which half of the editor the keyboard is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vim {
    /// Typing types.
    Insert,
    /// Keys are commands.
    Normal,
    /// A key that needs a second one to mean anything: the `d` of `dd`, the `g` of `gg`.
    ///
    /// Its own state rather than a flag, so an abandoned pair can only ever be dropped.
    /// Half of `dd` must never fall through and delete something on its own.
    Pending(char),
    /// The `:` line, holding what has been typed after the colon.
    Command(String),
    /// The `/` line. The same machinery pointed at a different verb.
    Search(String),
}

impl Vim {
    /// Whether typing a printable character puts it in the buffer.
    pub fn typing(&self) -> bool {
        matches!(self, Vim::Insert)
    }

    /// The prompt this mode is reading a line after, if it is reading one.
    pub fn prompt(&self) -> Option<(char, &str)> {
        match self {
            Vim::Command(s) => Some((':', s.as_str())),
            Vim::Search(s) => Some(('/', s.as_str())),
            _ => None,
        }
    }
}

/// What a character counts as when walking words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// A point the buffer can be put back to.
#[derive(Debug, Clone, PartialEq)]
struct Snap {
    lines: Vec<String>,
    line: usize,
    col: usize,
}

/// What kind of change an edit was, for grouping consecutive ones together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Insert,
    Delete,
}

/// Characters in one undo step before it is closed and a new one started.
///
/// Without a cap, a paragraph typed without pausing is a single undo — press it once and
/// the paragraph is gone. Without grouping at all, undo removes one letter at a time, which
/// is worse. Forty is about a line of prose.
const MAX_GROUP: usize = 40;

/// A text buffer and a cursor into it.
#[derive(Debug, Clone)]
pub struct Buffer {
    pub lines: Vec<String>,
    /// Cursor line, and column in *characters* — not bytes, because a note with an em dash
    /// in it would otherwise put the cursor inside a character.
    pub line: usize,
    pub col: usize,
    /// Whether anything has changed since the last save.
    pub dirty: bool,
    /// The column to return to when moving up or down through short lines, so a cursor
    /// walking down a ragged paragraph keeps the column you actually asked for.
    goal: usize,
    /// Bumped by every edit. Highlighting a file costs milliseconds and a frame costs
    /// microseconds, so the drawing side keeps its answer until this changes.
    pub rev: usize,
    /// States to go back to, most recent last.
    undo: Vec<Snap>,
    /// States undone, to go forward to again. Cleared by any new edit, because a redo past
    /// a change that never happened is a buffer nobody can reason about.
    redo: Vec<Snap>,
    /// The last edit's kind and where it left the cursor, so consecutive typing groups into
    /// one undo step rather than one per keystroke.
    last: Option<(Edit, usize, usize)>,
    group: usize,
}

impl Buffer {
    pub fn new(text: &str) -> Buffer {
        let mut lines: Vec<String> = text.split('\n').map(|l| l.to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Buffer {
            lines,
            line: 0,
            col: 0,
            dirty: false,
            goal: 0,
            rev: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last: None,
            group: 0,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn line_len(&self, i: usize) -> usize {
        self.lines.get(i).map(|l| l.chars().count()).unwrap_or(0)
    }

    /// Byte offset of the cursor's column, for slicing.
    fn byte_at(&self, line: usize, col: usize) -> usize {
        self.lines
            .get(line)
            .map(|l| l.char_indices().nth(col).map(|(b, _)| b).unwrap_or(l.len()))
            .unwrap_or(0)
    }

    /// Mark an edit finished: dirty, a new revision, and where it left the cursor so the
    /// next one can tell whether it continues this one.
    fn touched(&mut self, kind: Edit) {
        self.dirty = true;
        self.rev += 1;
        self.last = Some((kind, self.line, self.col));
    }

    fn snap(&self) -> Snap {
        Snap { lines: self.lines.clone(), line: self.line, col: self.col }
    }

    fn restore(&mut self, s: Snap) {
        self.lines = s.lines;
        self.line = s.line;
        self.col = s.col;
        self.goal = s.col;
        self.dirty = true;
        self.rev += 1;
        self.last = None;
    }

    /// Record a point to come back to, unless this edit continues the last one.
    ///
    /// Snapshots the whole buffer rather than the change. A note is kilobytes and an undo
    /// stack of them is nothing; inverting operations would be less memory and considerably
    /// more ways to be subtly wrong about what the buffer used to be.
    fn checkpoint(&mut self, kind: Edit) {
        let continues = self.last == Some((kind, self.line, self.col)) && self.group < MAX_GROUP;
        if !continues {
            self.undo.push(self.snap());
            self.group = 0;
            // A hundred steps back is more than anyone reaches for, and bounds the memory a
            // long session can hold.
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
        }
        self.group += 1;
        self.redo.clear();
    }

    /// Go back one step. Returns false when there is nothing to go back to.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(self.snap());
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(self.snap());
        self.restore(next);
        true
    }

    pub fn insert(&mut self, c: char) {
        self.checkpoint(Edit::Insert);
        let b = self.byte_at(self.line, self.col);
        self.lines[self.line].insert(b, c);
        self.col += 1;
        self.goal = self.col;
        self.touched(Edit::Insert);
    }

    pub fn newline(&mut self) {
        // Always its own step: a line break is where a thought ended, and undoing back
        // through several of them at once loses the shape of what was written.
        self.last = None;
        self.checkpoint(Edit::Insert);
        let b = self.byte_at(self.line, self.col);
        let rest = self.lines[self.line].split_off(b);
        self.lines.insert(self.line + 1, rest);
        self.line += 1;
        self.col = 0;
        self.goal = 0;
        self.touched(Edit::Insert);
        // Closed at both ends: what follows a line break starts its own step, so undo takes
        // back the line you just wrote and then, separately, the break that made room for it.
        self.last = None;
    }

    /// Delete backwards, joining lines when the cursor is at the start of one.
    pub fn backspace(&mut self) {
        self.checkpoint(Edit::Delete);
        if self.col > 0 {
            let start = self.byte_at(self.line, self.col - 1);
            let end = self.byte_at(self.line, self.col);
            self.lines[self.line].replace_range(start..end, "");
            self.col -= 1;
        } else if self.line > 0 {
            let cur = self.lines.remove(self.line);
            self.line -= 1;
            self.col = self.line_len(self.line);
            self.lines[self.line].push_str(&cur);
        } else {
            return; // start of the buffer: nothing to delete
        }
        self.goal = self.col;
        self.touched(Edit::Delete);
    }

    pub fn delete(&mut self) {
        self.checkpoint(Edit::Delete);
        if self.col < self.line_len(self.line) {
            let start = self.byte_at(self.line, self.col);
            let end = self.byte_at(self.line, self.col + 1);
            self.lines[self.line].replace_range(start..end, "");
            self.touched(Edit::Delete);
        } else if self.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.line + 1);
            self.lines[self.line].push_str(&next);
            self.touched(Edit::Delete);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.line > 0 {
            self.line -= 1;
            self.col = self.line_len(self.line);
        }
        self.goal = self.col;
    }

    pub fn right(&mut self) {
        if self.col < self.line_len(self.line) {
            self.col += 1;
        } else if self.line + 1 < self.lines.len() {
            self.line += 1;
            self.col = 0;
        }
        self.goal = self.col;
    }

    pub fn up(&mut self) {
        if self.line > 0 {
            self.line -= 1;
            self.col = self.goal.min(self.line_len(self.line));
        }
    }

    pub fn down(&mut self) {
        if self.line + 1 < self.lines.len() {
            self.line += 1;
            self.col = self.goal.min(self.line_len(self.line));
        }
    }

    /// Put the cursor somewhere deliberately — a click, or opening a note at a link.
    ///
    /// Sets the goal column too. Assigning `col` directly would leave the next `down` to
    /// snap back to a column nobody asked for, which is the kind of bug that feels like the
    /// editor having a mind of its own.
    pub fn goto(&mut self, line: usize, col: usize) {
        self.line = line.min(self.lines.len().saturating_sub(1));
        self.col = col.min(self.line_len(self.line));
        self.goal = self.col;
    }

    pub fn home(&mut self) {
        self.col = 0;
        self.goal = 0;
    }

    pub fn end(&mut self) {
        self.col = self.line_len(self.line);
        self.goal = self.col;
    }

    pub fn saved(&mut self) {
        self.dirty = false;
    }

    // -- normal mode -------------------------------------------------------
    //
    // Everything below is only reachable when the keyboard is in normal mode. It is here
    // rather than in the key handler because a motion is a fact about text — "where is the
    // next word" has a right answer that can be asserted without a terminal.

    /// Put the cursor *on* a character rather than after the last one.
    ///
    /// The two modes disagree about where a line ends: insert has a position after the final
    /// character, because that is where you type the next one, and normal does not, because
    /// there is nothing there to act on. Crossing between them goes through here.
    pub fn clamp(&mut self) {
        let last = self.line_len(self.line).saturating_sub(1);
        if self.col > last {
            self.col = last;
            self.goal = self.col;
        }
    }

    /// `h` and `l`, which stop at the ends of a line rather than wrapping round them.
    pub fn step(&mut self, right: bool) {
        if right {
            if self.col + 1 < self.line_len(self.line) {
                self.col += 1;
            }
        } else {
            self.col = self.col.saturating_sub(1);
        }
        self.goal = self.col;
    }

    pub fn top(&mut self) {
        self.goto(0, 0);
    }

    pub fn bottom(&mut self) {
        self.goto(self.lines.len().saturating_sub(1), 0);
    }

    /// `^`: the first character that is not indentation.
    pub fn first_nonblank(&mut self) {
        let n = self.lines[self.line].chars().take_while(|c| c.is_whitespace()).count();
        self.goto(self.line, n);
        self.clamp();
    }

    fn char_at(&self, (l, c): (usize, usize)) -> Option<char> {
        self.lines.get(l).and_then(|s| s.chars().nth(c))
    }

    /// What is at a position — where the slot past the end of a line reads as whitespace,
    /// which is what makes a line break behave like the word separator it is.
    fn class_at(&self, p: (usize, usize)) -> Class {
        self.char_at(p).map(class).unwrap_or(Class::Space)
    }

    fn fwd(&self, (l, c): (usize, usize)) -> Option<(usize, usize)> {
        if c < self.line_len(l) {
            Some((l, c + 1))
        } else if l + 1 < self.lines.len() {
            Some((l + 1, 0))
        } else {
            None
        }
    }

    fn back(&self, (l, c): (usize, usize)) -> Option<(usize, usize)> {
        if c > 0 {
            Some((l, c - 1))
        } else if l > 0 {
            Some((l - 1, self.line_len(l - 1)))
        } else {
            None
        }
    }

    /// `w`: the start of the next word.
    pub fn word_forward(&mut self) {
        let mut p = (self.line, self.col);
        let start = self.class_at(p);
        if start != Class::Space {
            while self.class_at(p) == start {
                let Some(n) = self.fwd(p) else { break };
                p = n;
            }
        }
        while self.class_at(p) == Class::Space {
            let Some(n) = self.fwd(p) else { break };
            p = n;
        }
        self.goto(p.0, p.1);
        self.clamp();
    }

    /// `b`: the start of the word before this one.
    pub fn word_back(&mut self) {
        let Some(mut p) = self.back((self.line, self.col)) else { return };
        while self.class_at(p) == Class::Space {
            let Some(n) = self.back(p) else { break };
            p = n;
        }
        let here = self.class_at(p);
        while let Some(n) = self.back(p) {
            if self.class_at(n) != here {
                break;
            }
            p = n;
        }
        self.goto(p.0, p.1);
        self.clamp();
    }

    /// `e`: the last character of the word the cursor is heading into.
    pub fn word_end(&mut self) {
        let Some(mut p) = self.fwd((self.line, self.col)) else { return };
        while self.class_at(p) == Class::Space {
            let Some(n) = self.fwd(p) else { break };
            p = n;
        }
        let here = self.class_at(p);
        while let Some(n) = self.fwd(p) {
            if self.class_at(n) != here {
                break;
            }
            p = n;
        }
        self.goto(p.0, p.1);
        self.clamp();
    }

    /// `{` and `}`: the next blank line, which in prose is the next paragraph.
    pub fn paragraph(&mut self, forward: bool) {
        let mut i = self.line;
        loop {
            let next = if forward { i + 1 } else { i.saturating_sub(1) };
            if forward && next >= self.lines.len() {
                i = self.lines.len().saturating_sub(1);
                break;
            }
            if !forward && i == 0 {
                break;
            }
            i = next;
            if self.lines[i].trim().is_empty() {
                break;
            }
        }
        self.goto(i, 0);
    }

    /// `x`: one character, and never a line break — `x` at the end of a line pulling the next
    /// one up is not what anybody means by it.
    pub fn delete_char(&mut self) -> bool {
        if self.col >= self.line_len(self.line) {
            return false;
        }
        self.checkpoint(Edit::Delete);
        let start = self.byte_at(self.line, self.col);
        let end = self.byte_at(self.line, self.col + 1);
        self.lines[self.line].replace_range(start..end, "");
        // Clamped before the edit is stamped: `touched` records where the cursor ended up,
        // and a run of `x` eating the end of a line has to keep grouping as one step.
        self.clamp();
        self.touched(Edit::Delete);
        true
    }

    /// `D`: from the cursor to the end of the line.
    pub fn delete_to_end(&mut self) {
        self.last = None;
        self.checkpoint(Edit::Delete);
        let b = self.byte_at(self.line, self.col);
        self.lines[self.line].truncate(b);
        self.touched(Edit::Delete);
        self.last = None;
    }

    /// `dd`: the whole line, handed back so it can be put down somewhere else.
    pub fn delete_line(&mut self) -> String {
        self.last = None;
        self.checkpoint(Edit::Delete);
        let gone = self.lines.remove(self.line);
        // A buffer always has a line in it, even an empty one: everything else here indexes
        // `lines[self.line]` and a truly empty buffer has no such thing.
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.goto(self.line.min(self.lines.len() - 1), 0);
        self.touched(Edit::Delete);
        self.last = None;
        gone
    }

    /// `cc`: empty the line without removing it, because you are about to write another.
    pub fn clear_line(&mut self) {
        self.last = None;
        self.checkpoint(Edit::Delete);
        self.lines[self.line].clear();
        self.goto(self.line, 0);
        self.touched(Edit::Delete);
        self.last = None;
    }

    /// `p` and `P`, and — with an empty string — `o` and `O`.
    pub fn put_line(&mut self, text: &str, below: bool) {
        self.last = None;
        self.checkpoint(Edit::Insert);
        let at = if below { self.line + 1 } else { self.line };
        self.lines.insert(at, text.to_string());
        self.goto(at, 0);
        self.touched(Edit::Insert);
        self.last = None;
    }

    /// `J`: pull the next line onto this one, with a single space where the break was.
    pub fn join(&mut self) {
        if self.line + 1 >= self.lines.len() {
            return;
        }
        self.last = None;
        self.checkpoint(Edit::Delete);
        let next = self.lines.remove(self.line + 1);
        let at = self.line_len(self.line);
        if !next.trim().is_empty() {
            if !self.lines[self.line].is_empty() {
                self.lines[self.line].push(' ');
            }
            self.lines[self.line].push_str(next.trim_start());
        }
        self.goto(self.line, at);
        self.clamp();
        self.touched(Edit::Delete);
        self.last = None;
    }

    pub fn line_text(&self) -> String {
        self.lines.get(self.line).cloned().unwrap_or_default()
    }

    /// Every place `needle` appears, as cursor positions.
    ///
    /// Smartcase: an all-lowercase search is a search for the word, and one with a capital in
    /// it is a search for that exact spelling — which is the rule people already have, and
    /// the one that makes searching a note for `horde` find the title too.
    ///
    /// Compares character by character rather than lowercasing the line, because case folding
    /// can change how many characters a string has and the answer here is a column.
    fn matches(&self, needle: &str) -> Vec<(usize, usize)> {
        let pat: Vec<char> = needle.chars().collect();
        if pat.is_empty() {
            return Vec::new();
        }
        let fold = !pat.iter().any(|c| c.is_uppercase());
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            let hay: Vec<char> = line.chars().collect();
            for start in 0..hay.len().saturating_sub(pat.len() - 1) {
                let hit = pat.iter().zip(&hay[start..]).all(|(a, b)| {
                    if fold { a.eq_ignore_ascii_case(b) } else { a == b }
                });
                if hit {
                    out.push((i, start));
                }
            }
        }
        out
    }

    /// `/`, `n` and `N`. Wraps, and says whether there was anything to find.
    pub fn search(&mut self, needle: &str, forward: bool) -> bool {
        let hits = self.matches(needle);
        let Some(first) = hits.first().copied() else { return false };
        let here = (self.line, self.col);
        let to = if forward {
            hits.iter().find(|p| **p > here).copied().unwrap_or(first)
        } else {
            hits.iter().rev().find(|p| **p < here).copied().unwrap_or_else(|| hits[hits.len() - 1])
        };
        self.goto(to.0, to.1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_inserts_at_the_cursor_and_splits_lines() {
        let mut b = Buffer::new("hello world");
        b.goto(0, 5);
        b.insert(',');
        assert_eq!(b.text(), "hello, world");
        assert_eq!(b.col, 6, "the cursor follows what you typed");

        b.newline();
        assert_eq!(b.text(), "hello,\n world");
        assert_eq!((b.line, b.col), (1, 0));
        assert!(b.dirty);
    }

    /// Backspace at the start of a line joins it to the one above, which is what every
    /// editor does and what nobody thinks about until it does not.
    #[test]
    fn backspace_joins_lines_and_stops_at_the_start_of_the_buffer() {
        let mut b = Buffer::new("one\ntwo");
        b.goto(1, 0);
        b.backspace();
        assert_eq!(b.text(), "onetwo");
        assert_eq!((b.line, b.col), (0, 3), "the cursor lands where the join happened");

        let mut b = Buffer::new("x");
        b.backspace();
        assert_eq!(b.text(), "x", "nothing to delete, and nothing breaks");
    }

    /// Moving down a ragged paragraph has to remember the column you asked for, or the
    /// cursor walks left every time it crosses a short line and never comes back.
    #[test]
    fn vertical_movement_remembers_the_column_across_short_lines() {
        let mut b = Buffer::new("a long line here\nshort\nanother long line");
        b.goto(0, 12);
        b.down();
        assert_eq!(b.col, 5, "clamped to the short line");
        b.down();
        assert_eq!(b.col, 12, "and back to where it was asked to be");
    }

    /// Columns are characters, not bytes. A note with an em dash in it is not exotic — this
    /// vault's titles are full of them — and byte arithmetic would slice one in half.
    #[test]
    fn multi_byte_characters_are_one_column_each() {
        let mut b = Buffer::new("a — b");
        b.end();
        assert_eq!(b.col, 5, "five characters, not seven bytes");
        b.backspace();
        b.backspace();
        b.backspace();
        assert_eq!(b.text(), "a ", "and each backspace removed exactly one");
    }

    #[test]
    fn delete_removes_forwards_and_joins_the_next_line() {
        let mut b = Buffer::new("ab\ncd");
        b.delete();
        assert_eq!(b.text(), "b\ncd");
        b.end();
        b.delete();
        assert_eq!(b.text(), "bcd", "at the end of a line it pulls the next one up");
    }

    /// Undo removes a word, not a letter. One keystroke per undo is technically correct and
    /// useless — nobody wants to press it eleven times to take back "hello world".
    #[test]
    fn typing_a_run_of_characters_undoes_as_one_step() {
        let mut b = Buffer::new("");
        for c in "hello world".chars() {
            b.insert(c);
        }
        assert_eq!(b.text(), "hello world");
        assert!(b.undo());
        assert_eq!(b.text(), "", "the whole run went back at once");
        assert!(!b.undo(), "and there is nothing before it");
    }

    /// Grouping stops where the shape of the writing does: a line break, a change of
    /// direction, or a cursor that moved somewhere else first.
    #[test]
    fn a_new_step_starts_where_the_writing_changed_direction() {
        let mut b = Buffer::new("");
        for c in "one".chars() {
            b.insert(c);
        }
        b.newline();
        for c in "two".chars() {
            b.insert(c);
        }
        assert_eq!(b.text(), "one\ntwo");
        b.undo();
        assert_eq!(b.text(), "one\n", "the second line, on its own");
        b.undo();
        assert_eq!(b.text(), "one", "then the break");
        b.undo();
        assert_eq!(b.text(), "", "then the first word");

        // Deleting after typing is its own step, not a continuation of it.
        let mut b = Buffer::new("");
        b.insert('a');
        b.backspace();
        assert_eq!(b.text(), "");
        b.undo();
        assert_eq!(b.text(), "a", "the delete came back before the type did");
    }

    /// Where the cursor was is part of what you are going back to. Undo that leaves the
    /// cursor elsewhere makes you find your place again, which is half of why undo exists.
    #[test]
    fn undo_puts_the_cursor_back_where_the_edit_happened() {
        let mut b = Buffer::new("first
second
third");
        b.goto(1, 6);
        b.insert('!');
        b.goto(0, 0);
        b.undo();
        assert_eq!((b.line, b.col), (1, 6), "back to the edit, not left at the top");
    }

    #[test]
    fn redo_goes_forward_again_and_a_new_edit_forgets_it() {
        let mut b = Buffer::new("");
        for c in "abc".chars() {
            b.insert(c);
        }
        b.undo();
        assert_eq!(b.text(), "");
        assert!(b.redo());
        assert_eq!(b.text(), "abc", "forward again");

        b.undo();
        b.insert('z');
        assert!(!b.redo(), "a new edit is a new history; there is no forward from here");
        assert_eq!(b.text(), "z");
    }

    /// A very long run still breaks into steps, or undo becomes all-or-nothing on a
    /// paragraph typed without pausing.
    #[test]
    fn a_long_run_of_typing_is_more_than_one_step() {
        let mut b = Buffer::new("");
        for _ in 0..(MAX_GROUP * 2 + 5) {
            b.insert('x');
        }
        let full = b.text().len();
        b.undo();
        assert!(b.text().len() < full, "something came back");
        assert!(!b.text().is_empty(), "but not the whole lot");
    }

    /// The two modes disagree about where a line ends, and the cursor has to be somewhere
    /// legal in whichever one it is in. Left unclamped, `x` on the position after the last
    /// character silently does nothing and the editor looks broken.
    #[test]
    fn normal_mode_puts_the_cursor_on_a_character_not_after_the_last_one() {
        let mut b = Buffer::new("abc\n\nxy");
        b.end();
        assert_eq!(b.col, 3, "insert can sit past the end, to type there");
        b.clamp();
        assert_eq!(b.col, 2, "normal cannot");

        b.goto(1, 0);
        b.clamp();
        assert_eq!(b.col, 0, "and an empty line is still a place to be");
    }

    /// `h` and `l` stop where the line does. Wrapping onto the next line is what the arrow
    /// keys do while writing; it is not what these mean.
    #[test]
    fn stepping_sideways_stays_on_its_line() {
        let mut b = Buffer::new("ab\ncd");
        b.step(false);
        assert_eq!((b.line, b.col), (0, 0), "already at the start");
        b.step(true);
        b.step(true);
        assert_eq!((b.line, b.col), (0, 1), "and it stops on the last character");
    }

    /// Word motions are the ones people use to get anywhere, so they have to agree with the
    /// rules already in everybody's hands: punctuation is its own word, and a line break is a
    /// separator like any other whitespace.
    #[test]
    fn word_motions_treat_punctuation_as_its_own_word_and_cross_lines() {
        let mut b = Buffer::new("let x = 1;\nnext line");
        b.goto(0, 0);
        b.word_forward();
        assert_eq!((b.line, b.col), (0, 4), "x");
        b.word_forward();
        assert_eq!((b.line, b.col), (0, 6), "=");
        b.word_forward();
        assert_eq!((b.line, b.col), (0, 8), "1");
        b.word_forward();
        assert_eq!((b.line, b.col), (0, 9), ";");
        b.word_forward();
        assert_eq!((b.line, b.col), (1, 0), "over the break to the next line");

        b.word_back();
        assert_eq!((b.line, b.col), (0, 9), "and back again");

        b.goto(1, 0);
        b.word_end();
        assert_eq!((b.line, b.col), (1, 3), "the end of `next`");
    }

    #[test]
    fn word_motions_stop_at_the_ends_of_the_buffer() {
        let mut b = Buffer::new("one");
        b.word_back();
        assert_eq!((b.line, b.col), (0, 0), "nothing before the first word");
        b.word_forward();
        assert_eq!((b.line, b.col), (0, 2), "and nowhere past the last");
        b.word_end();
        assert_eq!((b.line, b.col), (0, 2));
    }

    #[test]
    fn paragraph_motion_lands_on_the_blank_lines() {
        let mut b = Buffer::new("a\nb\n\nc\nd\n\ne");
        b.paragraph(true);
        assert_eq!(b.line, 2);
        b.paragraph(true);
        assert_eq!(b.line, 5);
        b.paragraph(true);
        assert_eq!(b.line, 6, "the end of the buffer, when there is no next blank line");
        b.paragraph(false);
        assert_eq!(b.line, 5);
    }

    /// `x` is the one people hold down. One undo per character would make taking it back a
    /// chore, and joining a line onto the one above at the end would be a surprise.
    #[test]
    fn x_deletes_within_the_line_and_a_run_of_them_undoes_at_once() {
        let mut b = Buffer::new("abcd\nefgh");
        assert!(b.delete_char());
        assert!(b.delete_char());
        assert_eq!(b.text(), "cd\nefgh");

        b.end();
        b.clamp();
        assert!(b.delete_char());
        assert_eq!(b.text(), "c\nefgh");
        assert_eq!(b.col, 0, "the cursor followed the end of the line back");
        assert!(b.delete_char());
        assert!(!b.delete_char(), "an empty line has nothing to delete, and does not join");
        assert_eq!(b.text(), "\nefgh");

        b.undo();
        assert_eq!(b.text(), "cd\nefgh", "the run after the cursor moved, back at once");
        b.undo();
        assert_eq!(b.text(), "abcd\nefgh", "then the run before it");
    }

    #[test]
    fn line_edits_do_what_their_keys_say() {
        let mut b = Buffer::new("one\ntwo\nthree");
        b.goto(1, 0);
        assert_eq!(b.delete_line(), "two");
        assert_eq!(b.text(), "one\nthree");
        assert_eq!((b.line, b.col), (1, 0), "and the cursor stays on the row it emptied");

        b.put_line("two", false);
        assert_eq!(b.text(), "one\ntwo\nthree", "put back above");
        b.put_line("mid", true);
        assert_eq!(b.text(), "one\ntwo\nmid\nthree");

        b.clear_line();
        assert_eq!(b.text(), "one\ntwo\n\nthree", "the line stays, its contents do not");

        // Deleting the only line leaves a buffer that still has somewhere to type.
        let mut b = Buffer::new("only");
        b.delete_line();
        assert_eq!(b.text(), "");
        assert_eq!((b.line, b.col), (0, 0));
    }

    #[test]
    fn join_puts_one_space_where_the_break_was() {
        let mut b = Buffer::new("one\n   two\nthree");
        b.join();
        assert_eq!(b.text(), "one two\nthree", "and eats the indentation");
        assert_eq!(b.col, 3, "the cursor sits where the join happened");

        let mut b = Buffer::new("only");
        b.join();
        assert_eq!(b.text(), "only", "nothing below to pull up");

        let mut b = Buffer::new("text\n\nafter");
        b.join();
        assert_eq!(b.text(), "text\nafter", "a blank line joins as nothing, not as a space");
    }

    /// Searching a note for `horde` should find the title too. Searching for `Horde` means
    /// you typed the capital on purpose.
    #[test]
    fn search_wraps_and_is_case_smart() {
        let mut b = Buffer::new("Horde is a thing\nthe horde grows\nHORDE");
        assert!(b.search("horde", true));
        assert_eq!((b.line, b.col), (1, 4), "forward from the top finds the next one");
        assert!(b.search("horde", true));
        assert_eq!((b.line, b.col), (2, 0));
        assert!(b.search("horde", true));
        assert_eq!((b.line, b.col), (0, 0), "and round again");

        b.goto(2, 0);
        assert!(b.search("Horde", true));
        assert_eq!((b.line, b.col), (0, 0), "a capital means exactly that spelling");

        assert!(!b.search("nowhere", true), "and it says when there is nothing");
        assert_eq!((b.line, b.col), (0, 0), "leaving the cursor alone");
    }

    #[test]
    fn search_backwards_wraps_too() {
        let mut b = Buffer::new("a x\nb x\nc x");
        b.goto(1, 2);
        assert!(b.search("x", false));
        assert_eq!((b.line, b.col), (0, 2));
        assert!(b.search("x", false));
        assert_eq!((b.line, b.col), (2, 2), "past the top and back to the bottom");
    }

    #[test]
    fn a_new_buffer_is_clean_until_something_changes_it() {
        let mut b = Buffer::new("text");
        b.right();
        b.down();
        assert!(!b.dirty, "moving around is not editing");
        b.insert('!');
        assert!(b.dirty);
        b.saved();
        assert!(!b.dirty);
    }
}
