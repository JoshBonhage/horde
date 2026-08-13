//! Palettes and the mapping from terminal colors to concrete RGB.
//!
//! The daemon resolves every cell color to RGB before it goes on the wire, so the client
//! never has to know about palettes and a theme change is a single re-render.

use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor};
use serde::Deserialize;

use crate::proto::Rgb;

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Semantic UI colors — everything horde's own chrome draws with.
#[derive(Debug, Clone, Copy)]
pub struct Ui {
    /// Primary accent: focused borders, selection, active tab.
    pub accent: Rgb,
    /// Secondary accent, used where two highlights would collide.
    pub accent_alt: Rgb,
    /// Page background behind panes.
    pub bg: Rgb,
    /// Sidebar / drawer background, deliberately a shade off `bg` so panels read as
    /// distinct surfaces without needing a border.
    pub panel_bg: Rgb,
    /// Background of the focused pane's title.
    pub title_bg: Rgb,
    pub text: Rgb,
    pub text_dim: Rgb,
    pub text_faint: Rgb,
    pub border: Rgb,
    pub border_focus: Rgb,
    pub working: Rgb,
    pub blocked: Rgb,
    pub done: Rgb,
    pub idle: Rgb,
    pub unknown: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub error: Rgb,
    pub selection: Rgb,
}

/// How many colours projects are tinted from.
///
/// Six is what every bundled theme can actually supply as *distinct* colours, which is the
/// binding constraint rather than a round number — see `ACCENT_SLOTS`. It is also about the
/// ceiling for telling hues apart when the thing carrying one is a single cell, and more
/// projects than anyone watches at once.
pub const SPACE_ACCENTS: usize = 6;

/// Which ANSI slots the project ramp is built from, ordered so that consecutive spaces differ
/// in hue *and* lightness rather than only hue.
///
/// Only the six chromatic normals. 0, 7, 8 and 15 are excluded because every theme reserves
/// them for text and background, so a space tinted with one would be invisible against the
/// panel it is drawn on. The *bright* variants 9–14 are excluded for a less obvious reason:
/// several themes alias them to the normal colours — catppuccin's 9–14 are byte-identical to
/// its 1–6 — so drawing from them would silently hand two projects the same colour under some
/// themes and not others. `every_theme_hands_out_distinct_visible_project_accents` is what
/// found that, and is what keeps a future palette from reintroducing it.
const ACCENT_SLOTS: [usize; SPACE_ACCENTS] = [4, 5, 6, 2, 3, 1];

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    /// ANSI 0-15.
    pub ansi: [Rgb; 16],
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub ui: Ui,
    /// When true, cell colors that are still "default" pass through to the host terminal's
    /// own palette instead of being pinned to `fg`/`bg`.
    pub inherit_terminal: bool,
    /// Per-slot replacements for the project ramp, from `[theme] space_accents`.
    ///
    /// Sparse rather than a whole palette, so naming one project's colour does not mean
    /// choosing all eight.
    pub space_accent_overrides: [Option<Rgb>; SPACE_ACCENTS],
}

impl Default for Theme {
    fn default() -> Self {
        Theme::horde()
    }
}

