//! Real images, in the terminals that can draw them.
//!
//! The kitty graphics protocol hands a terminal actual pixels rather than coloured cells:
//! `\x1b_G<keys>;<base64 payload>\x1b\\`, and the picture appears at the cursor, scaled into a
//! box measured in cells. Ghostty, kitty, Konsole and WezTerm all speak it. Where it is not
//! spoken, [`super::image`]'s half blocks still are, and that is why they were built first.
//!
//! **This does not go through ratatui, and cannot.** ratatui owns a grid of cells and paints
//! by diffing it; a kitty image is not in that grid at all. So the arrangement is: the
//! renderer leaves the image's rows *blank*, records where they were, and after the frame is
//! flushed the placements are made directly on the terminal. Nothing ratatui knows about is
//! written where an image is, so nothing ratatui does can half-erase one.
//!
//! Placements are only redone when they change. A note being read does not move, but the
//! graph beside it redraws twenty times a second, and re-transmitting a photograph at that
//! rate would be worse than not having images at all.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::client::graph::CELL_ASPECT;

/// Base64 characters per escape. The protocol's own limit is 4096.
const CHUNK: usize = 4096;

/// The widest an image is sent at.
///
/// The terminal scales into the cell box either way, so sending a 4K wallpaper at full size
/// buys nothing and costs a megabyte of base64 down a pipe on every placement.
const MAX_WIDTH: u32 = 1600;

/// Where one image is drawn, in cells on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub path: PathBuf,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

/// Whether this terminal draws images.
///
/// By name rather than by asking. The protocol has a query — send a one-pixel image and read
/// the reply — but that means reading from the terminal during startup, racing every other
/// thing that writes there, and having a timeout policy for terminals that never answer.
/// The name is right for every terminal that implements this, and `HORDE_IMAGES` settles any
/// argument in either direction.
pub fn supported() -> bool {
    match std::env::var("HORDE_IMAGES").as_deref() {
        Ok("1") | Ok("true") | Ok("kitty") => return true,
        Ok("0") | Ok("false") | Ok("off") => return false,
        _ => {}
    }
    let term = std::env::var("TERM").unwrap_or_default().to_ascii_lowercase();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
    term.contains("kitty")
        || term.contains("ghostty")
        || matches!(program.as_str(), "ghostty" | "wezterm" | "kitty")
        || std::env::var("KITTY_WINDOW_ID").is_ok()
}

/// How many cells tall an image `cols` wide should be, to keep its shape.
///
/// A cell is about twice as tall as it is wide, so a square picture is half as many rows as
/// it is columns. Without this every image is drawn stretched to twice its height.
pub fn rows_for(pixel_w: u32, pixel_h: u32, cols: u16, limit: u16) -> u16 {
    if pixel_w == 0 || pixel_h == 0 || cols == 0 {
        return 0;
    }
    let rows = (cols as f64 * pixel_h as f64 / pixel_w as f64 * CELL_ASPECT).round();
    (rows.max(1.0) as u16).min(limit).max(1)
}

/// The cell box an image should occupy, at most `max_cols` by `max_rows`, keeping its shape.
///
/// Both dimensions give way rather than only one: capping the height alone would squash a
/// tall picture into the width it was offered, which is the same distortion the half-block
/// path was fixed for.
pub fn fit(pixel_w: u32, pixel_h: u32, max_cols: u16, max_rows: u16) -> (u16, u16) {
    if pixel_w == 0 || pixel_h == 0 || max_cols == 0 || max_rows == 0 {
        return (0, 0);
    }
    let rows = rows_for(pixel_w, pixel_h, max_cols, u16::MAX);
    if rows <= max_rows {
        return (max_cols, rows.max(1));
    }
    let cols = (max_cols as f64 * max_rows as f64 / rows as f64).round().max(1.0) as u16;
    (cols, max_rows)
}

