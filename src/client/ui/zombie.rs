//! Something crosses the wordmark on the start screen, now and then.
//!
//! horde already reaps zombies in the Unix sense — see `daemon::pty` — so the greeter has
//! one of the other kind. It shambles in front of the letters, occludes them as it passes,
//! and is gone again for about a minute.
//!
//! Two things live here, and they are deliberately separate. The **schedule** is a pure
//! function of elapsed seconds: when a crossing starts, how far into one we are, and whether
//! anything is moving at all. The **figure** is pixel art blitted through [`super::sprite`].
//! Neither knows about the client's loop; the loop asks the schedule whether there is
//! anything to draw, and the renderer asks for a frame. That split is what lets the whole
//! feature be argued about in numbers rather than by staring at a terminal.
//!
//! Motion is a function of time rather than of frames, following the graph's drift: the
//! shamble looks the same however fast the client happens to be redrawing, and an extra
//! frame forced by a keypress lands the figure exactly where the clock already had it.

#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;

use super::sprite::{blit, Palette, Sprite};
use crate::proto::Rgb;
use crate::theme::{mix, Theme};

// ---------------------------------------------------------------------------
// The schedule
// ---------------------------------------------------------------------------

/// Seconds one crossing takes, entrance to exit.
///
/// A duration rather than a speed, so a narrower wordmark is crossed more slowly instead of
/// more briefly — the greeter's mood should not depend on how many projects you have open.
const CROSS: f64 = 25.0;

/// The slot one crossing lives in.
///
/// Fixed length on purpose: "which crossing is this" is then a division rather than a sum
/// over every crossing since the screen opened, so a start screen left up overnight answers
/// as quickly as one opened a second ago.
const CYCLE: f64 = 85.0;

/// Stillness guaranteed at the head of every slot, so two crossings whose jitter lands them
/// either side of a boundary still have a pause between them.
const REST: f64 = 25.0;

/// How far into its slot a crossing may be nudged.
const SPAN: f64 = CYCLE - CROSS - REST;

/// How long one drawing of the figure holds.
///
/// Three ticks of the client's animation beat, so each pose lasts the same number of
/// redraws rather than alternating two and three and wobbling.
const POSE: f64 = 0.33;

/// What the start screen is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// Nothing is moving, and the next crossing is this many seconds away.
    Still { next: f64 },
    /// A crossing is under way, this many seconds into it.
    Walking { at: f64 },
}

impl Phase {
    pub fn walking(self) -> bool {
        matches!(self, Phase::Walking { .. })
    }

    /// Seconds into the crossing, which is the renderer's whole input.
    pub fn at(self) -> Option<f64> {
        match self {
            Phase::Walking { at } => Some(at),
            Phase::Still { .. } => None,
        }
    }
}

/// A deterministic `0..1` from a slot number.
///
/// Not a random number generator, and deliberately not the `rand` crate: the schedule has to
/// be reproducible, because a test you have to run a hundred times to see fail is not a test.
/// SplitMix64's finaliser is four lines and mixes well enough that consecutive slots do not
/// rhyme.
fn jitter(seed: u64, slot: u64) -> f64 {
    let mut z = seed.wrapping_add(slot.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// When the crossing in a slot begins, in seconds since the screen opened.
///
/// The first one is on arrival rather than a minute into it. Opening the start screen is the
/// moment somebody is actually looking at it, and a greeter that waits until you have already
/// left to do its one trick is a greeter nobody ever sees. Every crossing after that is on
/// the jittered schedule.
fn start(seed: u64, slot: u64) -> f64 {
    if slot == 0 {
        return 0.0;
    }
    slot as f64 * CYCLE + REST + jitter(seed, slot) * SPAN
}

/// The whole state machine: elapsed seconds in, what the screen is doing out.
pub fn phase_at(seed: u64, elapsed: f64) -> Phase {
    let slot = (elapsed / CYCLE).max(0.0) as u64;
    // The slot before this one is checked too, so a crossing that began near the end of its
    // slot is still walking after the boundary rather than blinking out of existence.
    for s in [slot.saturating_sub(1), slot] {
        let at = elapsed - start(seed, s);
        if (0.0..CROSS).contains(&at) {
            return Phase::Walking { at };
        }
    }
    let next = if elapsed < start(seed, slot) { slot } else { slot + 1 };
    Phase::Still { next: start(seed, next) - elapsed }
}

/// A walk in progress: when its clock started, and the seed its schedule is jittered by.
#[derive(Debug, Clone, Copy)]
pub struct Walk {
    since: Instant,
    seed: u64,
}

impl Walk {
    /// Start the clock now, with a seed nobody can predict and nobody needs to.
    pub fn new() -> Walk {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5EED);
        Walk { since: Instant::now(), seed }
    }

    /// A walk with a known schedule, already this far along. For tests: everything about
    /// this feature is a function of these two numbers.
    #[cfg(test)]
    pub fn seeded(seed: u64, elapsed: Duration) -> Walk {
        let since = Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now);
        Walk { since, seed }
    }

    pub fn phase(&self) -> Phase {
        phase_at(self.seed, self.since.elapsed().as_secs_f64())
    }

    /// When the clock started, so a test can prove a keypress did not restart it.
    #[cfg(test)]
    pub fn since(&self) -> Instant {
        self.since
    }
}