impl Theme {
    /// horde's own palette: cool slate ground, mint accent, amber/coral for agent states.
    /// State colors are chosen to stay distinguishable for the common red-green
    /// deficiencies — amber vs coral vs mint differ in lightness as well as hue.
    pub fn horde() -> Theme {
        Theme {
            name: "horde".into(),
            ansi: [
                rgb(0x1a, 0x1e, 0x26), // black
                rgb(0xff, 0x7b, 0x72), // red
                rgb(0x7e, 0xe7, 0x87), // green
                rgb(0xf0, 0xc6, 0x74), // yellow
                rgb(0x79, 0xc0, 0xff), // blue
                rgb(0xd2, 0xa8, 0xff), // magenta
                rgb(0x56, 0xd4, 0xdd), // cyan
                rgb(0xb1, 0xba, 0xc4), // white
                rgb(0x3d, 0x44, 0x4d), // bright black
                rgb(0xff, 0xa1, 0x98),
                rgb(0xa5, 0xf3, 0xb0),
                rgb(0xff, 0xd8, 0x8a),
                rgb(0xa5, 0xd6, 0xff),
                rgb(0xe2, 0xc5, 0xff),
                rgb(0x7e, 0xe8, 0xf0),
                rgb(0xf0, 0xf6, 0xfc), // bright white
            ],
            fg: rgb(0xcd, 0xd6, 0xe4),
            bg: rgb(0x11, 0x14, 0x1a),
            cursor: rgb(0x7e, 0xe2, 0xc0),
            ui: Ui {
                accent: rgb(0x7e, 0xe2, 0xc0),
                accent_alt: rgb(0x79, 0xc0, 0xff),
                bg: rgb(0x11, 0x14, 0x1a),
                panel_bg: rgb(0x0c, 0x0f, 0x14),
                title_bg: rgb(0x1a, 0x20, 0x29),
                text: rgb(0xcd, 0xd6, 0xe4),
                text_dim: rgb(0x8b, 0x94, 0xa3),
                text_faint: rgb(0x55, 0x5d, 0x6a),
                border: rgb(0x26, 0x2c, 0x36),
                border_focus: rgb(0x7e, 0xe2, 0xc0),
                working: rgb(0xf0, 0xc6, 0x74),
                blocked: rgb(0xff, 0x7b, 0x72),
                done: rgb(0x7e, 0xe2, 0xc0),
                idle: rgb(0x6b, 0x74, 0x82),
                unknown: rgb(0x55, 0x5d, 0x6a),
                ok: rgb(0x7e, 0xe7, 0x87),
                warn: rgb(0xf0, 0xc6, 0x74),
                error: rgb(0xff, 0x7b, 0x72),
                selection: rgb(0x2d, 0x3a, 0x44),
            },
            inherit_terminal: false,
            space_accent_overrides: [None; SPACE_ACCENTS],
        }
    }

    pub fn tokyo_night() -> Theme {
        let mut t = Theme::horde();
        t.name = "tokyo-night".into();
        t.ansi = [
            rgb(0x15, 0x16, 0x1e),
            rgb(0xf7, 0x76, 0x8e),
            rgb(0x9e, 0xce, 0x6a),
            rgb(0xe0, 0xaf, 0x68),
            rgb(0x7a, 0xa2, 0xf7),
            rgb(0xbb, 0x9a, 0xf7),
            rgb(0x7d, 0xcf, 0xff),
            rgb(0xa9, 0xb1, 0xd6),
            rgb(0x41, 0x48, 0x68),
            rgb(0xff, 0x9e, 0x64),
            rgb(0xb9, 0xf2, 0x7c),
            rgb(0xff, 0xc7, 0x77),
            rgb(0x9a, 0xbd, 0xf5),
            rgb(0xc0, 0xa9, 0xf9),
            rgb(0xa4, 0xda, 0xff),
            rgb(0xc0, 0xca, 0xf5),
        ];
        t.fg = rgb(0xc0, 0xca, 0xf5);
        t.bg = rgb(0x1a, 0x1b, 0x26);
        t.cursor = rgb(0xc0, 0xca, 0xf5);
        t.ui.accent = rgb(0x7a, 0xa2, 0xf7);
        t.ui.accent_alt = rgb(0xbb, 0x9a, 0xf7);
        t.ui.bg = rgb(0x1a, 0x1b, 0x26);
        t.ui.panel_bg = rgb(0x16, 0x16, 0x1f);
        t.ui.title_bg = rgb(0x24, 0x28, 0x3b);
        t.ui.text = rgb(0xc0, 0xca, 0xf5);
        t.ui.text_dim = rgb(0x9a, 0xa5, 0xce);
        t.ui.text_faint = rgb(0x56, 0x5f, 0x89);
        t.ui.border = rgb(0x29, 0x2e, 0x42);
        t.ui.border_focus = rgb(0x7a, 0xa2, 0xf7);
        t.ui.working = rgb(0xe0, 0xaf, 0x68);
        t.ui.blocked = rgb(0xf7, 0x76, 0x8e);
        t.ui.done = rgb(0x9e, 0xce, 0x6a);
        t.ui.idle = rgb(0x56, 0x5f, 0x89);
        t.ui.selection = rgb(0x28, 0x34, 0x57);
        t
    }

