//! Getting a picture out of the system clipboard.
//!
//! **A terminal cannot do this.** Bracketed paste delivers text and only text — there is no
//! escape sequence by which an image arrives at a program running in a terminal, so pressing
//! paste in the editor can never be the whole mechanism. What horde does instead is ask the
//! platform directly, which is a shell-out, which is what horde does everywhere else it needs
//! something the operating system owns.
//!
//! Read on the *client* rather than in the daemon, because the clipboard belongs to the
//! machine somebody is sitting at. A client attached over a socket from another host pastes
//! its own clipboard, not the server's, which is the only answer that is ever right.

use std::process::Command;

/// The most a pasted image may be.
///
/// Eight megabytes is a very large screenshot and a very small photograph. The cap is here
/// rather than only in the daemon so an accident is refused before it goes down a socket.
pub const MAX_PASTE: usize = 8 * 1024 * 1024;




/// The clipboard's contents as a PNG, if it holds a picture at all.
///
/// `None` covers every ordinary case — text on the clipboard, nothing on it, no helper
/// installed — and none of them is an error worth interrupting somebody for.
pub fn image() -> Option<Vec<u8>> {
    let bytes = platform_image()?;
    (!bytes.is_empty() && bytes.len() <= MAX_PASTE).then_some(bytes)
}

#[cfg(target_os = "macos")]
fn platform_image() -> Option<Vec<u8>> {
    // AppleScript can hand over the clipboard's PNG representation, but only by writing it to
    // a file — `osascript` prints AppleScript data as a hex dump, not as bytes.
    let tmp = std::env::temp_dir().join(format!("horde-paste-{}.png", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let script = format!(
        r#"try
    set png to the clipboard as «class PNGf»
    set f to open for access POSIX file "{}" with write permission
    write png to f
    close access f
end try"#,
        tmp.display()
    );
    let ok = Command::new("osascript").arg("-e").arg(&script).status().ok()?.success();
    let bytes = ok.then(|| std::fs::read(&tmp).ok()).flatten();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[cfg(not(target_os = "macos"))]
fn platform_image() -> Option<Vec<u8>> {
    // Wayland first, then X11. Both print the image to stdout, which is the sane arrangement
    // and the reason this half needs no temporary file.
    for (cmd, args) in [
        ("wl-paste", vec!["--type", "image/png", "--no-newline"]),
        ("xclip", vec!["-selection", "clipboard", "-t", "image/png", "-o"]),
    ] {
        if let Ok(out) = Command::new(cmd).args(&args).output() {
            if out.status.success() && !out.stdout.is_empty() {
                return Some(out.stdout);
            }
        }
    }
    None
}

/// A filename for something pasted into `note`, at `stamp` seconds.
///
/// Named after the note it landed in rather than given a bare timestamp, so a year later the
/// attachments folder still says what each picture was for — which is the one question anyone
/// ever asks of a folder full of screenshots.
pub fn attachment_name(note: &str, stamp: u64) -> String {
    let stem = std::path::Path::new(note)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "note".into());
    let stem: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let stem = stem.trim();
    let stem = if stem.is_empty() { "note" } else { stem };
    format!("{stem} {stamp}.png")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder full of `Pasted image 20260815.png` answers no question anybody has. Naming
    /// the note it came from means the folder is still readable a year later.
    #[test]
    fn an_attachment_is_named_after_the_note_it_landed_in() {
        assert_eq!(attachment_name("Auth findings.md", 1755300000), "Auth findings 1755300000.png");
        assert_eq!(attachment_name("sub/dir/Notes.md", 42), "Notes 42.png");
    }

    /// The name becomes a path and a `[[wikilink]]`, so anything a filesystem or a link
    /// cannot carry has to be gone before either sees it.
    #[test]
    fn a_name_that_could_not_be_a_file_or_a_link_is_made_into_one() {
        let n = attachment_name("we[ir]d|name#here.md", 7);
        assert!(!n.contains(['[', ']', '|', '#', '/']), "{n}");
        assert!(n.ends_with(" 7.png"), "{n}");
        assert_eq!(attachment_name("", 1), "note 1.png", "and something unnamed still gets one");
    }

    /// Whatever the clipboard holds, this may not panic and may not return something the
    /// rest of the code would treat as a picture.
    #[test]
    fn reading_the_clipboard_never_returns_an_empty_picture() {
        if let Some(b) = image() {
            assert!(!b.is_empty() && b.len() <= MAX_PASTE);
        }
    }
}
