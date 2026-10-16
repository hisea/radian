//! The spinner (ADR 0027/9/10/13): one definition for every running indicator.
//!
//! The frame is a function of **time**, not of the text length (the old
//! `SPINNER[text.len() % 4]` turned once per token). `pulse` is the default: a `⏺` that keeps
//! its glyph, position and width and only changes colour. Since revision 13 it **breathes**:
//! one period of [`PULSE_PERIOD`] sampled [`PULSE_STEPS`] times along a sine, from the dim end to
//! the accent and back, so the colour changes slowest at both extremes. Where it cannot
//! interpolate — either end of the breath is not RGB (16/256-colour themes, `mono`), or the
//! `ascii` glyph set — it keeps revision 10's two-colour switch every 500 ms. The rotating kinds
//! turn every 250 ms. The glyph set decides the glyphs: `ascii` uses `| / - \` (and `*` for
//! pulse).

use std::time::Duration;

use ratatui::style::Color;

use crate::footer::Glyphs;

/// Which spinner the user configured (`tui.spinner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// A `⏺` that breathes between the accent and the dim level (the default, revisions 10/13).
    #[default]
    Pulse,
    /// `◜ ◝ ◞ ◟` (revision 2).
    Arc,
    /// `▖ ▘ ▝ ▗` (the quadrant blocks; every checked font has them).
    Quadrant,
    /// The braille frames.
    Braille,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "pulse" => Some(Self::Pulse),
            "arc" => Some(Self::Arc),
            "quadrant" => Some(Self::Quadrant),
            "braille" => Some(Self::Braille),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Arc => "arc",
            Self::Quadrant => "quadrant",
            Self::Braille => "braille",
        }
    }

    /// The frame interval when the pulse cannot breathe (revision 10's two-colour switch) and the
    /// rotating frames' own interval. [`frame_interval`] is the one the loop uses: it asks this
    /// only when the pulse cannot interpolate.
    pub fn interval(self) -> Duration {
        match self {
            Self::Pulse => PULSE_SWITCH,
            Self::Arc | Self::Quadrant | Self::Braille => ROTATING_INTERVAL,
        }
    }
}

/// One breath of the default pulse (ADR 0027).
pub const PULSE_PERIOD: Duration = Duration::from_millis(1_400);
/// The colours in one breath (revision 13): the sine below is sampled this many times, so the
/// breath lands on [`PULSE_STEPS`] distinct values from the dim end to the accent and back.
pub const PULSE_STEPS: u8 = 12;
/// Revision 10's two-colour switch, kept for a pulse that cannot interpolate (revision 13
/// decision 3).
pub const PULSE_SWITCH: Duration = Duration::from_millis(500);
/// One turn of the rotating kinds (revisions 2/9).
pub const ROTATING_INTERVAL: Duration = Duration::from_millis(250);

/// The colour end of the breath: the accent, and the brightness level of every rotating frame.
pub const PULSE_ACCENT_STEP: u8 = PULSE_STEPS;
/// The dim end of the breath: the level revision 10 switched to.
pub const PULSE_DIM_STEP: u8 = 0;

/// One step of the breath ([`PULSE_PERIOD`] over [`PULSE_STEPS`]: ~117 ms).
pub fn pulse_step() -> Duration {
    PULSE_PERIOD / u32::from(PULSE_STEPS)
}

/// One frame: the glyph and how bright the pulse is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub glyph: &'static str,
    /// The pulse's brightness in `0..=PULSE_STEPS` (revision 13): [`PULSE_DIM_STEP`] is the dim
    /// end, [`PULSE_ACCENT_STEP`] the accent. A pulse that cannot interpolate uses only those two
    /// — revision 10's switch — and the rotating kinds are always at the accent.
    pub step: u8,
}

impl Default for Frame {
    /// The frame for a non-animated path (line counting, search, copy): the glyph is
    /// decoration there, so the default is the pulse's first frame.
    fn default() -> Self {
        Self {
            glyph: "⏺",
            step: PULSE_ACCENT_STEP,
        }
    }
}

/// The `arc` frames (revision 2). Not every checked font has them (ADR 0027's
/// documented exception), so `quadrant` is the covered alternative.
pub const ARC: [&str; 4] = ["◜", "◝", "◞", "◟"];
/// The `quadrant` frames: every checked font has them.
pub const QUADRANT: [&str; 4] = ["▖", "▘", "▝", "▗"];
/// The `braille` frames.
pub const BRAILLE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The `ascii` frames for every rotating kind.
pub const ASCII: [&str; 4] = ["|", "/", "-", "\\"];

