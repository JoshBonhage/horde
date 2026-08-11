//! Translating client input into bytes a PTY understands.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};

/// CSI modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn mod_param(m: KeyModifiers) -> u8 {
    let mut v = 1;
    if m.contains(KeyModifiers::SHIFT) {
        v += 1;
    }
    if m.contains(KeyModifiers::ALT) {
        v += 2;
    }
    if m.contains(KeyModifiers::CONTROL) {
        v += 4;
    }
    v
}

/// Encode an arrow/navigation key, adding the modifier parameter when any modifier is held.
fn csi_key(final_byte: char, m: KeyModifiers) -> Vec<u8> {
    let p = mod_param(m);
    if p == 1 {
        format!("\x1b[{final_byte}").into_bytes()
    } else {
        format!("\x1b[1;{p}{final_byte}").into_bytes()
    }
}

/// Encode a `CSI <n> ~` style key.
fn csi_tilde(n: u8, m: KeyModifiers) -> Vec<u8> {
    let p = mod_param(m);
    if p == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{p}~").into_bytes()
    }
}

/// Bytes to send to a pane for a key press, or None when the key has no encoding.
pub fn encode_key(ev: &KeyEvent) -> Option<Vec<u8>> {
    // Terminals without the kitty protocol only report presses; when they do report
    // releases, sending them would double every keystroke.
    if ev.kind == KeyEventKind::Release {
        return None;
    }
    let m = ev.modifiers;

    Some(match ev.code {
        KeyCode::Char(c) => {
            if m.contains(KeyModifiers::CONTROL) {
                // Control collapses a letter to its low control code; ctrl+space is NUL.
                let byte = match c {
                    'a'..='z' => (c as u8) - b'a' + 1,
                    'A'..='Z' => (c as u8) - b'A' + 1,
                    ' ' | '@' => 0,
                    '[' => 27,
                    '\\' => 28,
                    ']' => 29,
                    '^' => 30,
                    '_' | '/' => 31,
                    '?' => 127,
                    _ => return Some(c.to_string().into_bytes()),
                };
                if m.contains(KeyModifiers::ALT) {
                    vec![0x1b, byte]
                } else {
                    vec![byte]
                }
            } else if m.contains(KeyModifiers::ALT) {
                // Alt is reported as ESC followed by the key.
                let mut v = vec![0x1b];
                v.extend(c.to_string().into_bytes());
                v
            } else {
                c.to_string().into_bytes()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // Terminals send DEL for backspace; ctrl+backspace sends BS.
        KeyCode::Backspace => {
            if m.contains(KeyModifiers::CONTROL) {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi_key('A', m),
        KeyCode::Down => csi_key('B', m),
        KeyCode::Right => csi_key('C', m),
        KeyCode::Left => csi_key('D', m),
        KeyCode::Home => csi_key('H', m),
        KeyCode::End => csi_key('F', m),
        KeyCode::Insert => csi_tilde(2, m),
        KeyCode::Delete => csi_tilde(3, m),
        KeyCode::PageUp => csi_tilde(5, m),
        KeyCode::PageDown => csi_tilde(6, m),
        KeyCode::F(n) => match n {
            // F1-F4 use SS3; the rest use CSI ~ with historical, non-contiguous numbers.
            1 => ss3('P', m),
            2 => ss3('Q', m),
            3 => ss3('R', m),
            4 => ss3('S', m),
            5 => csi_tilde(15, m),
            6 => csi_tilde(17, m),
            7 => csi_tilde(18, m),
            8 => csi_tilde(19, m),
            9 => csi_tilde(20, m),
            10 => csi_tilde(21, m),
            11 => csi_tilde(23, m),
            12 => csi_tilde(24, m),
            _ => return None,
        },
        _ => return None,
    })
}

fn ss3(final_byte: char, m: KeyModifiers) -> Vec<u8> {
    let p = mod_param(m);
    if p == 1 {
        format!("\x1bO{final_byte}").into_bytes()
    } else {
        // Modified function keys switch to the CSI form.
        format!("\x1b[1;{p}{final_byte}").into_bytes()
    }
}

/// Wrap pasted text in bracketed-paste markers when the pane asked for them.
///
/// Without this, a multi-line paste into an agent's prompt submits on the first newline.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut v = b"\x1b[200~".to_vec();
        v.extend(text.as_bytes());
        v.extend(b"\x1b[201~");
        v
    } else {
        text.as_bytes().to_vec()
    }
}

/// SGR mouse report, relative to a pane's content rect (1-based, as the protocol requires).
pub fn encode_mouse(kind: MouseEventKind, col: u16, row: u16, m: KeyModifiers) -> Option<Vec<u8>> {
    let mut base = match kind {
        MouseEventKind::Down(b) | MouseEventKind::Up(b) => match b {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        },
        MouseEventKind::Drag(b) => {
            32 + match b {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
            }
        }
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    if m.contains(KeyModifiers::SHIFT) {
        base += 4;
    }
    if m.contains(KeyModifiers::ALT) {
        base += 8;
    }
    if m.contains(KeyModifiers::CONTROL) {
        base += 16;
    }

    let release = matches!(kind, MouseEventKind::Up(_));
    let suffix = if release { 'm' } else { 'M' };
    Some(format!("\x1b[<{base};{};{}{suffix}", col + 1, row + 1).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    fn enc(code: KeyCode, m: KeyModifiers) -> Vec<u8> {
        encode_key(&key(code, m)).unwrap()
    }

    #[test]
    fn plain_characters_pass_through_as_utf8() {
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(enc(KeyCode::Char('é'), KeyModifiers::NONE), "é".as_bytes());
        assert_eq!(enc(KeyCode::Char('日'), KeyModifiers::NONE), "日".as_bytes());
    }

    #[test]
    fn control_letters_become_control_codes() {
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::CONTROL), vec![1]);
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(enc(KeyCode::Char('d'), KeyModifiers::CONTROL), vec![4]);
        // Uppercase must map to the same code as lowercase.
        assert_eq!(enc(KeyCode::Char('C'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(enc(KeyCode::Char(' '), KeyModifiers::CONTROL), vec![0]);
        assert_eq!(enc(KeyCode::Char('['), KeyModifiers::CONTROL), vec![27]);
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(enc(KeyCode::Char('b'), KeyModifiers::ALT), vec![0x1b, b'b']);
        assert_eq!(
            enc(KeyCode::Char('a'), KeyModifiers::ALT | KeyModifiers::CONTROL),
            vec![0x1b, 1]
        );
    }

    #[test]
    fn enter_sends_carriage_return_not_newline() {
        // A newline would not submit a prompt in most line editors.
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::NONE), b"\r");
    }

    #[test]
    fn backspace_sends_del_by_default() {
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::NONE), vec![0x7f]);
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::CONTROL), vec![0x08]);
    }

    #[test]
    fn arrows_use_csi_and_gain_a_modifier_parameter() {
        assert_eq!(enc(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(enc(KeyCode::Left, KeyModifiers::NONE), b"\x1b[D");
        // shift = 2, alt = 3, ctrl = 5, ctrl+shift = 6.
        assert_eq!(enc(KeyCode::Up, KeyModifiers::SHIFT), b"\x1b[1;2A");
        assert_eq!(enc(KeyCode::Up, KeyModifiers::ALT), b"\x1b[1;3A");
        assert_eq!(enc(KeyCode::Right, KeyModifiers::CONTROL), b"\x1b[1;5C");
        assert_eq!(
            enc(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            b"\x1b[1;6C"
        );
    }

    #[test]
    fn navigation_keys_use_the_tilde_forms() {
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(enc(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(enc(KeyCode::PageDown, KeyModifiers::NONE), b"\x1b[6~");
        assert_eq!(enc(KeyCode::Insert, KeyModifiers::NONE), b"\x1b[2~");
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::CONTROL), b"\x1b[3;5~");
    }

    #[test]
    fn function_keys_split_between_ss3_and_csi() {
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(4), KeyModifiers::NONE), b"\x1bOS");
        // F5 upward use CSI with historical numbering that skips values.
        assert_eq!(enc(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(6), KeyModifiers::NONE), b"\x1b[17~");
        assert_eq!(enc(KeyCode::F(12), KeyModifiers::NONE), b"\x1b[24~");
        // A modified F1 switches to the CSI form.
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::SHIFT), b"\x1b[1;2P");
        assert!(encode_key(&key(KeyCode::F(20), KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn tab_and_backtab_differ() {
        assert_eq!(enc(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(enc(KeyCode::BackTab, KeyModifiers::NONE), b"\x1b[Z");
    }

    #[test]
    fn key_releases_are_dropped_so_keystrokes_are_not_doubled() {
        let mut ev = key(KeyCode::Char('a'), KeyModifiers::NONE);
        ev.kind = KeyEventKind::Release;
        assert!(encode_key(&ev).is_none());

        // Repeats are real input and must be forwarded.
        ev.kind = KeyEventKind::Repeat;
        assert_eq!(encode_key(&ev).unwrap(), b"a");
    }

    #[test]
    fn bracketed_paste_wraps_only_when_the_pane_asked_for_it() {
        assert_eq!(encode_paste("a\nb", true), b"\x1b[200~a\nb\x1b[201~".to_vec());
        assert_eq!(encode_paste("a\nb", false), b"a\nb".to_vec());
    }

    #[test]
    fn mouse_reports_are_one_based_with_the_right_button_codes() {
        // The protocol counts from 1 while the client counts from 0.
        let down = encode_mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            0,
            KeyModifiers::NONE,
        )
        .unwrap();
        assert_eq!(down, b"\x1b[<0;1;1M");

        let up =
            encode_mouse(MouseEventKind::Up(MouseButton::Left), 4, 2, KeyModifiers::NONE).unwrap();
        assert_eq!(up, b"\x1b[<0;5;3m", "release uses a lowercase m");

        let right =
            encode_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, KeyModifiers::NONE)
                .unwrap();
        assert_eq!(right, b"\x1b[<2;1;1M");

        let wheel = encode_mouse(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE).unwrap();
        assert_eq!(wheel, b"\x1b[<64;1;1M");

        let drag =
            encode_mouse(MouseEventKind::Drag(MouseButton::Left), 1, 1, KeyModifiers::NONE)
                .unwrap();
        assert_eq!(drag, b"\x1b[<32;2;2M");
    }

    #[test]
    fn mouse_modifiers_add_to_the_button_code() {
        let ctrl =
            encode_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, KeyModifiers::CONTROL)
                .unwrap();
        assert_eq!(ctrl, b"\x1b[<16;1;1M");
        let shift =
            encode_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, KeyModifiers::SHIFT)
                .unwrap();
        assert_eq!(shift, b"\x1b[<4;1;1M");
    }
}
