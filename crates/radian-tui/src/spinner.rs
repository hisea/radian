//! The spinner (ADR 0027/9/10): one definition for every running indicator.
//!
//! The frame is a function of **time**, not of the text length (the old
//! `SPINNER[text.len() % 4]` turned once per token). `pulse` is the default: a `⏺` that keeps
//! its glyph, position and width and only alternates between the accent and the weak level,
//! every 500 ms. The rotating kinds turn every 250 ms. The glyph set decides the glyphs:
//! `ascii` uses `| / - \` (and `*` for pulse).

use std::time::Duration;

use crate::footer::Glyphs;

/// Which spinner the user configured (`tui.spinner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// A `⏺` that pulses between the accent and the weak level (the default, revision 10).
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

    /// The frame interval: pulse 500 ms, the rotating frames 250 ms (ADR 0027/10).
    pub fn interval(self) -> Duration {
        match self {
            Self::Pulse => Duration::from_millis(500),
            Self::Arc | Self::Quadrant | Self::Braille => Duration::from_millis(250),
        }
    }
}

/// One frame: the glyph and whether it is in the pulse's weak phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub glyph: &'static str,
    /// The pulse's off phase: draw the glyph one level weaker. Always `false` for the
    /// rotating kinds.
    pub dim: bool,
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

/// The frame at `tick` (a monotonic elapsed time since some fixed origin).
pub fn frame(kind: Kind, glyphs: Glyphs, tick: Duration) -> Frame {
    if glyphs == Glyphs::Ascii {
        return match kind {
            Kind::Pulse => Frame {
                glyph: "*",
                dim: (tick.as_millis() / Kind::Pulse.interval().as_millis()) % 2 == 1,
            },
            // Every rotating kind is `| / - \` in the ascii set.
            Kind::Arc | Kind::Quadrant | Kind::Braille => Frame {
                glyph: ASCII[index(tick, ASCII.len())],
                dim: false,
            },
        };
    }
    match kind {
        Kind::Pulse => Frame {
            glyph: "⏺",
            dim: (tick.as_millis() / Kind::Pulse.interval().as_millis()) % 2 == 1,
        },
        Kind::Arc => Frame {
            glyph: ARC[index(tick, ARC.len())],
            dim: false,
        },
        Kind::Quadrant => Frame {
            glyph: QUADRANT[index(tick, QUADRANT.len())],
            dim: false,
        },
        Kind::Braille => Frame {
            glyph: BRAILLE[index(tick, BRAILLE.len())],
            dim: false,
        },
    }
}

fn index(tick: Duration, len: usize) -> usize {
    // The rotating kinds all turn at 250 ms.
    (tick.as_millis() / Duration::from_millis(250).as_millis()) as usize % len
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
        assert_eq!(frame(Kind::Quadrant, Glyphs::Unicode, ms(0)).glyph, "▖");
        assert_eq!(frame(Kind::Quadrant, Glyphs::Unicode, ms(250)).glyph, "▘");
        assert_eq!(frame(Kind::Quadrant, Glyphs::Unicode, ms(500)).glyph, "▝");
        assert_eq!(frame(Kind::Quadrant, Glyphs::Unicode, ms(750)).glyph, "▗");
        assert_eq!(
            frame(Kind::Quadrant, Glyphs::Unicode, ms(1000)).glyph,
            "▖",
            "wraps"
        );
        // The braille kind has ten frames.
        assert_eq!(frame(Kind::Braille, Glyphs::Unicode, ms(0)).glyph, "⠋");
        assert_eq!(frame(Kind::Braille, Glyphs::Unicode, ms(2250)).glyph, "⠏");
        assert_eq!(frame(Kind::Braille, Glyphs::Unicode, ms(2500)).glyph, "⠋");
    }

    /// Pulse keeps its glyph and width; only the phase (the color) changes, every 500 ms.
    #[test]
    fn pulse_keeps_its_glyph_and_changes_phase() {
        let ms = |n: u64| Duration::from_millis(n);
        for t in [0, 250, 499] {
            let f = frame(Kind::Pulse, Glyphs::Unicode, ms(t));
            assert_eq!(f.glyph, "⏺", "{t}");
            assert!(!f.dim, "{t}");
        }
        for t in [500, 750, 999] {
            let f = frame(Kind::Pulse, Glyphs::Unicode, ms(t));
            assert_eq!(f.glyph, "⏺", "{t}");
            assert!(f.dim, "the weak phase at {t}");
        }
        assert!(!frame(Kind::Pulse, Glyphs::Unicode, ms(1000)).dim, "wraps");
        // One cell wide in every phase.
        use unicode_width::UnicodeWidthStr;
        assert_eq!(
            frame(Kind::Pulse, Glyphs::Unicode, ms(500)).glyph.width(),
            1
        );
    }

    /// The ascii set: `*` for pulse, `| / - \` for the rotating kinds.
    #[test]
    fn the_ascii_set_falls_back() {
        let ms = |n: u64| Duration::from_millis(n);
        assert_eq!(frame(Kind::Pulse, Glyphs::Ascii, ms(0)).glyph, "*");
        assert_eq!(frame(Kind::Arc, Glyphs::Ascii, ms(0)).glyph, "|");
        assert_eq!(frame(Kind::Arc, Glyphs::Ascii, ms(250)).glyph, "/");
        assert_eq!(frame(Kind::Braille, Glyphs::Ascii, ms(500)).glyph, "-");
        assert_eq!(frame(Kind::Quadrant, Glyphs::Ascii, ms(750)).glyph, "\\");
    }

    /// The intervals and the names round-trip.
    #[test]
    fn the_kind_names_and_intervals() {
        for kind in [Kind::Pulse, Kind::Arc, Kind::Quadrant, Kind::Braille] {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("nope"), None);
        assert_eq!(Kind::default(), Kind::Pulse);
        assert_eq!(Kind::Pulse.interval(), Duration::from_millis(500));
        assert_eq!(Kind::Arc.interval(), Duration::from_millis(250));
    }
}