/// Whether the pulse interpolates (revision 13 decision 3): the `ascii` `*` keeps revision 10's
/// two-colour switch, and `rgb` is whether both ends of the breath are RGB
/// ([`crate::theme::Theme::pulse_can_breathe`]).
pub fn breathes(kind: Kind, glyphs: Glyphs, rgb: bool) -> bool {
    kind == Kind::Pulse && glyphs != Glyphs::Ascii && rgb
}

/// How long a frame of `kind` is shown: the breath's step while the pulse interpolates
/// (revision 13), its 500 ms two-colour switch when it does not (revision 10), the rotating
/// kinds' 250 ms. The loop wakes at this interval while a spinner is visible.
pub fn frame_interval(kind: Kind, glyphs: Glyphs, rgb: bool) -> Duration {
    if breathes(kind, glyphs, rgb) {
        pulse_step()
    } else {
        kind.interval()
    }
}

/// The frame at `tick` (a monotonic elapsed time since some fixed origin).
pub fn frame(kind: Kind, glyphs: Glyphs, rgb: bool, tick: Duration) -> Frame {
    let breathe = breathes(kind, glyphs, rgb);
    if glyphs == Glyphs::Ascii {
        return match kind {
            Kind::Pulse => Frame {
                glyph: "*",
                step: pulse_switch_step(tick),
            },
            // Every rotating kind is `| / - \` in the ascii set.
            Kind::Arc | Kind::Quadrant | Kind::Braille => Frame {
                glyph: ASCII[index(tick, kind.interval(), ASCII.len())],
                step: PULSE_ACCENT_STEP,
            },
        };
    }
    match kind {
        Kind::Pulse => Frame {
            glyph: "⏺",
            step: if breathe {
                breath_level(breath_index(tick))
            } else {
                pulse_switch_step(tick)
            },
        },
        Kind::Arc => Frame {
            glyph: ARC[index(tick, kind.interval(), ARC.len())],
            step: PULSE_ACCENT_STEP,
        },
        Kind::Quadrant => Frame {
            glyph: QUADRANT[index(tick, kind.interval(), QUADRANT.len())],
            step: PULSE_ACCENT_STEP,
        },
        Kind::Braille => Frame {
            glyph: BRAILLE[index(tick, kind.interval(), BRAILLE.len())],
            step: PULSE_ACCENT_STEP,
        },
    }
}

/// Revision 10's two-colour switch: the accent on even intervals, the dim end on odd ones.
fn pulse_switch_step(tick: Duration) -> u8 {
    if index(tick, PULSE_SWITCH, 2) == 0 {
        PULSE_ACCENT_STEP
    } else {
        PULSE_DIM_STEP
    }
}

/// Which sample of the breath `tick` falls in (`0..PULSE_STEPS`). Nanoseconds, not the whole
/// milliseconds [`index`] uses: the step is 1.4 s / 12, which is not a whole number of ms.
fn breath_index(tick: Duration) -> usize {
    let step = pulse_step().as_nanos();
    ((tick.as_nanos() / step) % u128::from(PULSE_STEPS)) as usize
}

/// The brightness of breath sample `index`: one sine period over [`PULSE_STEPS`] samples, from
/// the dim end (sample 0) to the accent (the middle sample) and back. The change is slowest at
/// both extremes — that is what reads as breathing.
fn breath_level(index: usize) -> u8 {
    let phase = std::f64::consts::TAU * index as f64 / f64::from(PULSE_STEPS);
    let level = (1.0 - phase.cos()) / 2.0 * f64::from(PULSE_STEPS);
    (level.round() as i64).clamp(0, i64::from(PULSE_STEPS)) as u8
}

/// The colour of the breath at `step` (`0..=PULSE_STEPS`): `dim` at [`PULSE_DIM_STEP`], `accent`
/// at [`PULSE_ACCENT_STEP`], interpolated in RGB space between them. `None` when either end is
/// not RGB (revision 13 decision 3: a 16/256-colour theme and `mono` keep the two-colour switch).
pub fn breath_color(accent: Option<Color>, dim: Option<Color>, step: u8) -> Option<Color> {
    let (Some(Color::Rgb(ar, ag, ab)), Some(Color::Rgb(dr, dg, db))) = (accent, dim) else {
        return None;
    };
    let t = f64::from(step.min(PULSE_ACCENT_STEP)) / f64::from(PULSE_STEPS);
    let channel = |dim: u8, accent: u8| {
        (f64::from(dim) + (f64::from(accent) - f64::from(dim)) * t).round() as u8
    };
    Some(Color::Rgb(
        channel(dr, ar),
        channel(dg, ag),
        channel(db, ab),
    ))
}

fn index(tick: Duration, interval: Duration, len: usize) -> usize {
    (tick.as_millis() / interval.as_millis()) as usize % len
}

