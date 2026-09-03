//! Pictures, in a terminal that only has letters.
//!
//! Two ways, and the first one is the floor the second stands on.
//!
//! **Half blocks.** `▀` drawn with a foreground and a background colour is two pixels in one
//! cell: the top half is the foreground, the bottom half the background. That gives a grid of
//! `width x height*2` full-colour pixels, and — because a terminal cell is about twice as tall
//! as it is wide — those pixels come out square. It works in every terminal ever made, needs
//! no capability negotiation, and produces nothing but styled cells, which is exactly what
//! horde's render channel already carries. A remote client gets images for free.
//!
//! **The kitty graphics protocol** does far better where it exists, and is layered on top
//! rather than instead: see [`super::kitty`]. The half-block path is what runs when it does
//! not, and what every test asserts against, because it is the one that always happens.
//!
//! Decoding is cached. A note with a screenshot in it is redrawn twenty times a second while
//! the graph beside it breathes, and decoding a two-megabyte PNG per frame would be the most
//! expensive thing horde does by two orders of magnitude.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::proto::Rgb;
use crate::theme::Theme;

/// The largest file worth decoding.
///
/// A note's illustration is not sixteen megabytes. Past this the thing is a mistake — a video
/// frame dump, a raw scan — and decoding it would stall the frame it was asked for.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Decoded images kept. Small, because each one is a screenful of cells and the reader only
/// ever shows a handful at once.
const CACHE: usize = 8;

/// The upper half of a cell, which is how one cell becomes two pixels.
const HALF: &str = "▀";

/// A decoded image, sized to a particular box.
type Key = (PathBuf, u64, u16, u16);

thread_local! {
    /// Keyed by path, modification time and the box asked for.
    ///
    /// Thread-local rather than a field on `App` because it is a cache in the strictest
    /// sense: it changes nothing that anyone can observe except how long a frame takes, and
    /// threading a `&mut` for it through the markdown renderer would put it in three
    /// signatures that have no other reason to know images exist.
    static DECODED: RefCell<Vec<(Key, Vec<Line<'static>>)>> = const { RefCell::new(Vec::new()) };
}

/// Where an image referred to by a note actually is.
///
/// Obsidian resolves an embed against the whole vault; horde looks in the two places an
/// attachment is ever put — beside the note, and in the vault's attachment folder — and then
/// gives up rather than walking a thousand-note tree on every frame.
pub fn locate(target: &str, note_dir: Option<&Path>, vault: Option<&Path>) -> Option<PathBuf> {
    let raw = Path::new(target);
    if raw.is_absolute() && raw.is_file() {
        return Some(raw.to_path_buf());
    }
    let mut tries: Vec<PathBuf> = Vec::new();
    if let Some(dir) = note_dir {
        tries.push(dir.join(raw));
    }
    if let Some(root) = vault {
        tries.push(root.join(raw));
        tries.push(root.join(ATTACHMENTS).join(raw));
        // The bare filename too, since `![[shot.png]]` names a file rather than a path.
        if let Some(name) = raw.file_name() {
            tries.push(root.join(ATTACHMENTS).join(name));
        }
    }
    tries.into_iter().find(|p| p.is_file())
}

/// Where a pasted image is put, inside the vault.
pub const ATTACHMENTS: &str = "attachments";

/// Whether a filename is one of the formats this build decodes.
pub fn is_image(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif")
}

/// Render an image into at most `cols` x `rows` cells.
///
/// Fewer rows than asked for when the picture is wider than it is tall, which is the normal
/// case for a screenshot: the box is a limit, not a size to fill.
pub fn cells(
    path: &Path,
    cols: u16,
    rows: u16,
    theme: &Theme,
) -> Option<Vec<Line<'static>>> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_BYTES {
        return None;
    }
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key: Key = (path.to_path_buf(), stamp, cols, rows);

    if let Some(hit) = DECODED.with(|c| {
        c.borrow().iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
    }) {
        return Some(hit);
    }

    let lines = decode(path, cols, rows, theme)?;
    DECODED.with(|c| {
        let mut c = c.borrow_mut();
        c.push((key, lines.clone()));
        if c.len() > CACHE {
            c.remove(0);
        }
    });
    Some(lines)
}

fn decode(path: &Path, cols: u16, rows: u16, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let img = image::open(path).ok()?;
    // Two pixels to a cell vertically, one horizontally — which is what makes them square,
    // since a cell is about twice as tall as it is wide.
    let img = img.resize(
        cols as u32,
        rows as u32 * 2,
        image::imageops::FilterType::Triangle,
    );
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }

    let bg = theme.ui.bg;
    let at = |x: u32, y: u32| -> Rgb {
        if y >= h {
            return bg;
        }
        let p = rgba.get_pixel(x, y).0;
        // Composited onto the page rather than guessed at: a transparent PNG is the usual
        // case for a diagram, and drawing its background as black puts a slab behind it.
        let a = p[3] as f32 / 255.0;
        let mix = |c: u8, b: u8| (c as f32 * a + b as f32 * (1.0 - a)).round() as u8;
        Rgb { r: mix(p[0], bg.r), g: mix(p[1], bg.g), b: mix(p[2], bg.b) }
    };

    let mut out = Vec::new();
    for row in 0..h.div_ceil(2) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut style: Option<(Rgb, Rgb)> = None;
        for x in 0..w {
            let pair = (at(x, row * 2), at(x, row * 2 + 1));
            // Run-length by colour pair. A screenshot is mostly flat regions, so this is the
            // difference between a line of two hundred spans and a line of twenty.
            if style != Some(pair) && !run.is_empty() {
                spans.push(styled(&run, style.unwrap()));
                run.clear();
            }
            style = Some(pair);
            run.push_str(HALF);
        }
        if let Some(s) = style {
            spans.push(styled(&run, s));
        }
        out.push(Line::from(spans));
    }
    Some(out)
}

