//! How wide the *host* terminal draws a glyph, as opposed to how wide the tables say it is.
//!
//! horde lays out a pane in cells and trusts `unicode-width` to say how many cells a character
//! takes. For almost everything that is right. For the private-use ranges it is a guess, and
//! the host terminal is making its own: Nerd Font icons live in the PUA, where Unicode assigns
//! no width at all, so every terminal picks. Ghostty draws many of them two cells wide;
//! `unicode-width` reports one.
//!
//! One cell of disagreement per glyph is enough to break a pane. horde budgets a row at the
//! pane's width, the host paints it wider, and everything after the glyph slides right —
//! including the pane border, which the row then spills across. It shows up only on rows
//! carrying those glyphs, which is what makes it look intermittent: a shell prompt with two
//! Nerd Font icons bleeds over the border, and the line under it sits perfectly inside.
//!
//! Rather than keep a table of what each terminal believes — which is a guess about someone
//! else's guess, wrong the moment either moves — horde asks. Print the glyph, ask where the
//! cursor ended up, and that difference is the answer, from the only authority that matters.

use std::io::Write;
use std::ops::RangeInclusive;
use std::sync::OnceLock;

use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use unicode_width::UnicodeWidthChar;

/// A block horde and the host can legitimately disagree about, and glyphs to settle it with.
struct Ambiguous {
    /// For the log line, so a surprising result can be traced to a block.
    name: &'static str,
    range: RangeInclusive<u32>,
    /// Two samples rather than one. Nerd Fonts are a patchwork of donated icon sets and a
    /// single glyph proves less about its neighbours than it looks; a block is only overridden
    /// when both samples agree, so a mixed block falls back to the tables instead of applying
    /// one icon's answer to thousands of others.
    samples: [char; 2],
}

/// The blocks worth asking about.
///
/// Deliberately only the private-use areas. Unicode's *ambiguous* width class is a real problem
/// too, but it is scattered across the BMP rather than blocked, and it is entangled with the
/// host's locale rather than its font — a different question that this cannot answer.
const AMBIGUOUS: &[Ambiguous] = &[
    // Powerline separators and friends. Drawn to occupy exactly one cell, but they share the
    // BMP private-use area with icons that are not, so they get asked about like everything.
    Ambiguous { name: "powerline", range: 0xE000..=0xE0FF, samples: ['\u{e0b0}', '\u{e0b2}'] },
    // The rest of the BMP private-use area: the older Nerd Font sets.
    Ambiguous { name: "nerd-bmp", range: 0xE100..=0xF8FF, samples: ['\u{f07b}', '\u{f121}'] },
    // Supplementary private-use area A, where the Material Design icons live. This is the block
    // a Powerlevel10k or Starship prompt draws from, and the one Ghostty widens.
    Ambiguous { name: "nerd-spua", range: 0xF0000..=0xFFFFD, samples: ['\u{f0035}', '\u{f0219}'] },
];

/// What the host said, for the blocks it gave a straight answer about.
static MEASURED: OnceLock<Vec<(RangeInclusive<u32>, usize)>> = OnceLock::new();