/// Every glyph a spinner can draw, for the coverage check (ADR 0027). `arc` and
/// `pulse` are the documented exceptions there; the rotating covered sets are `quadrant`,
/// `braille` and `ascii`.
pub fn glyphs() -> Vec<&'static str> {
    let mut out = Vec::new();
    out.extend(ARC);
    out.extend(QUADRANT);
    out.extend(BRAILLE);
    out.extend(ASCII);
    out.push("⏺");
    out.push("*");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame advances with time, not with the text: at 250 ms steps the rotating frames
    /// walk their sequence and wrap.
    #[test]
    fn the_frame_advances_with_time() {
        let ms = |n: u64| Duration::from_millis(n);
        let f = |t: Duration| frame(Kind::Quadrant, Glyphs::Unicode, false, t);
        assert_eq!(f(ms(0)).glyph, "▖");
        assert_eq!(f(ms(250)).glyph, "▘");
        assert_eq!(f(ms(500)).glyph, "▝");
        assert_eq!(f(ms(750)).glyph, "▗");
        assert_eq!(f(ms(1000)).glyph, "▖", "wraps");
        // The braille kind has ten frames.
        assert_eq!(
            frame(Kind::Braille, Glyphs::Unicode, false, ms(0)).glyph,
            "⠋"
        );
        assert_eq!(
            frame(Kind::Braille, Glyphs::Unicode, false, ms(2250)).glyph,
            "⠏"
        );
        assert_eq!(
            frame(Kind::Braille, Glyphs::Unicode, false, ms(2500)).glyph,
            "⠋"
        );
    }

    /// ADR 0027: one period of the breath is [`PULSE_STEPS`] samples of a sine, from
    /// the dim end (sample 0) up to the accent (the middle sample) and back down. The ends slow
    /// down — every sample is a step of at most a quarter of the range — and the glyph, position
    /// and width never change.
    #[test]
    fn the_breath_walks_one_sine_period_in_twelve_steps() {
        let sample = |index: u32| frame(Kind::Pulse, Glyphs::Unicode, true, pulse_step() * index);
        let levels: Vec<u8> = (0..u32::from(PULSE_STEPS))
            .map(|i| sample(i).step)
            .collect();
        assert_eq!(
            levels,
            vec![0, 1, 3, 6, 9, 11, 12, 11, 9, 6, 3, 1],
            "the sampled sine, dark -> bright -> dark"
        );
        assert_eq!(levels[0], PULSE_DIM_STEP, "sample 0 is the dim end");
        assert_eq!(
            levels[usize::from(PULSE_STEPS) / 2],
            PULSE_ACCENT_STEP,
            "the middle sample is the accent"
        );
        // The ADR's two numbers, pinned; everything else derives from them.
        assert_eq!(PULSE_PERIOD, Duration::from_millis(1_400));
        assert_eq!(PULSE_STEPS, 12);
        let (up, down) = levels.split_at(usize::from(PULSE_STEPS) / 2 + 1);
        assert!(
            up.windows(2).all(|pair| pair[0] < pair[1]),
            "strictly brighter to the middle: {up:?}"
        );
        assert!(
            down.windows(2).all(|pair| pair[0] > pair[1]),
            "strictly dimmer after it: {down:?}"
        );
        assert!(
            levels
                .windows(2)
                .all(|pair| pair[1].abs_diff(pair[0]) <= PULSE_STEPS / 4),
            "slow at both ends: {levels:?}"
        );
        // The next sample after the last one wraps to the start of the next breath, and the
        // glyph is the same in every phase (only the colour moves).
        assert_eq!(
            frame(Kind::Pulse, Glyphs::Unicode, true, PULSE_PERIOD).step,
            PULSE_DIM_STEP,
            "one period is exactly one breath"
        );
        use unicode_width::UnicodeWidthStr;
        for i in 0..u32::from(PULSE_STEPS) {
            let f = sample(i);
            assert_eq!(f.glyph, "⏺", "sample {i}");
            assert_eq!(f.glyph.width(), 1, "sample {i}");
        }
    }

    /// ADR 0027: a pulse that cannot interpolate — either end of the
    /// breath is not RGB, or the glyph set is `ascii` — keeps revision 10's two-colour switch,
    /// and its interval is the switch's 500 ms.
    #[test]
    fn a_pulse_that_cannot_interpolate_keeps_the_two_colour_switch() {
        let ms = |n: u64| Duration::from_millis(n);
        for (kind_glyphs, rgb) in [
            (Glyphs::Unicode, false),
            (Glyphs::Ascii, true),
            (Glyphs::Nerd, false),
        ] {
            let at = |t: u64| frame(Kind::Pulse, kind_glyphs, rgb, ms(t)).step;
            assert_eq!(at(0), PULSE_ACCENT_STEP, "{kind_glyphs:?} {rgb}");
            assert_eq!(at(499), PULSE_ACCENT_STEP, "{kind_glyphs:?} {rgb}");
            assert_eq!(at(500), PULSE_DIM_STEP, "{kind_glyphs:?} {rgb}");
            assert_eq!(at(999), PULSE_DIM_STEP, "{kind_glyphs:?} {rgb}");
            assert_eq!(at(1000), PULSE_ACCENT_STEP, "{kind_glyphs:?} {rgb} wraps");
            assert_eq!(
                frame_interval(Kind::Pulse, kind_glyphs, rgb),
                PULSE_SWITCH,
                "{kind_glyphs:?} {rgb}"
            );
        }
        // The interpolating pulse wakes at the step, not at the switch.
        assert_eq!(
            frame_interval(Kind::Pulse, Glyphs::Unicode, true),
            pulse_step()
        );
        // One step is the period over the step count (~117 ms) and covers it once.
        assert_eq!(pulse_step(), PULSE_PERIOD / u32::from(PULSE_STEPS));
        assert!(pulse_step().as_millis() > 100 && pulse_step().as_millis() < 130);
    }

    /// The breath's colours (revision 13): the dim end exactly at [`PULSE_DIM_STEP`], the accent
    /// exactly at [`PULSE_ACCENT_STEP`], the levels in between strictly advancing through the
    /// RGB space between them. `None` — the two-colour switch — when either end is not RGB,
    /// which is a 16/256-colour theme or `mono`.
    #[test]
    fn the_breath_colour_interpolates_only_between_two_rgb_ends() {
        let accent = Some(Color::Rgb(200, 40, 120));
        let dim = Some(Color::Rgb(60, 60, 60));
        assert_eq!(breath_color(accent, dim, PULSE_DIM_STEP), dim);
        assert_eq!(breath_color(accent, dim, PULSE_ACCENT_STEP), accent);
        assert_eq!(
            breath_color(accent, dim, PULSE_STEPS / 2),
            Some(Color::Rgb(130, 50, 90)),
            "the midpoint is the half-and-half mix"
        );
        let brightness = |step: u8| match breath_color(accent, dim, step) {
            Some(Color::Rgb(r, g, b)) => u32::from(r) + u32::from(g) + u32::from(b),
            other => panic!("an RGB breath at {step}: {other:?}"),
        };
        // The level-to-colour map is the straight line from the dim end to the accent: every
        // step up is brighter. (It is the *samples* of the sine that turn around, above.)
        for step in 1..=PULSE_ACCENT_STEP {
            assert!(
                brightness(step) > brightness(step - 1),
                "the rise must be strict at {step}"
            );
        }
        // Not RGB at one end: no interpolation (the caller then switches, revision 10).
        assert_eq!(breath_color(Some(Color::Indexed(5)), dim, 3), None);
        assert_eq!(breath_color(accent, Some(Color::Indexed(5)), 3), None);
        assert_eq!(breath_color(accent, None, 3), None);
        assert_eq!(breath_color(None, dim, 3), None);
    }

    /// The ascii set: `*` for pulse, `| / - \` for the rotating kinds.
    #[test]
    fn the_ascii_set_falls_back() {
        let ms = |n: u64| Duration::from_millis(n);
        assert_eq!(frame(Kind::Pulse, Glyphs::Ascii, true, ms(0)).glyph, "*");
        assert_eq!(frame(Kind::Arc, Glyphs::Ascii, true, ms(0)).glyph, "|");
        assert_eq!(frame(Kind::Arc, Glyphs::Ascii, true, ms(250)).glyph, "/");
        assert_eq!(
            frame(Kind::Braille, Glyphs::Ascii, true, ms(500)).glyph,
            "-"
        );
        assert_eq!(
            frame(Kind::Quadrant, Glyphs::Ascii, true, ms(750)).glyph,
            "\\"
        );
    }

    /// The intervals and the names round-trip.
    #[test]
    fn the_kind_names_and_intervals() {
        for kind in [Kind::Pulse, Kind::Arc, Kind::Quadrant, Kind::Braille] {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("nope"), None);
        assert_eq!(Kind::default(), Kind::Pulse);
        assert_eq!(Kind::Pulse.interval(), PULSE_SWITCH);
        assert_eq!(Kind::Arc.interval(), ROTATING_INTERVAL);
        // The rotating kinds are unaffected by the breath (revision 13 decision 3).
        for kind in [Kind::Arc, Kind::Quadrant, Kind::Braille] {
            assert_eq!(
                frame_interval(kind, Glyphs::Unicode, true),
                ROTATING_INTERVAL
            );
            assert_eq!(
                frame(kind, Glyphs::Unicode, true, Duration::from_millis(250)).step,
                PULSE_ACCENT_STEP
            );
        }
    }
}
