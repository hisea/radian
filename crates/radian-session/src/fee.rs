//! The fee one model call cost, recorded beside its usage (ADR 0027, ADR 0007
//! ).
//!
//! The session total is accumulated **per call, at the price in force when the call ran**
//! (ADR 0027). A journal written that way used to record only the tokens: a resume
//! re-priced every historical call with whatever the catalog said *then*, so a catalog update
//! silently changed what an old session had cost. The record now carries the fee the call was
//! billed at, so a resume adds it as it stands (ADR 0027).
//!
//! The fee is **money**, not a token count: a counter the catalog has no rate for is not billed
//! and makes the sum *incomplete* rather than silently dropping the term. `micros` is
//! micro-dollars (millionths of a US dollar), the same unit `radian_host::cost` computes in.

use serde::{Deserialize, Serialize};

/// One call's fee in micro-dollars, and whether a billable counter had no rate.
///
/// `incomplete` is `true` when at least one **non-zero** counter the provider reported had no
/// rate in the catalog: `micros` is then the sum of the counters that could be priced, a lower
/// bound on the call's real cost. A call whose every non-zero counter had a rate is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFee {
    /// The known fee in micro-dollars (millionths of a US dollar).
    pub micros: u64,
    /// Whether a non-zero counter the provider reported had no rate. Omitted from JSON when
    /// `false`, so a complete call serializes as just `{"micros": …}` (ADR 0006: an unknown
    /// field is ignored by an older reader, and a missing `incomplete` reads as `false`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub incomplete: bool,
}

impl UsageFee {
    pub fn new(micros: u64, incomplete: bool) -> Self {
        Self { micros, incomplete }
    }

    /// Adds two fees: the money adds, and the result is incomplete if either part was.
    pub fn merge(self, other: Self) -> Self {
        Self {
            micros: self.micros.saturating_add(other.micros),
            incomplete: self.incomplete || other.incomplete,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_fee_serializes_without_the_flag_and_reads_back() {
        let complete = UsageFee::new(3_270, false);
        assert_eq!(
            serde_json::to_string(&complete).unwrap(),
            r#"{"micros":3270}"#
        );
        assert_eq!(
            serde_json::from_str::<UsageFee>(r#"{"micros":3270}"#).unwrap(),
            complete
        );
    }

    #[test]
    fn an_incomplete_fee_carries_the_flag() {
        let incomplete = UsageFee::new(1_234, true);
        assert_eq!(
            serde_json::to_string(&incomplete).unwrap(),
            r#"{"micros":1234,"incomplete":true}"#
        );
        assert_eq!(
            serde_json::from_str::<UsageFee>(r#"{"micros":1234,"incomplete":true}"#).unwrap(),
            incomplete
        );
    }

    #[test]
    fn merging_keeps_the_incomplete_flag() {
        assert_eq!(
            UsageFee::new(1, false).merge(UsageFee::new(2, true)),
            UsageFee::new(3, true)
        );
        assert_eq!(
            UsageFee::new(1, false).merge(UsageFee::new(2, false)),
            UsageFee::new(3, false)
        );
    }
}
