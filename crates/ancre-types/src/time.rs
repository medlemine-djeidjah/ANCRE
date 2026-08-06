//! Timestamps, as an explicit integer.
//!
//! Microseconds since the Unix epoch, UTC, as an `i64`. Deliberately not
//! `time::OffsetDateTime`: this value is hashed, and the canonical encoding
//! must not depend on how a third-party crate's serde impl happens to
//! represent a datetime today. A `time` upgrade must never be able to
//! invalidate a chain.
//!
//! `i64` microseconds covers years 1677–2262. The 7-year retention ceiling
//! makes that a non-question.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Wall clock. Used for `occurred_at` on the gateway and `ingested_at` on
    /// the ingester.
    ///
    /// Note what this is *not* used for: ordering. Chain order comes from
    /// `seq`, which the ingester allocates, precisely so that a skewed clock
    /// on one gateway node cannot reorder a chain (PRD §9).
    #[must_use]
    pub fn now() -> Self {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self(i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
    }

    #[must_use]
    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    #[must_use]
    pub const fn as_micros(self) -> i64 {
        self.0
    }

    /// RFC 3339, for reports an auditor reads. Never for hashing.
    #[must_use]
    pub fn to_rfc3339(self) -> String {
        let secs = self.0.div_euclid(1_000_000);
        let micros = self.0.rem_euclid(1_000_000);
        match time::OffsetDateTime::from_unix_timestamp(secs) {
            Ok(t) => (t + time::Duration::microseconds(micros))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| self.0.to_string()),
            // Out of range for a datetime. Fall back to the raw value rather
            // than panicking — a report with an odd timestamp is far better
            // than a verifier that dies on one bad row.
            Err(_) => self.0.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_after_2020_and_before_2100() {
        let n = Timestamp::now().as_micros();
        assert!(n > 1_577_836_800_000_000, "before 2020");
        assert!(n < 4_102_444_800_000_000, "after 2100");
    }

    #[test]
    fn formats_as_rfc3339() {
        assert_eq!(
            Timestamp::from_micros(1_754_400_000_000_000).to_rfc3339(),
            "2025-08-05T13:20:00Z"
        );
    }

    #[test]
    fn sub_second_precision_survives_formatting() {
        assert_eq!(
            Timestamp::from_micros(1_754_400_000_123_456).to_rfc3339(),
            "2025-08-05T13:20:00.123456Z"
        );
    }

    #[test]
    fn handles_pre_epoch_without_panicking() {
        // div_euclid/rem_euclid rather than / and %, so a negative timestamp
        // does not produce a negative microsecond component.
        assert!(Timestamp::from_micros(-1).to_rfc3339().starts_with("1969-"));
    }
}
