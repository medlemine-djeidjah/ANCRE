//! Risk classification and flags.
//!
//! Every enum here carries an explicit `as_str()` that is what actually goes
//! into a hashed body. The serde attributes are for JSON and for the API; the
//! hashed encoding must not depend on them, or renaming a variant silently
//! changes what a chain hashes to.

use serde::{Deserialize, Serialize};

/// Discriminants are pinned to the ClickHouse `Enum8` in mvp-plan §4 and are
/// part of the canonical encoding. They may never be renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum RiskClass {
    Unclassified = 0,
    Minimal = 1,
    Transparency = 2,
    High = 3,
}

impl RiskClass {
    /// Frozen wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unclassified => "unclassified",
            Self::Minimal => "minimal",
            Self::Transparency => "transparency",
            Self::High => "high",
        }
    }

    /// Cold start and stale config both fail closed for High. Configurable,
    /// defaulted on, and any attempt to turn it off is itself a governance
    /// event (resolver spec §6).
    #[must_use]
    pub const fn fails_closed_by_default(self) -> bool {
        matches!(self, Self::High)
    }
}

/// Countable gaps. Each variant is a finding in a readiness report, which is
/// why they are an enum and not free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskFlag {
    /// Served under a configuration older than the staleness budget.
    StaleConfig,
    /// Provider returned a floating alias, not a pinned model identifier.
    /// The recorded `model_version` is `unresolved:<alias>` and cannot be
    /// relied on to reconstruct the decision (resolver spec §7).
    UnpinnedModel,
    /// A caller pinned their own version via header. Governance event.
    PinOverridden,
    /// No policy engine in the MVP; `policy_version` is `none`.
    NoPolicyEngine,
    /// Telemetry was dropped in this window — the chain is complete but the
    /// record is not. Counted, and the drop is itself an event.
    TelemetryDropped,
}

impl RiskFlag {
    /// Frozen wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaleConfig => "stale_config",
            Self::UnpinnedModel => "unpinned_model",
            Self::PinOverridden => "pin_overridden",
            Self::NoPolicyEngine => "no_policy_engine",
            Self::TelemetryDropped => "telemetry_dropped",
        }
    }
}

/// Output of the consecutive-generation diff (resolver spec §8).
///
/// The product **surfaces a candidate**; it never declares the answer.
/// Substantial modification is a legal determination and wording that suggests
/// otherwise is a liability. UI copy: "may constitute a substantial
/// modification — review required."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeClass {
    Minor,
    Material,
    Substantial,
}

impl ChangeClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minor => "minor",
            Self::Material => "material",
            Self::Substantial => "substantial",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire strings are frozen. If this test fails because someone
    /// intentionally changed one, that change requires a new `canon_version`
    /// and a verifier that dispatches on it — not a fixed-up assertion.
    #[test]
    fn wire_forms_are_frozen() {
        assert_eq!(RiskClass::High.as_str(), "high");
        assert_eq!(RiskClass::Unclassified.as_str(), "unclassified");
        assert_eq!(RiskFlag::StaleConfig.as_str(), "stale_config");
        assert_eq!(RiskFlag::UnpinnedModel.as_str(), "unpinned_model");
        assert_eq!(ChangeClass::Substantial.as_str(), "substantial");
    }

    #[test]
    fn discriminants_match_the_clickhouse_enum8() {
        assert_eq!(RiskClass::Unclassified as u8, 0);
        assert_eq!(RiskClass::Minimal as u8, 1);
        assert_eq!(RiskClass::Transparency as u8, 2);
        assert_eq!(RiskClass::High as u8, 3);
    }

    #[test]
    fn only_high_fails_closed_by_default() {
        assert!(RiskClass::High.fails_closed_by_default());
        assert!(!RiskClass::Transparency.fails_closed_by_default());
        assert!(!RiskClass::Minimal.fails_closed_by_default());
        assert!(!RiskClass::Unclassified.fails_closed_by_default());
    }
}