impl Default for Walk {
    fn default() -> Walk {
        Walk::new()
    }
}

// ---------------------------------------------------------------------------
// The figure
// ---------------------------------------------------------------------------

/// One figure, at one size: the poses it cycles through.
pub struct Cast {
    frames: &'static [Sprite],
}

/// The letters the art is drawn with.
///
/// `%` and `x` are both dark and deliberately distinct: `%` is the figure receding, `x` is a
/// hole punched through it. Collapse them and the sockets stop being sunken.
///
/// | `.` | transparent — the wordmark shows through |
/// | `#` | rotting flesh, mid tone |
/// | `%` | flesh in shadow: undersides, socket rims, the far arm |
/// | `o` | bone: teeth, exposed ribs, a bare foot |
/// | `x` | a hole, not a shadow: eye sockets, the gaps between ribs |
/// | `c` | the torn shirt |
/// | `C` | shirt in shadow, and the one remaining boot |
/// | `b` | blood, dark and clotted, a few pixels only |
/// | `e` | the one hot pixel, in the far socket |
/// | `h` | matted scalp |
#[cfg(test)]
const LETTERS: &str = "#%oxcCbeh";

/// Nine colours, derived rather than literal, so the figure is a rotting version of whatever
/// palette is up rather than a green sticker pasted on top of one.
///
/// Everything hangs off which of the theme's two poles is the lighter — that one decision is
/// what makes a light theme work, because every push toward shadow becomes a push toward the
/// theme's own ink rather than toward black, with no second code path.
pub fn palette(t: &Theme) -> Palette {
    let lum = |c: Rgb| (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0;
    let (lit, dark) =
        if lum(t.ui.bg) > lum(t.ui.text) { (t.ui.bg, t.ui.text) } else { (t.ui.text, t.ui.bg) };
    // The pole that reads *against* the background — pale ink on a dark theme, dark ink on a
    // light one. Everything the figure is made of leans on this; shadow always leans on
    // `dark`. That is the whole light-theme story, and there is no second code path.
    let ink = if lum(t.ui.bg) > 0.5 { dark } else { lit };

    // `ok` is green in every bundled theme and is already guaranteed to be legible on the
    // background. Pulled toward the ink and then slightly back toward the surface, it stops
    // reading as a health indicator and starts reading as something that died a while ago.
    let skin = mix(mix(t.ui.ok, ink, 0.3), t.ui.bg, 0.2);
    let cloth = separate(mix(t.ui.text_faint, ink, 0.2), skin, dark, lit, 0.06);
    Palette::new(
        t.ui.bg,
        vec![
            ('#', skin),
            ('%', mix(skin, dark, 0.45)),
            ('o', mix(ink, t.ui.warn, 0.3)),
            // Darker than anything the theme owns, so a socket is a hole on any palette —
            // but lifted slightly off pure shadow, or the head looks bitten wherever it
            // overhangs the letters into empty space.
            ('x', mix(mix(dark, Rgb::new(0, 0, 0), 0.35), skin, 0.12)),
            ('c', cloth),
            ('C', mix(cloth, dark, 0.5)),
            ('b', mix(t.ui.error, dark, 0.3)),
            // Amber rather than red: red is the blocked-agent colour, and a red pinprick on
            // the greeter reads as an alert.
            ('e', mix(t.ui.warn, ink, 0.2)),
            ('h', mix(t.ui.border, ink, 0.25)),
        ],
    )
}

/// Push `c` away from `from` until their luminance differs by at least `want`.
///
/// The figure is built out of two theme colours -- `ok` for flesh, `text_faint` for cloth --
/// that no theme promises to keep apart. Most keep them apart by accident; solarized-light
/// puts an olive green and a warm grey within a hundredth of each other, and the zombie comes
/// out a single-coloured blob. A theme somebody writes themselves has nothing checking it at
/// all, so this guarantees the gap rather than hoping for it.
///
/// Away from whichever pole is further, so the nudge never walks the colour into the
/// background it also has to stay legible against.
fn separate(c: Rgb, from: Rgb, dark: Rgb, lit: Rgb, want: f32) -> Rgb {
    let lum = |c: Rgb| (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0;
    let gap = lum(c) - lum(from);
    if gap.abs() >= want {
        return c;
    }
    // Toward the pole `from` is furthest from, so the two move apart rather than together.
    let toward = if (lum(from) - lum(dark)).abs() > (lum(from) - lum(lit)).abs() { dark } else { lit };
    // Enough to clear `want` with room over, then stop: this is a legibility floor, not a
    // restyle, and a colour dragged all the way to a pole is no longer the theme's.
    let mut out = c;
    for step in 1..=8 {
        out = mix(c, toward, step as f32 * 0.08);
        if (lum(out) - lum(from)).abs() >= want {
            break;
        }
    }
    out
}

/// The tall figure: 14 × 20 pixels, which is 14 columns by 10 rows of cells.
///
/// The head is a third of the height, which is wrong anatomically and right at this size —
/// realistic proportions give you a three-pixel skull with no room for sockets, and the head
/// is what carries the whole character.
#[rustfmt::skip]
static TALL: Cast = Cast {
    frames: &[
        // Stance: good leg planted forward, the dead one trailing far behind.
        Sprite { rows: &[
            "..hhhhh.......",
            ".hh####%......",
            ".h#####%......",
            ".#xx%xx%......",
            ".%%x%%e%......",
            ".#%%x##%......",
            "..oxoxo%......",
            "...%##%.......",
            "..CCCccc%CCC%.",
            "..cCcccc%%##b#",
            "..cCoooo%.....",
            "..cCxxxx%%%#%.",
            "..cCoooo%b###b",
            "..cCCxxc%.....",
            "...ccccc......",
            "...cc.cc%.....",
            "...##..cc%....",
            "...##...##....",
            "..o##...%%....",
            ".ooo....CC....",
        ] },
        // The scrape: the dead foot hauled a column closer, everything else holding.
        Sprite { rows: &[
            "..hhhhh.......",
            ".hh####%......",
            ".h#####%......",
            ".#xx%xx%......",
            ".%%x%%e%......",
            ".#%%x##%......",
            "..oxoxo%......",
            "...%##%.......",
            "..CCCccc%CCC%.",
            "..cCcccc%%##b#",
            "..cCoooo%.....",
            "..cCxxxx%%%#%.",
            "..cCoooo%b###b",
            "..cCCxxc%.....",
            "...ccccc......",
            "...cc.cc%.....",
            "...##..cc%....",
            "...##...##....",
            "...##...%%....",
            "..ooo...CC....",
        ] },
        // The swing: body dipped a pixel and pitched forward, head lolled, boot lifted.
        // Row zero is empty — the box is sized to the tallest pose and the bob is the
        // difference.
        Sprite { rows: &[
            "..............",
            "...hhhhh......",
            "..hh####%.....",
            "..h#####%.....",
            "..#xx%xx%.....",
            "..%%x%%e%.....",
            "..#%%x##%.....",
            "...oxoxo%.....",
            "...%##%%......",
            "..CCCccc%%CC%.",
            "..cCcccc%.%#b#",
            "..cCoooo%.....",
            "..cCxxxx%%%%..",
            "..cCoooo%b###b",
            "..cCCxxc%.....",
            "...ccccc......",
            "....##.cc%....",
            "....##..cc....",
            "...o##...CC...",
            "..ooo.........",
        ] },
        // The heel strike: weight going onto the front foot, the dead one left behind.
        Sprite { rows: &[
            "..............",
            "...hhhhh......",
            "..hh####%.....",
            "..h#####%.....",
            "..#xx%xx%.....",
            "..%%x%%e%.....",
            "..#%%x##%.....",
            "...oxoxo%.....",
            "...%##%%......",
            "..CCCccc%%CC%.",
            "..cCcccc%.%#b#",
            "..cCoooo%.....",
            "..cCxxxx%%%%..",
            "..cCoooo%b###b",
            "..cCCxxc%.....",
            "...ccccc......",
            "...##...cc%...",
            "...##....cc...",
            "..o##.....CC..",
            ".ooo..........",
        ] },
    ],
};

/// The short figure: 14 × 12 pixels, six cell rows.
///
/// Not the tall one squashed. Halving the height and keeping the width buys the detail back
/// by turning the figure side-on and hunching it: the head is thrust forward at shoulder
/// height, the back arches, and the ribcage is what you see. Horizontal pixels are the cheap
/// ones.
#[rustfmt::skip]
static SHORT: Cast = Cast {
    frames: &[
        Sprite { rows: &[
            "......hhh.....",
            ".....h####%...",
            "..CC%#xx%x#%..",
            ".CCcc#%%x%o%..",
            ".cCccc%oxo%...",
            ".cCcoo%%......",
            ".ccxxc%##b##..",
            "..ccooc%......",
            "..cc.cc%......",
            "..##..cc......",
            "..##...%%.....",
            ".ooo....CC....",
        ] },
        Sprite { rows: &[
            "......hhh.....",
            ".....h####%...",
            "..CC%#xx%x#%..",
            ".CCcc#%%x%o%..",
            ".cCccc%oxo%...",
            ".cCcoo%%......",
            ".ccxxc%##b##..",
            "..ccooc%......",
            "..cc.cc%......",
            "..##..cc......",
            "..##...%%.....",
            "..ooo...CC....",
        ] },
        Sprite { rows: &[
            "..............",
            ".....hh####...",
            "..CCc#xx%x#%..",
            ".CCcc%%%x%o%..",
            ".cCccc%oxo%...",
            ".cCcoo%%%.....",
            ".ccxxc%%##b#..",
            "..ccooc%......",
            "...cccc%......",
            "...##.##......",
            "...##..CC.....",
            "..ooo.........",
        ] },
        Sprite { rows: &[
            "..............",
            ".....hh####...",
            "..CCc#xx%x#%..",
            ".CCcc%%%x%o%..",
            ".cCccc%oxo%...",
            ".cCcoo%%%.....",
            ".ccxxc%%##b#..",
            "..ccooc%......",
            "...cccc%......",
            "..##...cc.....",
            "..##....CC....",
            ".ooo..........",
        ] },
    ],
};

/// Which figure a stage can hold, if any.
///
/// Keyed off the height the caller actually gave us rather than off the terminal size: the
/// stage is as tall as the banner that got drawn, so this cannot disagree with the wordmark
/// about which one is up.
fn cast_for(stage: TRect) -> Option<&'static Cast> {
    let cast = match stage.height {
        h if h >= 10 => &TALL,
        h if h >= 6 => &SHORT,
        // The one-line wordmark fallback. A greeter already short of room has none to
        // spare for a passer-by, and three rows will not hold a face.
        _ => return None,
    };
    // Room to enter and leave, or it is a jump rather than a walk.
    (stage.width >= cast.frames[0].width() + 6).then_some(cast)
}

/// Draw the crossing. `at` is seconds into it, as [`Phase::at`] reports.
///
/// Returns whether anything was drawn, so a caller that needs to know if the screen is
/// moving does not have to ask the geometry a second time.
pub fn draw(buf: &mut Buffer, stage: TRect, t: &Theme, at: f64) -> bool {
    let Some(cast) = cast_for(stage) else { return false };
    let sprite = &cast.frames[(at.max(0.0) / POSE) as usize % cast.frames.len()];

    // Enters fully off one side and leaves fully off the other, so it is never seen to
    // appear or to stop.
    let w = f64::from(sprite.width());
    let travel = f64::from(stage.width) + w;
    let x = (-w + travel * (at / CROSS).clamp(0.0, 1.0)).round() as i32;
    // Feet on the bottom row of the stage, whatever height the art is.
    let y = i32::from(stage.height) * 2 - i32::from(sprite.height());

    blit(buf, stage, sprite, x, y, &palette(t));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn casts() -> [(&'static str, &'static Cast); 2] {
        [("tall", &TALL), ("short", &SHORT)]
    }

    /// Ragged art is a bug you see as a torn edge rather than as an error, so it is checked
    /// here — the same reason `logo` asserts its banners are rectangular.
    #[test]
    fn every_frame_is_rectangular_and_drawn_only_in_palette_letters() {
        for (name, cast) in casts() {
            let (w, h) = (cast.frames[0].width(), cast.frames[0].height());
            for (i, f) in cast.frames.iter().enumerate() {
                assert_eq!(f.height(), h, "{name} frame {i} is a different height");
                for (r, row) in f.rows.iter().enumerate() {
                    assert_eq!(
                        row.chars().count() as u16,
                        w,
                        "{name} frame {i} row {r} is {:?}",
                        row
                    );
                    for c in row.chars() {
                        assert!(
                            c == '.' || LETTERS.contains(c),
                            "{name} frame {i} row {r} uses {c:?}, which is not a palette slot"
                        );
                    }
                }
            }
        }
    }

    /// A figure with nothing on the bottom row is a figure floating above the letters.
    #[test]
    fn every_frame_stands_on_something() {
        for (name, cast) in casts() {
            for (i, f) in cast.frames.iter().enumerate() {
                let ground = f.rows[f.rows.len() - 1];
                assert!(ground.chars().any(|c| c != '.'), "{name} frame {i} has no foot down");
            }
        }
    }

    /// The two sizes are hand-drawn rather than one scaled, so they are allowed to differ in
    /// everything but the width they are authored to.
    #[test]
    fn the_two_figures_are_the_sizes_the_stages_offer() {
        assert_eq!((TALL.frames[0].width(), TALL.frames[0].height()), (14, 20));
        assert_eq!((SHORT.frames[0].width(), SHORT.frames[0].height()), (14, 12));
        // Ten cell rows is twenty pixels; six is twelve. Both fit their stage exactly.
        assert_eq!(cast_for(TRect::new(0, 0, 72, 10)).map(|c| c.frames[0].height()), Some(20));
        assert_eq!(cast_for(TRect::new(0, 0, 72, 7)).map(|c| c.frames[0].height()), Some(12));
    }

    /// The one-line wordmark has no stage, and a narrow one is a jump rather than a walk.
    #[test]
    fn a_stage_too_small_holds_nobody() {
        assert!(cast_for(TRect::new(0, 0, 72, 3)).is_none(), "the plain-word fallback");
        assert!(cast_for(TRect::new(0, 0, 72, 0)).is_none(), "no banner at all");
        assert!(cast_for(TRect::new(0, 0, 18, 10)).is_none(), "too narrow to cross");
    }

    /// A crossing lasts what it says it lasts, and the rest of the time nothing moves.
    #[test]
    fn a_crossing_takes_twenty_five_seconds_and_the_rest_is_still() {
        let (mut walking, mut total) = (0, 0);
        let mut t = 0.0;
        while t < 3600.0 {
            if phase_at(7, t).walking() {
                walking += 1;
            }
            total += 1;
            t += 0.1;
        }
        let duty = walking as f64 / total as f64;
        let want = CROSS / CYCLE;
        assert!((duty - want).abs() < 0.01, "moving {duty:.3} of the time, wanted {want:.3}");
    }

    /// The phase runs the whole width of a crossing and never restarts inside one.
    #[test]
    fn the_phase_runs_from_nothing_to_the_full_crossing() {
        let (mut seen_start, mut seen_end, mut last) = (false, false, -1.0f64);
        let mut t = 0.0;
        while t < CYCLE * 2.0 {
            if let Phase::Walking { at } = phase_at(3, t) {
                if last >= 0.0 && at > last {
                    assert!(at - last < 0.2, "the phase jumped from {last} to {at}");
                }
                seen_start |= at < 0.1;
                seen_end |= at > CROSS - 0.2;
                last = at;
            } else {
                last = -1.0;
            }
            t += 0.05;
        }
        assert!(seen_start && seen_end, "a crossing must run end to end");
    }

    /// Arriving is the moment somebody is looking, so the first crossing is on arrival — it
    /// enters from off stage at once rather than a minute later, whatever the seed.
    #[test]
    fn the_first_crossing_begins_the_moment_the_screen_opens() {
        for seed in [0, 1, 42, u64::MAX] {
            assert_eq!(phase_at(seed, 0.0), Phase::Walking { at: 0.0 }, "seed {seed}");
            assert!(phase_at(seed, CROSS / 2.0).walking(), "seed {seed}: gone already");
            assert!(!phase_at(seed, CROSS + 1.0).walking(), "seed {seed}: never stopped");
        }
        // And what follows is the ordinary jittered schedule, not another arrival.
        assert!(start(7, 1) >= CYCLE, "the second crossing is a whole slot away");
    }

    /// Crossings must not run into each other, whatever the constants are retuned to.
    #[test]
    fn no_two_crossings_overlap_and_every_gap_is_a_real_pause() {
        for seed in [0, 1, 42, u64::MAX] {
            for slot in 0..2000u64 {
                let (a, b) = (start(seed, slot), start(seed, slot + 1));
                assert!(a + CROSS <= b, "seed {seed} slot {slot}: {a} + {CROSS} > {b}");
                let gap = b - (a + CROSS);
                assert!(gap >= REST - SPAN, "seed {seed} slot {slot}: only {gap}s of stillness");
            }
        }
    }

    /// A jitter that got stuck returning a constant would pass every other test here.
    #[test]
    fn the_jitter_is_spread_and_seed_dependent() {
        let mut buckets = [0usize; 10];
        for slot in 0..1000 {
            let j = jitter(9, slot);
            assert!((0.0..1.0).contains(&j), "{j} is not a fraction");
            buckets[(j * 10.0) as usize] += 1;
        }
        assert!(buckets.iter().all(|&n| n > 40), "lumpy jitter: {buckets:?}");
        assert!(
            (0..10).any(|s| start(1, s) != start(2, s)),
            "two seeds must not share a schedule"
        );
    }

    /// Stillness is stillness: the renderer is never handed a zero-length walk to draw.
    #[test]
    fn a_still_screen_has_no_phase_to_draw() {
        let mut t = 0.0;
        while t < CYCLE {
            let p = phase_at(5, t);
            assert_eq!(p.walking(), p.at().is_some(), "at {t}: {p:?}");
            t += 0.25;
        }
    }

    /// The figure enters from off stage and leaves off the other side, rather than appearing
    /// and stopping.
    #[test]
    fn the_walk_starts_and_ends_out_of_sight() {
        let theme = Theme::horde();
        let stage = TRect::new(0, 0, 72, 10);
        let painted = |at: f64| {
            let mut buf = Buffer::empty(stage);
            assert!(draw(&mut buf, stage, &theme, at));
            (0..stage.width)
                .filter(|x| (0..stage.height).any(|y| buf[(*x, y)].symbol() != " "))
                .count()
        };
        assert_eq!(painted(0.0), 0, "still off stage on the left");
        assert_eq!(painted(CROSS), 0, "and gone off the right");
        assert!(painted(CROSS / 2.0) > 8, "and solidly on screen in between");
    }

    /// It has to actually move, and it has to keep its feet on the ground while it does.
    #[test]
    fn the_figure_crosses_the_stage_with_its_feet_on_the_floor() {
        let theme = Theme::horde();
        let stage = TRect::new(0, 0, 72, 10);
        let left_edge = |at: f64| {
            let mut buf = Buffer::empty(stage);
            draw(&mut buf, stage, &theme, at);
            (0..stage.width).find(|x| (0..stage.height).any(|y| buf[(*x, y)].symbol() != " "))
        };
        let (a, b) = (left_edge(6.0).unwrap(), left_edge(18.0).unwrap());
        assert!(b > a, "it walked backwards: {a} -> {b}");

        // Whatever the pose, the bottom row of the stage is where the feet are.
        for at in [6.0, 9.0, 12.0, 15.0] {
            let mut buf = Buffer::empty(stage);
            draw(&mut buf, stage, &theme, at);
            let floor = stage.height - 1;
            assert!(
                (0..stage.width).any(|x| buf[(x, floor)].symbol() != " "),
                "nothing on the floor at {at}s — it is hovering"
            );
        }
    }

    /// Every slot has to stay legible on every theme, including a light one, since the
    /// `terminal` theme inherits whatever the user's palette happens to be.
    #[test]
    fn the_palette_reads_on_every_theme() {
        let lum = |c: Rgb| (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0;
        let mut light = Theme::horde();
        light.ui.bg = Rgb::new(0xfa, 0xfa, 0xfa);
        light.ui.text = Rgb::new(0x20, 0x20, 0x20);
        let themes: Vec<Theme> =
            Theme::names().iter().filter_map(|n| Theme::by_name(n)).chain([light]).collect();

        for t in &themes {
            let pal = palette(t);
            for slot in ['#', 'o', 'c', 'e'] {
                let c = pal.get(slot).unwrap_or_else(|| panic!("{slot} has no colour"));
                let d = (lum(c) - lum(t.ui.bg)).abs();
                assert!(d >= 0.10, "{}: {slot} is invisible on the background ({d:.3})", t.name);
            }
            // Flesh, its shadow and the shirt have to be told apart, or the figure is a blob.
            let (skin, shade, cloth) =
                (pal.get('#').unwrap(), pal.get('%').unwrap(), pal.get('c').unwrap());
            assert!((lum(skin) - lum(shade)).abs() >= 0.04, "{}: flat flesh", t.name);
            assert!((lum(skin) - lum(cloth)).abs() >= 0.04, "{}: flesh reads as shirt", t.name);
        }
    }

    /// Print the art, in colour, every pose side by side.
    ///
    /// `cargo test zombie -- --nocapture` is how a walk cycle actually gets reviewed:
    /// rebuilding horde and waiting up to a minute for a crossing is no way to iterate on a
    /// drawing. The escape codes are emitted rather than the symbols alone because colour is
    /// most of what the figure is made of — a half-block dump in mono is a smudge.
    #[test]
    fn the_frames_print() {
        let theme = Theme::horde();
        for (name, cast) in casts() {
            let f = &cast.frames[0];
            let (w, rows) = (f.width() + 2, f.height().div_ceil(2));
            println!(
                "\n  {name} — {} poses, {}x{} pixels",
                cast.frames.len(),
                f.width(),
                f.height()
            );
            let area = TRect::new(0, 0, w * cast.frames.len() as u16, rows);
            let mut buf = Buffer::empty(area);
            for (i, frame) in cast.frames.iter().enumerate() {
                blit(&mut buf, area, frame, i as i32 * i32::from(w) + 1, 0, &palette(&theme));
            }
            for y in 0..area.height {
                let mut line = String::new();
                for x in 0..area.width {
                    let st = buf[(x, y)].style();
                    let esc = |c: Option<ratatui::style::Color>, base: u8| match c {
                        Some(ratatui::style::Color::Rgb(r, g, b)) => {
                            format!("\x1b[{base};2;{r};{g};{b}m")
                        }
                        _ => format!("\x1b[{base};2;17;20;26m"),
                    };
                    line.push_str(&esc(st.fg, 38));
                    line.push_str(&esc(st.bg, 48));
                    line.push_str(buf[(x, y)].symbol());
                }
                println!("  {line}\x1b[0m");
            }
        }
    }
}
