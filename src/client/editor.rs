//! A plain text buffer for writing notes.
//!
//! **Modeless, deliberately.** The thing this is rebuilding is a notes app, and in a notes
//! app typing types. A vim grammar here would mean every note you open is a small quiz about
//! which mode you are in before a keystroke means what it looks like it means — which is a
//! fine trade in a code editor you live in, and a bad one for catching a thought.
//!
//! Pure: no drawing, no daemon. It holds lines and a cursor and answers questions about
//! them, which is what makes it testable as text.

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
            self.dirty = true;
        self.rev += 1;
        } else if self.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.line + 1);
            self.lines[self.line].push_str(&next);
            self.dirty = true;
        self.rev += 1;
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
