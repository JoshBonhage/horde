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
    /// A dev server or watcher that is up. Its own hue on purpose: a service is background
    /// texture you want to be able to *not* look at, which it cannot be while it shares a
    /// colour with an agent mid-turn.
    pub serving: Rgb,
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

/// Colours for the things a language has in it.
///
/// Derived from the theme's own palette rather than declared per theme: a scheme that looks
/// like horde in its chrome and like somebody else's editor in its code is two designs in
/// one window. Every theme gets syntax colouring for free, and a theme that wants to differ
/// can still override the fields it cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syntax {
    pub keyword: Rgb,
    pub function: Rgb,
    pub type_name: Rgb,
    pub string: Rgb,
    pub number: Rgb,
    pub comment: Rgb,
    pub constant: Rgb,
    pub punctuation: Rgb,
    pub variable: Rgb,
}

impl Syntax {
    /// The palette a theme implies. Keywords take the accent, because they are the words
    /// that give a line its shape; comments take the faintest text colour, because they are
    /// the part you skip when you are looking for something.
    fn from(ui: &Ui, ansi: &[Rgb; 16]) -> Syntax {
        Syntax {
            keyword: ui.accent,
            function: ui.accent_alt,
            type_name: ansi[6],
            string: ui.ok,
            number: ansi[5],
            comment: ui.text_faint,
            constant: ui.warn,
            punctuation: ui.text_dim,
            variable: ui.text,
        }
    }