    pub fn catppuccin() -> Theme {
        let mut t = Theme::horde();
        t.name = "catppuccin".into();
        t.ansi = [
            rgb(0x45, 0x47, 0x5a),
            rgb(0xf3, 0x8b, 0xa8),
            rgb(0xa6, 0xe3, 0xa1),
            rgb(0xf9, 0xe2, 0xaf),
            rgb(0x89, 0xb4, 0xfa),
            rgb(0xcb, 0xa6, 0xf7),
            rgb(0x94, 0xe2, 0xd5),
            rgb(0xba, 0xc2, 0xde),
            rgb(0x58, 0x5b, 0x70),
            rgb(0xf3, 0x8b, 0xa8),
            rgb(0xa6, 0xe3, 0xa1),
            rgb(0xf9, 0xe2, 0xaf),
            rgb(0x89, 0xb4, 0xfa),
            rgb(0xcb, 0xa6, 0xf7),
            rgb(0x94, 0xe2, 0xd5),
            rgb(0xa6, 0xad, 0xc8),
        ];
        t.fg = rgb(0xcd, 0xd6, 0xf4);
        t.bg = rgb(0x1e, 0x1e, 0x2e);
        t.cursor = rgb(0xf5, 0xe0, 0xdc);
        t.ui.accent = rgb(0xa6, 0xe3, 0xa1);
        t.ui.accent_alt = rgb(0x89, 0xb4, 0xfa);
        t.ui.bg = rgb(0x1e, 0x1e, 0x2e);
        t.ui.panel_bg = rgb(0x18, 0x18, 0x25);
        t.ui.title_bg = rgb(0x31, 0x32, 0x44);
        t.ui.text = rgb(0xcd, 0xd6, 0xf4);
        t.ui.text_dim = rgb(0xa6, 0xad, 0xc8);
        t.ui.text_faint = rgb(0x6c, 0x70, 0x86);
        t.ui.border = rgb(0x31, 0x32, 0x44);
        t.ui.border_focus = rgb(0xa6, 0xe3, 0xa1);
        t.ui.working = rgb(0xf9, 0xe2, 0xaf);
        t.ui.blocked = rgb(0xf3, 0x8b, 0xa8);
        t.ui.done = rgb(0xa6, 0xe3, 0xa1);
        t.ui.idle = rgb(0x6c, 0x70, 0x86);
        t.ui.selection = rgb(0x41, 0x42, 0x59);
        t
    }

    pub fn gruvbox() -> Theme {
        let mut t = Theme::horde();
        t.name = "gruvbox".into();
        t.ansi = [
            rgb(0x28, 0x28, 0x28),
            rgb(0xcc, 0x24, 0x1d),
            rgb(0x98, 0x97, 0x1a),
            rgb(0xd7, 0x99, 0x21),
            rgb(0x45, 0x85, 0x88),
            rgb(0xb1, 0x62, 0x86),
            rgb(0x68, 0x9d, 0x6a),
            rgb(0xa8, 0x99, 0x84),
            rgb(0x92, 0x83, 0x74),
            rgb(0xfb, 0x49, 0x34),
            rgb(0xb8, 0xbb, 0x26),
            rgb(0xfa, 0xbd, 0x2f),
            rgb(0x83, 0xa5, 0x98),
            rgb(0xd3, 0x86, 0x9b),
            rgb(0x8e, 0xc0, 0x7c),
            rgb(0xeb, 0xdb, 0xb2),
        ];
        t.fg = rgb(0xeb, 0xdb, 0xb2);
        t.bg = rgb(0x28, 0x28, 0x28);
        t.cursor = rgb(0xeb, 0xdb, 0xb2);
        t.ui.accent = rgb(0x8e, 0xc0, 0x7c);
        t.ui.accent_alt = rgb(0x83, 0xa5, 0x98);
        t.ui.bg = rgb(0x28, 0x28, 0x28);
        t.ui.panel_bg = rgb(0x1d, 0x20, 0x21);
        t.ui.title_bg = rgb(0x3c, 0x38, 0x36);
        t.ui.text = rgb(0xeb, 0xdb, 0xb2);
        t.ui.text_dim = rgb(0xbd, 0xae, 0x93);
        t.ui.text_faint = rgb(0x92, 0x83, 0x74);
        t.ui.border = rgb(0x3c, 0x38, 0x36);
        t.ui.border_focus = rgb(0x8e, 0xc0, 0x7c);
        t.ui.working = rgb(0xfa, 0xbd, 0x2f);
        t.ui.blocked = rgb(0xfb, 0x49, 0x34);
        t.ui.done = rgb(0x8e, 0xc0, 0x7c);
        t.ui.idle = rgb(0x92, 0x83, 0x74);
        t.ui.selection = rgb(0x50, 0x49, 0x45);
        t
    }