/// Columns `ch` occupies on this terminal.
///
/// The measured answer where there is one, the tables everywhere else. Combining marks stay
/// zero-width whatever happens: they are not in the blocks asked about, and a mark that
/// consumed a cell of its own would corrupt every row carrying an accent.
pub fn width(ch: char) -> usize {
    let cp = ch as u32;
    if let Some(measured) = MEASURED.get() {
        for (range, w) in measured {
            if range.contains(&cp) {
                return *w;
            }
        }
    }
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Ask the terminal how wide it draws each ambiguous block, and remember the answers.
///
/// Call once, after raw mode and the alternate screen are on and before anything else is
/// reading stdin — the reply arrives as ordinary input and whoever reads first gets it.
///
/// Failure is not an error worth stopping for. A terminal that does not answer a cursor query
/// leaves horde exactly where it was: on the Unicode tables, which is what every version until
/// now used for everything.
pub fn measure(out: &mut impl Write) -> Vec<String> {
    let mut notes = Vec::new();
    let mut table: Vec<(RangeInclusive<u32>, usize)> = Vec::new();

    for block in AMBIGUOUS {
        let mut widths = [0usize; 2];
        for (i, ch) in block.samples.iter().enumerate() {
            match probe(out, *ch) {
                Some(w) => widths[i] = w,
                None => {
                    // The terminal is not answering. It will not start answering for the next
                    // glyph either, and each attempt costs a two-second timeout, so stop.
                    notes.push(
                        "terminal did not answer a cursor query; \
                         glyph widths fall back to the Unicode tables"
                            .to_string(),
                    );
                    let _ = clear_probe_row(out);
                    return notes;
                }
            }
        }
        let _ = clear_probe_row(out);

        // Only a block whose samples agree, and agree on something sane, is worth overriding.
        // A zero would erase text; anything above two is not a width, it is a misparse.
        if widths[0] == widths[1] && (1..=2).contains(&widths[0]) {
            let table_says = UnicodeWidthChar::width(block.samples[0]).unwrap_or(0);
            if widths[0] != table_says {
                notes.push(format!(
                    "{}: this terminal draws {} cells where the tables say {table_says}",
                    block.name, widths[0]
                ));
            }
            table.push((block.range.clone(), widths[0]));
        }
    }

    let _ = MEASURED.set(table);
    notes
}

/// Print one glyph at a known column and report where the cursor ended up.
fn probe(out: &mut impl Write, ch: char) -> Option<usize> {
    // Column 0 of the last row: whatever is drawn here is overwritten by the first frame, and
    // the bottom row is the one least likely to be mid-scroll on a terminal that ignores the
    // alternate screen.
    let row = crossterm::terminal::size().map(|(_, h)| h.saturating_sub(1)).unwrap_or(0);
    execute!(out, MoveTo(0, row)).ok()?;
    write!(out, "{ch}").ok()?;
    out.flush().ok()?;
    let (col, _) = crossterm::cursor::position().ok()?;
    Some(col as usize)
}

fn clear_probe_row(out: &mut impl Write) -> std::io::Result<()> {
    let row = crossterm::terminal::size().map(|(_, h)| h.saturating_sub(1)).unwrap_or(0);
    execute!(out, MoveTo(0, row), Clear(ClearType::CurrentLine))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a measurement — every test, and every terminal that does not answer — this has
    /// to behave exactly as the bare table lookup it replaced.
    #[test]
    fn an_unmeasured_terminal_falls_back_to_the_tables() {
        assert_eq!(width('a'), 1);
        assert_eq!(width('日'), 2);
        // A combining acute belongs to the character before it and must never take a cell.
        assert_eq!(width('\u{0301}'), 0);
        assert_eq!(width('\u{f0035}'), UnicodeWidthChar::width('\u{f0035}').unwrap_or(0));
    }

    /// The blocks must not overlap, or which answer a glyph gets would depend on the order
    /// they happen to sit in the list.
    #[test]
    fn the_ambiguous_blocks_do_not_overlap() {
        for (i, a) in AMBIGUOUS.iter().enumerate() {
            for b in &AMBIGUOUS[i + 1..] {
                assert!(
                    a.range.end() < b.range.start() || b.range.end() < a.range.start(),
                    "{} and {} overlap",
                    a.name,
                    b.name
                );
            }
        }
    }

    /// Every sample has to sit inside the block it is meant to settle, or the measurement is
    /// answering a question about a different block.
    #[test]
    fn every_sample_lies_inside_the_block_it_speaks_for() {
        for block in AMBIGUOUS {
            for ch in block.samples {
                assert!(
                    block.range.contains(&(ch as u32)),
                    "{}: U+{:04X} is outside {:04X}..={:04X}",
                    block.name,
                    ch as u32,
                    block.range.start(),
                    block.range.end()
                );
            }
            assert_ne!(block.samples[0], block.samples[1], "{}: one glyph asked twice", block.name);
        }
    }
}
