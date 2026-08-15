//! A plain text buffer for writing notes.
//!
//! **Modeless, deliberately.** The thing this is rebuilding is a notes app, and in a notes
//! app typing types. A vim grammar here would mean every note you open is a small quiz about
//! which mode you are in before a keystroke means what it looks like it means — which is a
//! fine trade in a code editor you live in, and a bad one for catching a thought.
//!
//! Pure: no drawing, no daemon. It holds lines and a cursor and answers questions about
//! them, which is what makes it testable as text.

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
}

impl Buffer {
    pub fn new(text: &str) -> Buffer {
        let mut lines: Vec<String> = text.split('\n').map(|l| l.to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Buffer { lines, line: 0, col: 0, dirty: false, goal: 0 }
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

    pub fn insert(&mut self, c: char) {
        let b = self.byte_at(self.line, self.col);
        self.lines[self.line].insert(b, c);
        self.col += 1;
        self.goal = self.col;
        self.dirty = true;
    }

    pub fn newline(&mut self) {
        let b = self.byte_at(self.line, self.col);
        let rest = self.lines[self.line].split_off(b);
        self.lines.insert(self.line + 1, rest);
        self.line += 1;
        self.col = 0;
        self.goal = 0;
        self.dirty = true;
    }

    /// Delete backwards, joining lines when the cursor is at the start of one.
    pub fn backspace(&mut self) {
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
        self.dirty = true;
    }

    pub fn delete(&mut self) {
        if self.col < self.line_len(self.line) {
            let start = self.byte_at(self.line, self.col);
            let end = self.byte_at(self.line, self.col + 1);
            self.lines[self.line].replace_range(start..end, "");
            self.dirty = true;
        } else if self.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.line + 1);
            self.lines[self.line].push_str(&next);
            self.dirty = true;
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