    /// Follow the host terminal's own ANSI palette. Cell colors that are still "default"
    /// are passed through untouched so the pane looks exactly as it would outside horde.
    pub fn terminal() -> Theme {
        let mut t = Theme::horde();
        t.name = "terminal".into();
        t.inherit_terminal = true;
        t
    }

    /// The colours projects are tinted with, in assignment order.
    ///
    /// Derived from `ansi` rather than stored, because every theme but `horde` is built by
    /// cloning `horde()` and overwriting the ANSI table. A stored ramp would be one more
    /// thing each of them had to remember to set, and the one that forgot would quietly hand
    /// out horde's mint against gruvbox brown.
    pub fn space_accents(&self) -> [Rgb; SPACE_ACCENTS] {
        std::array::from_fn(|i| self.space_accent(i as u8))
    }

    /// One project accent. Wraps, so no caller has to know how many there are.
    pub fn space_accent(&self, slot: u8) -> Rgb {
        let i = slot as usize % SPACE_ACCENTS;
        self.space_accent_overrides[i].unwrap_or(self.ansi[ACCENT_SLOTS[i]])
    }

    pub fn by_name(name: &str) -> Option<Theme> {
        Some(match name {
            "horde" => Theme::horde(),
            "tokyo-night" => Theme::tokyo_night(),
            "catppuccin" => Theme::catppuccin(),
            "gruvbox" => Theme::gruvbox(),
            "terminal" => Theme::terminal(),
            _ => return None,
        })
    }

    pub fn names() -> &'static [&'static str] {
        &["horde", "tokyo-night", "catppuccin", "gruvbox", "terminal"]
    }

    /// Apply `[theme.custom]` overrides from config.
    pub fn apply_overrides(&mut self, o: &ThemeOverrides) {
        macro_rules! set {
            ($($field:ident),* $(,)?) => {
                $(if let Some(c) = o.$field.as_deref().and_then(parse_color) {
                    self.ui.$field = c;
                })*
            };
        }
        set!(
            accent, accent_alt, bg, panel_bg, title_bg, text, text_dim, text_faint, border,
            border_focus, working, blocked, done, idle, unknown, ok, warn, error, selection,
        );
        if let Some(c) = o.fg.as_deref().and_then(parse_color) {
            self.fg = c;
        }
        if let Some(c) = o.cursor.as_deref().and_then(parse_color) {
            self.cursor = c;
        }
        // `bg` doubles as the terminal background, so keep the two in step.
        if let Some(c) = o.bg.as_deref().and_then(parse_color) {
            self.bg = c;
        }
    }

    /// Resolve a terminal cell color to RGB.
    ///
    /// `Named(Foreground)`/`Named(Background)` mean "whatever the default is", which is why
    /// they resolve against the theme rather than the ANSI table.
    pub fn resolve(&self, c: VteColor) -> Rgb {
        match c {
            VteColor::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
            VteColor::Indexed(i) => self.indexed(i),
            VteColor::Named(n) => self.named(n),
        }
    }

    fn named(&self, n: NamedColor) -> Rgb {
        use NamedColor as N;
        match n {
            N::Black => self.ansi[0],
            N::Red => self.ansi[1],
            N::Green => self.ansi[2],
            N::Yellow => self.ansi[3],
            N::Blue => self.ansi[4],
            N::Magenta => self.ansi[5],
            N::Cyan => self.ansi[6],
            N::White => self.ansi[7],
            N::BrightBlack => self.ansi[8],
            N::BrightRed => self.ansi[9],
            N::BrightGreen => self.ansi[10],
            N::BrightYellow => self.ansi[11],
            N::BrightBlue => self.ansi[12],
            N::BrightMagenta => self.ansi[13],
            N::BrightCyan => self.ansi[14],
            N::BrightWhite => self.ansi[15],
            N::Foreground => self.fg,
            N::Background => self.bg,
            N::Cursor => self.cursor,
            // Dim variants are the standard 0.66 scaling of their base color.
            N::DimBlack => dim(self.ansi[0]),
            N::DimRed => dim(self.ansi[1]),
            N::DimGreen => dim(self.ansi[2]),
            N::DimYellow => dim(self.ansi[3]),
            N::DimBlue => dim(self.ansi[4]),
            N::DimMagenta => dim(self.ansi[5]),
            N::DimCyan => dim(self.ansi[6]),
            N::DimWhite => dim(self.ansi[7]),
            N::BrightForeground => self.ansi[15],
            N::DimForeground => dim(self.fg),
        }
    }

    /// xterm 256: 0-15 ANSI, 16-231 a 6x6x6 cube, 232-255 a 24-step gray ramp.
    pub fn indexed(&self, i: u8) -> Rgb {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                let i = i as usize - 16;
                Rgb::new(STEPS[(i / 36) % 6], STEPS[(i / 6) % 6], STEPS[i % 6])
            }
            232..=255 => {
                let v = 8 + (i as u16 - 232) * 10;
                let v = v.min(255) as u8;
                Rgb::new(v, v, v)
            }
        }
    }
}