    /// The colour for a tree-sitter highlight name, longest match first.
    ///
    /// Names are dotted (`keyword.control`, `string.special`), so a scope nobody has an
    /// opinion about falls back to its family rather than to nothing.
    pub fn for_scope(&self, scope: &str) -> Rgb {
        let head = scope.split('.').next().unwrap_or(scope);
        match head {
            "keyword" | "operator" | "keyword.control" => self.keyword,
            "function" | "method" => self.function,
            "type" | "constructor" | "namespace" | "module" => self.type_name,
            "string" | "character" => self.string,
            "number" | "float" | "boolean" => self.number,
            "comment" => self.comment,
            "constant" | "attribute" | "label" => self.constant,
            "punctuation" | "tag" => self.punctuation,
            _ => self.variable,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    /// ANSI 0-15.
    pub ansi: [Rgb; 16],
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub ui: Ui,
    /// Colours for code, derived from `ui` unless a theme says otherwise.
    pub syntax: Syntax,
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

/// Stand-ins used only while the first `Theme` literal is being built; every theme's real
/// syntax palette is computed by `derive_syntax` once its colours are final.
const ANSI_PLACEHOLDER: [Rgb; 16] = [Rgb::new(0, 0, 0); 16];
const UI_PLACEHOLDER: Ui = Ui {
    accent: Rgb::new(0, 0, 0),
    accent_alt: Rgb::new(0, 0, 0),
    bg: Rgb::new(0, 0, 0),
    panel_bg: Rgb::new(0, 0, 0),
    title_bg: Rgb::new(0, 0, 0),
    text: Rgb::new(0, 0, 0),
    text_dim: Rgb::new(0, 0, 0),
    text_faint: Rgb::new(0, 0, 0),
    border: Rgb::new(0, 0, 0),
    border_focus: Rgb::new(0, 0, 0),
    working: Rgb::new(0, 0, 0),
    blocked: Rgb::new(0, 0, 0),
    done: Rgb::new(0, 0, 0),
    idle: Rgb::new(0, 0, 0),
    unknown: Rgb::new(0, 0, 0),
    serving: Rgb::new(0, 0, 0),
    ok: Rgb::new(0, 0, 0),
    warn: Rgb::new(0, 0, 0),
    error: Rgb::new(0, 0, 0),
    selection: Rgb::new(0, 0, 0),
};

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
                serving: rgb(0x79, 0xc0, 0xff),
                ok: rgb(0x7e, 0xe7, 0x87),
                warn: rgb(0xf0, 0xc6, 0x74),
                error: rgb(0xff, 0x7b, 0x72),
                selection: rgb(0x2d, 0x3a, 0x44),
            },
            inherit_terminal: false,
            space_accent_overrides: [None; SPACE_ACCENTS],
            // Stand-in only: `derive_syntax` below replaces it before this returns. Every
            // other constructor copies this theme and *then* changes `ansi` and `ui`, so a
            // palette computed at the literal would describe horde's colours in gruvbox.
            syntax: Syntax::from(&UI_PLACEHOLDER, &ANSI_PLACEHOLDER),
        }
        .derive_syntax()
    }

    /// Recompute the syntax palette from the theme's own colours.
    ///
    /// Called after a theme is built rather than inside each constructor, because the
    /// derived themes change their palette after copying horde's and would otherwise carry
    /// horde's syntax colours into a window painted in gruvbox.
    fn derive_syntax(mut self) -> Theme {
        self.syntax = Syntax::from(&self.ui, &self.ansi);
        self
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
        t.ui.serving = rgb(0x7d, 0xcf, 0xff);
        t.ui.selection = rgb(0x28, 0x34, 0x57);
        t.derive_syntax()
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
        t.ui.serving = rgb(0x89, 0xb4, 0xfa);
        t.ui.selection = rgb(0x41, 0x42, 0x59);
        t.derive_syntax()
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
        t.ui.serving = rgb(0x83, 0xa5, 0x98);
        t.ui.selection = rgb(0x50, 0x49, 0x45);
        t.derive_syntax()
    }


    pub fn nord() -> Theme {
        let mut t = Theme::horde();
        t.name = "nord".into();
        t.ansi = [
            rgb(0x3b, 0x42, 0x52),
            rgb(0xbf, 0x61, 0x6a),
            rgb(0xa3, 0xbe, 0x8c),
            rgb(0xeb, 0xcb, 0x8b),
            rgb(0x81, 0xa1, 0xc1),
            rgb(0xb4, 0x8e, 0xad),
            rgb(0x88, 0xc0, 0xd0),
            rgb(0xe5, 0xe9, 0xf0),
            rgb(0x4c, 0x56, 0x6a),
            rgb(0xd0, 0x87, 0x70),
            rgb(0xb9, 0xd0, 0xa4),
            rgb(0xf0, 0xd8, 0xa8),
            rgb(0x9a, 0xb6, 0xd4),
            rgb(0xc9, 0xa8, 0xc4),
            rgb(0x8f, 0xbc, 0xbb),
            rgb(0xec, 0xef, 0xf4),
        ];
        t.fg = rgb(0xd8, 0xde, 0xe9);
        t.bg = rgb(0x2e, 0x34, 0x40);
        t.cursor = rgb(0x88, 0xc0, 0xd0);
        t.ui.accent = rgb(0x88, 0xc0, 0xd0);
        t.ui.accent_alt = rgb(0x81, 0xa1, 0xc1);
        t.ui.bg = rgb(0x2e, 0x34, 0x40);
        t.ui.panel_bg = rgb(0x29, 0x2e, 0x39);
        t.ui.title_bg = rgb(0x3b, 0x42, 0x52);
        t.ui.text = rgb(0xd8, 0xde, 0xe9);
        t.ui.text_dim = rgb(0x9b, 0xa7, 0xbb);
        t.ui.text_faint = rgb(0x66, 0x72, 0x86);
        t.ui.border = rgb(0x3b, 0x42, 0x52);
        t.ui.border_focus = rgb(0x88, 0xc0, 0xd0);
        t.ui.working = rgb(0xeb, 0xcb, 0x8b);
        t.ui.blocked = rgb(0xbf, 0x61, 0x6a);
        t.ui.done = rgb(0xa3, 0xbe, 0x8c);
        t.ui.idle = rgb(0x66, 0x72, 0x86);
        t.ui.unknown = rgb(0x4c, 0x56, 0x6a);
        t.ui.serving = rgb(0x81, 0xa1, 0xc1);
        t.ui.ok = rgb(0xa3, 0xbe, 0x8c);
        t.ui.warn = rgb(0xeb, 0xcb, 0x8b);
        t.ui.error = rgb(0xbf, 0x61, 0x6a);
        t.ui.selection = rgb(0x43, 0x4c, 0x5e);
        t.derive_syntax()
    }

    pub fn rose_pine() -> Theme {
        let mut t = Theme::horde();
        t.name = "rose-pine".into();
        t.ansi = [
            rgb(0x26, 0x23, 0x3a),
            rgb(0xeb, 0x6f, 0x92),
            rgb(0x31, 0x74, 0x8f),
            rgb(0xf6, 0xc1, 0x77),
            rgb(0x9c, 0xcf, 0xd8),
            rgb(0xc4, 0xa7, 0xe7),
            rgb(0xeb, 0xbc, 0xba),
            rgb(0xe0, 0xde, 0xf4),
            rgb(0x6e, 0x6a, 0x86),
            rgb(0xf2, 0x8a, 0xa8),
            rgb(0x3f, 0x93, 0xb3),
            rgb(0xf9, 0xd0, 0x99),
            rgb(0xb5, 0xdf, 0xe6),
            rgb(0xd6, 0xbe, 0xf2),
            rgb(0xf2, 0xcd, 0xcb),
            rgb(0xf0, 0xef, 0xfa),
        ];
        t.fg = rgb(0xe0, 0xde, 0xf4);
        t.bg = rgb(0x19, 0x17, 0x24);
        t.cursor = rgb(0xeb, 0xbc, 0xba);
        t.ui.accent = rgb(0xeb, 0xbc, 0xba);
        t.ui.accent_alt = rgb(0x9c, 0xcf, 0xd8);
        t.ui.bg = rgb(0x19, 0x17, 0x24);
        t.ui.panel_bg = rgb(0x13, 0x11, 0x1c);
        t.ui.title_bg = rgb(0x26, 0x23, 0x3a);
        t.ui.text = rgb(0xe0, 0xde, 0xf4);
        t.ui.text_dim = rgb(0x90, 0x8c, 0xaa);
        t.ui.text_faint = rgb(0x6e, 0x6a, 0x86);
        t.ui.border = rgb(0x26, 0x23, 0x3a);
        t.ui.border_focus = rgb(0xeb, 0xbc, 0xba);
        t.ui.working = rgb(0xf6, 0xc1, 0x77);
        t.ui.blocked = rgb(0xeb, 0x6f, 0x92);
        t.ui.done = rgb(0x9c, 0xcf, 0xd8);
        t.ui.idle = rgb(0x6e, 0x6a, 0x86);
        t.ui.unknown = rgb(0x55, 0x51, 0x6b);
        t.ui.serving = rgb(0xc4, 0xa7, 0xe7);
        t.ui.ok = rgb(0x9c, 0xcf, 0xd8);
        t.ui.warn = rgb(0xf6, 0xc1, 0x77);
        t.ui.error = rgb(0xeb, 0x6f, 0x92);
        t.ui.selection = rgb(0x2a, 0x27, 0x3f);
        t.derive_syntax()
    }

    /// The first light theme horde has had.
    ///
    /// Light is not dark with the two ends swapped. Three things have to be re-decided rather
    /// than inverted: `text_faint` has to stay *readable* on paper where a dark theme can let
    /// it fall away to nothing, `selection` has to darken the ground instead of lightening
    /// it, and the ANSI normals have to be the darker halves of each hue or a yellow string
    /// on a near-white background is a blank line.
    pub fn rose_pine_dawn() -> Theme {
        let mut t = Theme::horde();
        t.name = "rose-pine-dawn".into();
        t.ansi = [
            rgb(0xf2, 0xe9, 0xe1),
            rgb(0xb4, 0x63, 0x7a),
            rgb(0x28, 0x69, 0x83),
            rgb(0xea, 0x9d, 0x34),
            rgb(0x56, 0x94, 0x9f),
            rgb(0x90, 0x7a, 0xa9),
            rgb(0xd7, 0x82, 0x7e),
            rgb(0x57, 0x52, 0x79),
            rgb(0x9d, 0x8b, 0x8b),
            rgb(0xc4, 0x76, 0x8c),
            rgb(0x35, 0x7b, 0x95),
            rgb(0xf0, 0xaf, 0x55),
            rgb(0x6b, 0xa6, 0xb1),
            rgb(0xa2, 0x8c, 0xbb),
            rgb(0xe2, 0x95, 0x91),
            rgb(0x57, 0x52, 0x79),
        ];
        t.fg = rgb(0x57, 0x52, 0x79);
        t.bg = rgb(0xfa, 0xf4, 0xed);
        t.cursor = rgb(0xd7, 0x82, 0x7e);
        t.ui.accent = rgb(0xd7, 0x82, 0x7e);
        t.ui.accent_alt = rgb(0x28, 0x69, 0x83);
        t.ui.bg = rgb(0xfa, 0xf4, 0xed);
        t.ui.panel_bg = rgb(0xff, 0xfa, 0xf3);
        t.ui.title_bg = rgb(0xf2, 0xe9, 0xe1);
        t.ui.text = rgb(0x57, 0x52, 0x79);
        t.ui.text_dim = rgb(0x79, 0x73, 0x93);
        // Deliberately darker than a dark theme's faint. On paper, "barely there" and
        // "illegible" are a few percent apart.
        t.ui.text_faint = rgb(0x9d, 0x8b, 0x8b);
        t.ui.border = rgb(0xdf, 0xd9, 0xd2);
        t.ui.border_focus = rgb(0xd7, 0x82, 0x7e);
        t.ui.working = rgb(0xea, 0x9d, 0x34);
        t.ui.blocked = rgb(0xb4, 0x63, 0x7a);
        t.ui.done = rgb(0x28, 0x69, 0x83);
        t.ui.idle = rgb(0x9d, 0x8b, 0x8b);
        t.ui.unknown = rgb(0xb5, 0xa9, 0xa9);
        t.ui.serving = rgb(0x56, 0x94, 0x9f);
        t.ui.ok = rgb(0x28, 0x69, 0x83);
        t.ui.warn = rgb(0xea, 0x9d, 0x34);
        t.ui.error = rgb(0xb4, 0x63, 0x7a);
        // Darker than the page, not lighter: on light ground a highlight is a shadow.
        t.ui.selection = rgb(0xea, 0xdf, 0xd4);
        t.derive_syntax()
    }

    pub fn solarized_light() -> Theme {
        let mut t = Theme::horde();
        t.name = "solarized-light".into();
        t.ansi = [
            rgb(0x07, 0x36, 0x42),
            rgb(0xdc, 0x32, 0x2f),
            rgb(0x85, 0x99, 0x00),
            rgb(0xb5, 0x89, 0x00),
            rgb(0x26, 0x8b, 0xd2),
            rgb(0xd3, 0x36, 0x82),
            rgb(0x2a, 0xa1, 0x98),
            rgb(0xee, 0xe8, 0xd5),
            rgb(0x58, 0x6e, 0x75),
            rgb(0xcb, 0x4b, 0x16),
            rgb(0x93, 0xa1, 0xa1),
            rgb(0x83, 0x94, 0x96),
            rgb(0x65, 0x7b, 0x83),
            rgb(0x6c, 0x71, 0xc4),
            rgb(0x35, 0xb6, 0xac),
            rgb(0xfd, 0xf6, 0xe3),
        ];
        t.fg = rgb(0x65, 0x7b, 0x83);
        t.bg = rgb(0xfd, 0xf6, 0xe3);
        t.cursor = rgb(0x26, 0x8b, 0xd2);
        t.ui.accent = rgb(0x26, 0x8b, 0xd2);
        t.ui.accent_alt = rgb(0x2a, 0xa1, 0x98);
        t.ui.bg = rgb(0xfd, 0xf6, 0xe3);
        t.ui.panel_bg = rgb(0xee, 0xe8, 0xd5);
        t.ui.title_bg = rgb(0xe4, 0xdd, 0xc8);
        t.ui.text = rgb(0x07, 0x36, 0x42);
        t.ui.text_dim = rgb(0x58, 0x6e, 0x75);
        t.ui.text_faint = rgb(0x93, 0xa1, 0xa1);
        t.ui.border = rgb(0xdc, 0xd5, 0xc0);
        t.ui.border_focus = rgb(0x26, 0x8b, 0xd2);
        t.ui.working = rgb(0xb5, 0x89, 0x00);
        t.ui.blocked = rgb(0xdc, 0x32, 0x2f);
        t.ui.done = rgb(0x85, 0x99, 0x00);
        t.ui.idle = rgb(0x93, 0xa1, 0xa1);
        t.ui.unknown = rgb(0xb0, 0xba, 0xba);
        t.ui.serving = rgb(0x2a, 0xa1, 0x98);
        t.ui.ok = rgb(0x85, 0x99, 0x00);
        t.ui.warn = rgb(0xb5, 0x89, 0x00);
        t.ui.error = rgb(0xdc, 0x32, 0x2f);
        t.ui.selection = rgb(0xe4, 0xdd, 0xc8);
        t.derive_syntax()
    }

    /// Follow the host terminal's own ANSI palette. Cell colors that are still "default"
    /// are passed through untouched so the pane looks exactly as it would outside horde.
    pub fn terminal() -> Theme {
        let mut t = Theme::horde();
        t.name = "terminal".into();
        t.inherit_terminal = true;
        t.derive_syntax()
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

    /// A theme compiled into horde. Never touches the disk.
    ///
    /// Separate from [`Theme::by_name`] because a theme file's `base` must resolve to a
    /// built-in: letting one file base itself on another is a cycle waiting to happen, and
    /// nobody has asked for it.
    pub fn builtin(name: &str) -> Option<Theme> {
        Some(match name {
            "horde" => Theme::horde(),
            "tokyo-night" => Theme::tokyo_night(),
            "catppuccin" => Theme::catppuccin(),
            "gruvbox" => Theme::gruvbox(),
            "nord" => Theme::nord(),
            "rose-pine" => Theme::rose_pine(),
            "rose-pine-dawn" => Theme::rose_pine_dawn(),
            "solarized-light" => Theme::solarized_light(),
            "terminal" => Theme::terminal(),
            _ => return None,
        })
    }

    /// A theme by name: a built-in, or one of the user's own from `themes/`.
    ///
    /// Built-ins win. Someone who writes `gruvbox.toml` gets the bundled gruvbox and a
    /// warning from `named`, rather than a theme that silently is not the one everyone else
    /// means by that word.
    pub fn by_name(name: &str) -> Option<Theme> {
        Theme::builtin(name).or_else(|| Theme::from_file(name).ok().flatten())
    }

    /// A theme by name, with a sentence to print when it does not load.
    ///
    /// `by_name` swallows a broken theme file as "not found", which is right for the places
    /// that only want a palette and wrong for config loading: a typo in a colour should say
    /// which line, not silently leave you on the default and let you wonder.
    pub fn load(name: &str) -> Result<Theme, String> {
        if let Some(t) = Theme::builtin(name) {
            return Ok(t);
        }
        match Theme::from_file(name) {
            Ok(Some(t)) => Ok(t),
            Err(e) => Err(e),
            Ok(None) => Err(format!(
                "unknown theme {name:?}; known themes: {}",
                Theme::names().join(", ")
            )),
        }
    }

    pub fn builtin_names() -> &'static [&'static str] {
        &[
            "horde",
            "tokyo-night",
            "catppuccin",
            "gruvbox",
            "nord",
            "rose-pine",
            "rose-pine-dawn",
            "solarized-light",
            "terminal",
        ]
    }

    /// Every theme that can be selected: the built-ins, then the user's own, alphabetically.
    ///
    /// Reads the directory each call rather than caching, because the settings page's theme
    /// picker is the obvious place to notice a theme you just wrote, and a cache would mean
    /// restarting horde to see it.
    pub fn names() -> Vec<String> {
        let mut out: Vec<String> = Theme::builtin_names().iter().map(|s| s.to_string()).collect();
        let Ok(dir) = std::fs::read_dir(crate::config::themes_dir()) else { return out };
        let mut mine: Vec<String> = dir
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                (path.extension()? == "toml").then(|| path.file_stem()?.to_str().map(String::from))?
            })
            .filter(|n| !Theme::builtin_names().contains(&n.as_str()))
            .collect();
        mine.sort();
        out.extend(mine);
        out
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
            border_focus, working, blocked, done, idle, unknown, serving, ok, warn, error,
            selection,
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
        // All sixteen or none: a table where four entries moved and twelve did not is not a
        // palette, it is two palettes fighting. A short or unparseable list leaves the base
        // theme's table alone rather than half-applying.
        if let Some(list) = &o.ansi {
            let parsed: Vec<Rgb> = list.iter().filter_map(|s| parse_color(s)).collect();
            if parsed.len() == 16 {
                self.ansi.copy_from_slice(&parsed);
            }
        }
        // The syntax palette is computed from `ui` and `ansi`, both of which may have just
        // moved. Without this a custom theme recolours everything except code.
        self.syntax = Syntax::from(&self.ui, &self.ansi);
    }

    /// Load a theme from `~/.config/horde/themes/<name>.toml`.
    ///
    /// Returns `Err` with something worth printing when the file is there but wrong, and
    /// `Ok(None)` when it simply is not there — the caller wants to tell those apart, because
    /// a typo in a theme name and a broken theme file need different sentences.
    pub fn from_file(name: &str) -> Result<Option<Theme>, String> {
        Theme::from_dir(&crate::config::themes_dir(), name)
    }

    /// [`Theme::from_file`], against a named directory.
    ///
    /// Split out so the loader can be tested against a temp directory rather than by setting
    /// `HORDE_CONFIG_DIR` -- tests share a process, and an env var one of them writes is an
    /// env var all the others read.
    pub fn from_dir(dir: &std::path::Path, name: &str) -> Result<Option<Theme>, String> {
        // A name is a filename. Anything with a separator in it is either a mistake or an
        // attempt to read a file somewhere else, and neither should be honoured.
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            return Err(format!("{name:?} is not a usable theme name"));
        }
        let path = dir.join(format!("{name}.toml"));
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let file: ThemeFile =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let base_name = file.base.as_deref().unwrap_or("horde");
        let mut theme = Theme::builtin(base_name)
            .ok_or_else(|| format!("{}: unknown base theme {base_name:?}", path.display()))?;
        theme.apply_overrides(&file.colors);
        // The file's own name, not the base's: this is a theme in its own right and has to
        // come back out of `[theme] name` as the thing that was asked for.
        theme.name = name.to_string();
        Ok(Some(theme))
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
    pub serving: Option<String>,
    pub ok: Option<String>,
    pub warn: Option<String>,
    pub error: Option<String>,
    pub selection: Option<String>,
    pub fg: Option<String>,
    pub cursor: Option<String>,
    /// ANSI 0-15, all sixteen or none.
    ///
    /// The chrome colours above only restyle what horde draws. These are what every program
    /// *inside* a pane paints with, so without them a custom theme repaints the borders and
    /// leaves vim looking like the theme you were trying to replace.
    ///
    /// All-or-nothing rather than sparse because a half-replaced ANSI table is the one way to
    /// get a palette that is neither of the two themes it came from.
    pub ansi: Option<Vec<String>>,
}