fn styled(text: &str, (top, bottom): (Rgb, Rgb)) -> Span<'static> {
    Span::styled(
        text.to_string(),
        Style::default()
            .fg(super::ui::color(top))
            .bg(super::ui::color(bottom)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(name: &str, w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("horde-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut buf = image::RgbaImage::new(w, h);
        for (x, y, p) in buf.enumerate_pixels_mut() {
            *p = image::Rgba(f(x, y));
        }
        buf.save(&path).unwrap();
        path
    }

    /// One cell is two pixels, one above the other. Getting that backwards draws every image
    /// upside down in alternating stripes, which is the kind of thing that looks like a
    /// broken decoder rather than a swapped constant.
    #[test]
    fn a_cell_carries_the_pixel_above_it_and_the_one_below() {
        // Red on top, blue underneath.
        let path = write_png("split.png", 4, 2, |_, y| {
            if y == 0 { [255, 0, 0, 255] } else { [0, 0, 255, 255] }
        });
        let theme = Theme::horde();
        let lines = cells(&path, 4, 4, &theme).expect("decoded");
        assert_eq!(lines.len(), 1, "two pixel rows are one cell row");

        let span = &lines[0].spans[0];
        assert!(span.content.starts_with('▀'), "{:?}", span.content);
        assert_eq!(span.style.fg, Some(super::super::ui::color(Rgb { r: 255, g: 0, b: 0 })));
        assert_eq!(span.style.bg, Some(super::super::ui::color(Rgb { r: 0, g: 0, b: 255 })));
        let _ = std::fs::remove_file(&path);
    }

    /// The box is a limit, not a size to fill. A wide screenshot in a tall panel must come
    /// back short rather than stretched, or every picture is the shape of its container.
    #[test]
    fn a_wide_image_comes_back_short_rather_than_stretched() {
        let path = write_png("wide.png", 100, 20, |_, _| [10, 20, 30, 255]);
        let theme = Theme::horde();
        let lines = cells(&path, 50, 40, &theme).expect("decoded");
        assert!(lines.len() < 40, "it used {} of 40 rows", lines.len());
        // 100x20 into 50 columns is 50x10 pixels, which is five cell rows.
        assert_eq!(lines.len(), 5);
        let width: usize = lines[0].spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(width, 50);
        let _ = std::fs::remove_file(&path);
    }

    /// A transparent diagram is the common case, and drawing its background as black puts a
    /// slab behind it on every theme.
    #[test]
    fn transparency_is_composited_onto_the_page_not_onto_black() {
        let path = write_png("clear.png", 2, 2, |_, _| [255, 255, 255, 0]);
        let theme = Theme::horde();
        let lines = cells(&path, 2, 2, &theme).expect("decoded");
        let fg = lines[0].spans[0].style.fg.unwrap();
        assert_eq!(fg, super::super::ui::color(theme.ui.bg), "fully clear reads as the page");
        let _ = std::fs::remove_file(&path);
    }

    /// Decoding is the expensive thing here and the reader redraws constantly. A second look
    /// at the same picture must not decode it again.
    #[test]
    fn the_same_image_is_only_decoded_once() {
        let path = write_png("cached.png", 8, 8, |x, y| [x as u8, y as u8, 0, 255]);
        let theme = Theme::horde();
        let first = std::time::Instant::now();
        let a = cells(&path, 8, 8, &theme).expect("decoded");
        let cold = first.elapsed();
        let second = std::time::Instant::now();
        let b = cells(&path, 8, 8, &theme).expect("decoded");
        let warm = second.elapsed();

        assert_eq!(a.len(), b.len());
        assert!(warm < cold, "warm {warm:?} was not faster than cold {cold:?}");
        let _ = std::fs::remove_file(&path);
    }

    /// Anything that is not a picture, or is too big to be one on purpose, comes back as
    /// nothing rather than as a broken frame.
    #[test]
    fn what_cannot_be_drawn_says_so_rather_than_failing() {
        let theme = Theme::horde();
        assert!(cells(Path::new("/nowhere/at/all.png"), 10, 10, &theme).is_none());

        let dir = std::env::temp_dir().join(format!("horde-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("notreally.png");
        std::fs::write(&text, "this is not a png").unwrap();
        assert!(cells(&text, 10, 10, &theme).is_none(), "a lying extension is not a picture");
        let _ = std::fs::remove_file(&text);
    }

    /// The two places an attachment is ever put, and no more — walking a thousand-note vault
    /// on every frame to find a screenshot is not a search, it is a stall.
    #[test]
    fn an_embed_is_found_beside_the_note_or_in_the_attachment_folder() {
        let root = std::env::temp_dir().join(format!("horde-loc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(ATTACHMENTS)).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join(ATTACHMENTS).join("shot.png"), "x").unwrap();
        std::fs::write(root.join("sub").join("beside.png"), "x").unwrap();

        let dir = root.join("sub");
        assert_eq!(
            locate("beside.png", Some(&dir), Some(&root)),
            Some(dir.join("beside.png")),
            "beside the note wins"
        );
        assert_eq!(
            locate("shot.png", Some(&dir), Some(&root)),
            Some(root.join(ATTACHMENTS).join("shot.png")),
            "and the attachment folder is the other place to look"
        );
        assert_eq!(locate("nothing.png", Some(&dir), Some(&root)), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_formats_this_build_decodes_are_offered() {
        assert!(is_image("a.png") && is_image("B.JPG") && is_image("c.gif"));
        assert!(!is_image("a.svg"), "vector is not something a decoder here handles");
        assert!(!is_image("notes.md") && !is_image("noextension"));
    }
}