fn dim(c: Rgb) -> Rgb {
    Rgb::new(
        (c.r as f32 * 0.66) as u8,
        (c.g as f32 * 0.66) as u8,
        (c.b as f32 * 0.66) as u8,
    )
}

/// Blend `a` over `b` by `t` in 0..=1. Used for the pulse on blocked rows.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgb::new(f(a.r, b.r), f(a.g, b.g), f(a.b, b.b))
}

/// `#rgb`, `#rrggbb`, `rgb(r,g,b)`, or a bare ANSI color name.
pub fn parse_color(s: &str) -> Option<Rgb> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
                Some(Rgb::new(d(0)?, d(1)?, d(2)?))
            }
            6 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
                Some(Rgb::new(d(0)?, d(2)?, d(4)?))
            }
            _ => None,
        };
    }
    if let Some(inner) = s.strip_prefix("rgb(").and_then(|r| r.strip_suffix(')')) {
        let parts: Vec<_> = inner.split(',').map(|p| p.trim().parse::<u8>()).collect();
        if parts.len() == 3 {
            return Some(Rgb::new(
                *parts[0].as_ref().ok()?,
                *parts[1].as_ref().ok()?,
                *parts[2].as_ref().ok()?,
            ));
        }
        return None;
    }
    let t = Theme::horde();
    let idx = match s.to_ascii_lowercase().as_str() {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        "brightblack" | "gray" | "grey" => 8,
        "brightred" => 9,
        "brightgreen" => 10,
        "brightyellow" => 11,
        "brightblue" => 12,
        "brightmagenta" => 13,
        "brightcyan" => 14,
        "brightwhite" => 15,
        _ => return None,
    };
    Some(t.ansi[idx])
}