/// A theme read from `~/.config/horde/themes/<name>.toml`.
///
/// The same fields as `[theme.custom]` plus a `base` to start from, because "gruvbox but the
/// accent is orange" should be three lines rather than a full palette. Flattened so a theme
/// file is a flat list of colours with no section headers to remember.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeFile {
    /// The built-in this starts from. Defaults to `horde`.
    pub base: Option<String>,
    #[serde(flatten)]
    pub colors: ThemeOverrides,
}

/// A theme file to start editing, written out from a built-in.
///
/// Not a full dump. Every field horde reads would be ninety lines of hex, and the reader's
/// job then starts with working out which nine of them matter. `base` carries the rest, and
/// what is written here is the palette people actually reach for -- with the ANSI sixteen
/// commented out, because they are the ones you want when you want them and noise otherwise.
pub fn starter_file(t: &Theme) -> String {
    let hex = |c: Rgb| format!("\"#{:02x}{:02x}{:02x}\"", c.r, c.g, c.b);
    let mut out = String::new();
    out.push_str(&format!(
        "# A horde theme. Anything you leave out follows `base`, so this file can stay short.\n\
         # Colours may be \"#rgb\", \"#rrggbb\", \"rgb(r, g, b)\" or an ANSI name like \"cyan\".\n\
         base = \"{}\"\n\n",
        t.name
    ));
    let rows: [(&str, Rgb, &str); 12] = [
        ("accent", t.ui.accent, "focused borders, the active tab, a finished agent"),
        ("accent_alt", t.ui.accent_alt, "the second highlight, where two would collide"),
        ("bg", t.ui.bg, "the page behind panes, and the terminal background"),
        ("panel_bg", t.ui.panel_bg, "sidebar and drawers, a shade off `bg`"),
        ("text", t.ui.text, ""),
        ("text_dim", t.ui.text_dim, ""),
        ("text_faint", t.ui.text_faint, "comments, and anything you are meant to skip"),
        ("border", t.ui.border, ""),
        ("working", t.ui.working, "an agent mid-turn"),
        ("blocked", t.ui.blocked, "an agent waiting on you"),
        ("serving", t.ui.serving, "a dev server that is up"),
        ("selection", t.ui.selection, "on a light theme this should darken, not lighten"),
    ];
    for (name, color, note) in rows {
        let note = if note.is_empty() { String::new() } else { format!("   # {note}") };
        out.push_str(&format!("{name} = {}{note}\n", hex(color)));
    }
    out.push_str(
        "\n# The sixteen colours every program *inside* a pane paints with. All or nothing:\n\
         # a half-replaced table is two palettes fighting. Uncomment to take them over.\n",
    );
    out.push_str("# ansi = [\n");
    const SLOTS: [&str; 16] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        "bright black", "bright red", "bright green", "bright yellow",
        "bright blue", "bright magenta", "bright cyan", "bright white",
    ];
    for (i, c) in t.ansi.iter().enumerate() {
        out.push_str(&format!("#   {}, # {}\n", hex(*c), SLOTS[i]));
    }
    out.push_str("# ]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file `horde theme edit` writes has to be a file `horde` can read.
    ///
    /// These are two separate pieces of code -- a formatter and a serde struct -- and nothing
    /// but this holds them together. A `deny_unknown_fields` struct plus a generator that
    /// emits one key it does not know about means the first thing a new user does with the
    /// feature is hit an error.
    #[test]
    fn the_starter_file_round_trips_through_the_parser() {
        for name in Theme::builtin_names() {
            let base = Theme::builtin(name).unwrap();
            let text = starter_file(&base);
            let parsed: ThemeFile = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("{name}: the file we write does not parse: {e}\n{text}"));
            assert_eq!(parsed.base.as_deref(), Some(*name));

            // And the colours in it round-trip to the same theme, so an untouched copy is
            // genuinely the theme it says it is rather than a near-miss.
            let mut rebuilt = Theme::builtin(name).unwrap();
            rebuilt.apply_overrides(&parsed.colors);
            assert_eq!(rebuilt.ui.accent, base.ui.accent, "{name}");
            assert_eq!(rebuilt.ui.bg, base.ui.bg, "{name}");
            assert_eq!(rebuilt.ui.selection, base.ui.selection, "{name}");
        }
    }

    /// The ANSI block is commented out, so an untouched copy must leave the base's table
    /// alone rather than arriving as an empty list that wipes it.
    #[test]
    fn an_untouched_copy_keeps_the_base_ansi_table() {
        let base = Theme::gruvbox();
        let parsed: ThemeFile = toml::from_str(&starter_file(&base)).unwrap();
        assert!(parsed.colors.ansi.is_none(), "the ansi block should still be commented out");
        let mut rebuilt = Theme::gruvbox();
        rebuilt.apply_overrides(&parsed.colors);
        assert_eq!(rebuilt.ansi, base.ansi);
    }

    /// All sixteen or none. Four moved and twelve not is not a palette, it is two palettes.
    #[test]
    fn a_short_or_broken_ansi_list_leaves_the_table_alone() {
        let base = Theme::horde();
        for list in [
            vec!["#ff0000".to_string(); 15],
            vec!["#ff0000".to_string(); 17],
            {
                let mut v = vec!["#ff0000".to_string(); 16];
                v[3] = "not a colour".into();
                v
            },
        ] {
            let mut t = Theme::horde();
            t.apply_overrides(&ThemeOverrides { ansi: Some(list), ..Default::default() });
            assert_eq!(t.ansi, base.ansi, "a partial table was applied");
        }
        // ...and a complete one is.
        let mut t = Theme::horde();
        t.apply_overrides(&ThemeOverrides {
            ansi: Some(vec!["#ff0000".to_string(); 16]),
            ..Default::default()
        });
        assert_eq!(t.ansi[0], Rgb::new(255, 0, 0));
        // The syntax palette is built from `ansi` and `ui`, so it has to have been recomputed.
        assert_eq!(t.syntax.type_name, Rgb::new(255, 0, 0), "syntax still on the old table");
    }

    /// The whole point, end to end: three lines on disk become a theme.
    #[test]
    fn a_three_line_theme_file_inherits_everything_it_does_not_say() {
        let dir = std::env::temp_dir().join(format!("horde-theme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mine.toml"),
            "base = \"gruvbox\"\naccent = \"#ff8800\"\n",
        )
        .unwrap();

        let t = Theme::from_dir(&dir, "mine").unwrap().expect("the file is there");
        assert_eq!(t.name, "mine", "a theme file is a theme in its own right");
        assert_eq!(t.ui.accent, Rgb::new(0xff, 0x88, 0x00), "the one line it did say");
        // ...and everything it did not.
        let gruvbox = Theme::gruvbox();
        assert_eq!(t.ansi, gruvbox.ansi, "the ANSI table should still be gruvbox's");
        assert_eq!(t.ui.panel_bg, gruvbox.ui.panel_bg);
        // Syntax follows the accent, because keywords are painted with it.
        assert_eq!(t.syntax.keyword, Rgb::new(0xff, 0x88, 0x00));

        // A file that is not there is not an error; a file that is there and wrong is.
        assert!(Theme::from_dir(&dir, "absent").unwrap().is_none());
        std::fs::write(dir.join("bad.toml"), "base = \"nope\"\n").unwrap();
        assert!(Theme::from_dir(&dir, "bad").is_err(), "an unknown base must say so");
        std::fs::write(dir.join("junk.toml"), "accent = [1, 2\n").unwrap();
        assert!(Theme::from_dir(&dir, "junk").is_err(), "broken TOML must say so");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A built-in always wins, so nobody's `gruvbox.toml` quietly redefines what everyone
    /// else means by the word.
    #[test]
    fn a_user_theme_cannot_shadow_a_builtin() {
        assert_eq!(Theme::by_name("gruvbox").unwrap().ui.accent, Theme::gruvbox().ui.accent);
        for n in Theme::builtin_names() {
            assert!(Theme::builtin(n).is_some(), "{n} is listed but does not build");
        }
    }

    /// A theme name is a filename, and a filename with a separator in it is either a mistake
    /// or an attempt to read something that is not a theme.
    #[test]
    fn a_theme_name_cannot_escape_the_themes_directory() {
        for bad in ["../config", "a/b", "", ".ssh/id_rsa"] {
            assert!(Theme::from_file(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// Both light themes have to actually be light, or they are a dark theme with a typo.
    /// The chrome is checked together because a light page with dark panels beside it is the
    /// classic half-converted theme.
    #[test]
    fn the_light_themes_are_light_all_the_way_through() {
        let lum =
            |c: Rgb| (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0;
        for name in ["rose-pine-dawn", "solarized-light"] {
            let t = Theme::builtin(name).unwrap();
            assert!(lum(t.ui.bg) > 0.8, "{name}: the page is not light ({:.2})", lum(t.ui.bg));
            assert!(lum(t.ui.panel_bg) > 0.8, "{name}: the panels are dark beside a light page");
            assert!(lum(t.ui.text) < 0.5, "{name}: the text is not dark");
            // On paper a highlight is a shadow. A selection lighter than the page it sits on
            // is invisible, which is the single most common way a light theme is wrong.
            assert!(
                lum(t.ui.selection) < lum(t.ui.bg),
                "{name}: the selection is lighter than the page"
            );
            // Faint text still has to be readable, which is where inverted dark themes fail.
            assert!(
                lum(t.ui.bg) - lum(t.ui.text_faint) > 0.15,
                "{name}: faint text vanishes into the page"
            );
        }
    }

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
            assert_eq!(Theme::by_name(&n).unwrap_or_else(|| panic!("{n}")).name, n);
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
            let t = Theme::by_name(&name).unwrap();
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
