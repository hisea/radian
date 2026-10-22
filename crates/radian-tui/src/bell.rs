//! The `tui.bell` terminal bell: one `\x07` for attention-class events, off by default.
//!
//! `off` never rings; `attention` rings when an approval or `ask_user` prompt opens and
//! when a turn ends (completed or failed); `all` additionally rings when a background job
//! settles. Each event rings at most once, and events that share a frame merge into one
//! (the loop's `Effect::Bell` runs once per frame).

/// The `tui.bell` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BellMode {
    /// Never ring (the default).
    #[default]
    Off,
    /// Ring for a newly shown approval/question and for a turn end.
    Attention,
    /// Like `attention`, plus a ring when a background job settles.
    All,
}

impl BellMode {
    /// The config spelling. `None` for anything else, so the caller warns and falls back to
    /// the default (the `tui.*` rule for a display preference, the way `tui.clipboard` does).
    pub fn parse(name: &str) -> Option<Self> {
        if !radian_host::config::BELL_MODES.contains(&name) {
            return None;
        }
        match name {
            "off" => Some(Self::Off),
            "attention" => Some(Self::Attention),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// The config spelling of this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => radian_host::config::BELL_MODES[0],
            Self::Attention => radian_host::config::BELL_MODES[1],
            Self::All => radian_host::config::BELL_MODES[2],
        }
    }

    /// Whether a background job's settle rings in this mode (approvals, questions and turn
    /// ends ring in every mode but `off`).
    pub fn rings_for_job_settle(self) -> bool {
        matches!(self, Self::All)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_and_unknown_names_are_none() {
        for mode in [BellMode::Off, BellMode::Attention, BellMode::All] {
            assert_eq!(
                BellMode::parse(match mode {
                    BellMode::Off => "off",
                    BellMode::Attention => "attention",
                    BellMode::All => "all",
                }),
                Some(mode)
            );
        }
        assert_eq!(BellMode::parse("on"), None);
        assert_eq!(BellMode::parse(""), None);
        assert_eq!(BellMode::default(), BellMode::Off);
    }

    #[test]
    fn only_all_rings_for_a_job_settle() {
        assert!(!BellMode::Off.rings_for_job_settle());
        assert!(!BellMode::Attention.rings_for_job_settle());
        assert!(BellMode::All.rings_for_job_settle());
    }
}