/// The bytes to send for one placement, cursor move included.
///
/// Separate from writing them so the whole protocol is testable without a terminal — which
/// matters more here than usual, because a malformed escape does not fail, it prints.
pub fn place(id: u32, png: &[u8], p: &Place) -> Vec<u8> {
    let payload = base64(png);
    let mut out = Vec::with_capacity(payload.len() + 256);
    // The image lands at the cursor, so the cursor goes first.
    out.extend_from_slice(format!("\x1b[{};{}H", p.y + 1, p.x + 1).as_bytes());

    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(CHUNK).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        if i == 0 {
            // `a=T` transmits and displays in one go. `f=100` means the payload is a PNG and
            // the terminal decodes it. `q=2` asks for silence: horde does not read replies,
            // and an unread one would land in the keyboard.
            out.extend_from_slice(
                format!(
                    "\x1b_Ga=T,f=100,i={id},c={},r={},q=2,m={more};",
                    p.cols, p.rows
                )
                .as_bytes(),
            );
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Show an image the terminal already has, at a new place.
///
/// The reason images are transmitted with an id rather than re-sent: a 4K photograph is two
/// hundred kilobytes of base64, and scrolling changes a placement on every line. Sending the
/// picture again each time would make scrolling past one crawl.
pub fn replace(id: u32, p: &Place) -> Vec<u8> {
    format!(
        "\x1b[{};{}H\x1b_Ga=p,i={id},c={},r={},q=2\x1b\\",
        p.y + 1,
        p.x + 1,
        p.cols,
        p.rows
    )
    .into_bytes()
}

/// Take down every placement, leaving the pictures themselves in the terminal's memory.
pub fn unplace() -> Vec<u8> {
    b"\x1b_Ga=d,d=a\x1b\\".to_vec()
}

/// Remove every image, data and all.
pub fn clear() -> Vec<u8> {
    b"\x1b_Ga=d,d=A\x1b\\".to_vec()
}

/// Re-encode an image small enough to send, and say how big it was.
///
/// Cached alongside the half-block cache for the same reason: a 4K wallpaper decoded and
/// re-encoded per frame would cost more than everything else horde does put together.
pub fn encode(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::open(path).ok()?;
    let (w, h) = (img.width(), img.height());
    let img = if w > MAX_WIDTH {
        img.resize(MAX_WIDTH, u32::MAX, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some((png.into_inner(), w, h))
}

/// Standard base64, which is what the protocol asks for.
fn base64(bytes: &[u8]) -> String {
    const SET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b = [group[0], *group.get(1).unwrap_or(&0), *group.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(SET[(n >> 18) as usize & 63] as char);
        out.push(SET[(n >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 { SET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if group.len() > 2 { SET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Images this terminal has been sent, and where they are showing.
#[derive(Default)]
pub struct Placed {
    shown: Vec<Place>,
    /// Path, the id it was given, and the encoded bytes — so a picture is transmitted once
    /// and thereafter only pointed at.
    sent: Vec<(PathBuf, u32, Vec<u8>)>,
    next_id: u32,
}

impl Placed {
    /// Make the terminal show exactly `want`, doing nothing if it already does.
    ///
    /// The "doing nothing" is the point. The graph redraws twenty times a second and a note
    /// being read does not move, so re-transmitting on every frame would turn a photograph
    /// into the most expensive thing on the screen.
    pub fn sync(&mut self, out: &mut impl Write, want: &[Place]) -> std::io::Result<()> {
        if self.shown == want {
            return Ok(());
        }
        // Placements go, the pictures stay. Scrolling a note moves an image every line, and
        // re-sending two hundred kilobytes of base64 each time is the difference between
        // scrolling and waiting.
        out.write_all(&unplace())?;
        for p in want {
            match self.known(&p.path) {
                Some((id, _)) => out.write_all(&replace(id, p))?,
                None => {
                    let Some((png, _, _)) = encode(&p.path) else { continue };
                    self.next_id += 1;
                    let id = self.next_id;
                    out.write_all(&place(id, &png, p))?;
                    self.sent.push((p.path.clone(), id, png));
                    // A handful, because each is a whole encoded picture.
                    if self.sent.len() > 4 {
                        self.sent.remove(0);
                    }
                }
            }
        }
        out.flush()?;
        self.shown = want.to_vec();
        Ok(())
    }

    fn known(&self, path: &Path) -> Option<(u32, usize)> {
        self.sent.iter().find(|(p, _, _)| p == path).map(|(_, id, png)| (*id, png.len()))
    }

    /// Forget what is shown, so the next sync redraws it.
    ///
    /// For anything that wipes the screen underneath us — a resize, leaving the alternate
    /// screen — where the terminal has dropped the images but this has not noticed.
    pub fn forget(&mut self) {
        self.shown.clear();
        // The pictures go too. After a resize the terminal's idea of what it holds is not
        // one this side can check, and pointing at an id it may have dropped draws nothing.
        self.sent.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xef, 0xbe]), "/+++");
    }

    /// A cell is twice as tall as it is wide, so a square picture takes half as many rows as
    /// columns. Without it every image is drawn stretched to twice its height.
    #[test]
    fn an_image_keeps_its_shape_in_a_grid_of_tall_cells() {
        assert_eq!(rows_for(100, 100, 40, 99), 20, "square");
        assert_eq!(rows_for(200, 100, 40, 99), 10, "twice as wide, half as tall");
        assert_eq!(rows_for(100, 200, 40, 99), 40, "and the other way");
        assert_eq!(rows_for(3840, 2400, 80, 99), 25, "a 4K wallpaper at 80 columns");
        assert_eq!(rows_for(100, 100, 40, 8), 8, "never past the room it was given");
        assert_eq!(rows_for(0, 0, 40, 99), 0, "and nothing sized nothing");
    }

    /// A malformed escape does not fail, it prints — so the shape of one is worth pinning
    /// rather than finding out by watching gibberish scroll past.
    /// Both dimensions give way. Capping only the height squashes a tall picture into the
    /// width it was offered, which is the distortion the half-block path was already fixed for.
    #[test]
    fn a_tall_picture_narrows_rather_than_squashing() {
        assert_eq!(fit(100, 100, 40, 99), (40, 20), "it fits, so it uses the width");
        let (c, r) = fit(100, 400, 40, 20);
        assert_eq!(r, 20, "height-limited");
        assert!(c < 40, "so the width came in too: {c}");
        // Still the right shape: 1:4 in pixels is 1:2 in cells.
        assert!(((r as f64 / c as f64) - 2.0).abs() < 0.35, "{c}x{r}");
        assert_eq!(fit(0, 0, 40, 20), (0, 0));
    }

    #[test]
    fn a_placement_moves_the_cursor_then_transmits_and_displays() {
        let png = vec![0u8; 10];
        let p = Place { path: "x.png".into(), x: 4, y: 9, cols: 20, rows: 10 };
        let bytes = String::from_utf8_lossy(&place(7, &png, &p)).to_string();

        assert!(bytes.starts_with("\x1b[10;5H"), "cursor first, one-based: {bytes:?}");
        assert!(bytes.contains("\x1b_Ga=T,f=100,i=7,c=20,r=10,q=2,m=0;"), "{bytes:?}");
        assert!(bytes.ends_with("\x1b\\"), "terminated: {bytes:?}");
        assert!(bytes.contains("q=2"), "silence, or the reply lands in the keyboard");
    }

    /// Anything past four kilobytes has to arrive in pieces, and every piece but the last has
    /// to say so — a chunk that forgets leaves the terminal waiting for the rest forever.
    #[test]
    fn a_large_image_is_chunked_and_every_chunk_but_the_last_says_more_is_coming() {
        let png = vec![7u8; CHUNK * 3];
        let p = Place { path: "big.png".into(), x: 0, y: 0, cols: 10, rows: 5 };
        let bytes = String::from_utf8_lossy(&place(1, &png, &p)).to_string();

        let opens = bytes.matches("\x1b_G").count();
        assert!(opens >= 4, "chunked into {opens} escapes");
        assert_eq!(bytes.matches("\x1b\\").count(), opens, "each one terminated");
        assert_eq!(bytes.matches("m=0;").count(), 1, "exactly one final chunk");
        assert!(bytes.matches("m=1;").count() >= 3, "and the rest say more is coming");
    }

    /// The graph redraws twenty times a second. Re-sending a photograph each time would make
    /// it the most expensive thing on screen by a wide margin.
    #[test]
    fn an_unchanged_placement_is_not_sent_again() {
        let dir = std::env::temp_dir().join(format!("horde-kitty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.png");
        image::RgbaImage::new(4, 4).save(&path).unwrap();

        let want = vec![Place { path: path.clone(), x: 0, y: 0, cols: 4, rows: 2 }];
        let mut placed = Placed::default();
        let mut buf: Vec<u8> = Vec::new();
        placed.sync(&mut buf, &want).unwrap();
        assert!(!buf.is_empty(), "the first one goes");

        let mut again: Vec<u8> = Vec::new();
        placed.sync(&mut again, &want).unwrap();
        assert!(again.is_empty(), "the second does not");

        // Moving it is a change, and a change is sent — but the picture is not sent again.
        // Scrolling a note moves an image every line, and a 4K photograph is two hundred
        // kilobytes of base64.
        let moved = vec![Place { path, x: 1, y: 0, cols: 4, rows: 2 }];
        let mut third: Vec<u8> = Vec::new();
        placed.sync(&mut third, &moved).unwrap();
        let third = String::from_utf8_lossy(&third).to_string();
        assert!(third.contains("a=p,i=1"), "pointed at, not re-sent: {third:?}");
        assert!(!third.contains("a=T"), "and no transmission: {third:?}");
        assert!(third.len() < buf.len() / 2, "so it is much smaller than the first");

        // And nothing on screen means the clear still goes, or the last one stays up.
        let mut fourth: Vec<u8> = Vec::new();
        placed.sync(&mut fourth, &[]).unwrap();
        assert!(String::from_utf8_lossy(&fourth).contains("a=d,d=a"), "placements are taken down");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one thing that must never be guessed wrong in the direction of "yes": drawing
    /// kitty escapes at a terminal that cannot read them prints them.
    #[test]
    fn the_override_settles_it_in_both_directions() {
        // SAFETY: single-threaded test, and the variable is read only by `supported`.
        unsafe {
            std::env::set_var("HORDE_IMAGES", "0");
            assert!(!supported(), "off means off, whatever the terminal is");
            std::env::set_var("HORDE_IMAGES", "1");
            assert!(supported(), "and on means on");
            std::env::remove_var("HORDE_IMAGES");
        }
    }
}