/// `[theme.custom]` in config.toml. Every field is an optional color string.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeOverrides {
    pub accent: Option<String>,
    pub accent_alt: Option<String>,
    pub bg: Option<String>,
    pub panel_bg: Option<String>,
    pub title_bg: Option<String>,
    pub text: Option<String>,
    pub text_dim: Option<String>,
    pub text_faint: Option<String>,
    pub border: Option<String>,
    pub border_focus: Option<String>,
    pub working: Option<String>,
    pub blocked: Option<String>,
    pub done: Option<String>,
    pub idle: Option<String>,
    pub unknown: Option<String>,
    pub ok: Option<String>,
    pub warn: Option<String>,
    pub error: Option<String>,
    pub selection: Option<String>,
    pub fg: Option<String>,
    pub cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_supported_color_form() {
        assert_eq!(parse_color("#7ee2c0"), Some(Rgb::new(0x7e, 0xe2, 0xc0)));
        assert_eq!(parse_color("#fff"), Some(Rgb::new(255, 255, 255)));
        assert_eq!(parse_color("rgb(10, 20, 30)"), Some(Rgb::new(10, 20, 30)));
        assert_eq!(parse_color("cyan"), Some(Theme::horde().ansi[6]));
        assert_eq!(parse_color("nonsense"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("rgb(1,2)"), None);
        assert_eq!(parse_color("rgb(1,2,999)"), None, "out-of-range must not wrap");
    }

    #[test]
    fn indexed_palette_matches_xterm_landmarks() {
        let t = Theme::horde();
        assert_eq!(t.indexed(3), t.ansi[3]);
        // 16 is the base of the cube (pure black), 231 its apex (pure white).
        assert_eq!(t.indexed(16), Rgb::new(0, 0, 0));
        assert_eq!(t.indexed(231), Rgb::new(255, 255, 255));
        // 196 is xterm's pure red.
        assert_eq!(t.indexed(196), Rgb::new(255, 0, 0));
        assert_eq!(t.indexed(232), Rgb::new(8, 8, 8));
        assert_eq!(t.indexed(255), Rgb::new(238, 238, 238));
    }

    #[test]
    fn default_colors_resolve_through_the_theme_not_the_ansi_table() {
        let t = Theme::horde();
        assert_eq!(t.resolve(VteColor::Named(NamedColor::Foreground)), t.fg);
        assert_eq!(t.resolve(VteColor::Named(NamedColor::Background)), t.bg);
    }

    #[test]
    fn every_named_theme_loads() {
        for n in Theme::names() {
            assert_eq!(&Theme::by_name(n).expect(n).name, n);
        }
        assert!(Theme::by_name("nope").is_none());
    }

    #[test]
    fn overrides_replace_only_named_fields() {
        let mut t = Theme::horde();
        let before_border = t.ui.border;
        t.apply_overrides(&ThemeOverrides {
            accent: Some("#ff0000".into()),
            blocked: Some("bogus".into()),
            ..Default::default()
        });
        assert_eq!(t.ui.accent, Rgb::new(255, 0, 0));
        assert_eq!(t.ui.border, before_border, "untouched fields must survive");
        assert_eq!(
            t.ui.blocked,
            Theme::horde().ui.blocked,
            "an unparseable override must be ignored, not zeroed"
        );
    }

    #[test]
    fn mix_interpolates_between_endpoints() {
        let a = Rgb::new(0, 0, 0);
        let b = Rgb::new(100, 200, 50);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Rgb::new(50, 100, 25));
        assert_eq!(mix(a, b, 5.0), b, "t must clamp");
    }

    /// A project dot is often a single cell, so two spaces sharing a colour is the same as
    /// having no colour — and one drawn in the panel's own background is invisible outright.
    #[test]
    fn every_theme_hands_out_distinct_visible_project_accents() {
        for name in Theme::names() {
            let t = Theme::by_name(name).unwrap();
            let ramp = t.space_accents();
            for (i, c) in ramp.iter().enumerate() {
                assert_ne!(*c, t.ui.panel_bg, "{name} slot {i} is invisible on the panel");
            }
            let mut sorted: Vec<(u8, u8, u8)> = ramp.iter().map(|c| (c.r, c.g, c.b)).collect();
            sorted.sort_unstable();
            let n = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), n, "{name} repeats a project colour");
        }
    }

    /// The executable form of "an accent is a slot, not a hex string": the same space, drawn
    /// under two themes, is two different colours. Storing RGB would have left one project
    /// painted in the old palette while everything around it moved.
    #[test]
    fn project_accents_follow_the_theme_not_a_stored_hex() {
        let a = Theme::horde();
        let b = Theme::gruvbox();
        assert_ne!(a.space_accent(0), b.space_accent(0));
    }

    #[test]
    fn a_space_accent_override_replaces_only_that_slot() {
        let mut t = Theme::horde();
        let before = t.space_accents();
        t.space_accent_overrides[2] = Some(Rgb::new(1, 2, 3));
        assert_eq!(t.space_accent(2), Rgb::new(1, 2, 3));
        assert_eq!(t.space_accent(0), before[0]);
        assert_eq!(t.space_accent(5), before[5]);
    }

    /// Callers pass a slot without knowing how many exist, so it has to wrap rather than
    /// panic at the edge of the ramp.
    #[test]
    fn an_out_of_range_slot_wraps() {
        let t = Theme::horde();
        assert_eq!(t.space_accent(SPACE_ACCENTS as u8), t.space_accent(0));
        assert_eq!(t.space_accent(255), t.space_accent((255 % SPACE_ACCENTS) as u8));
    }
}
