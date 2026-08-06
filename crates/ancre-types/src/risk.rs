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

    /// The `Enum8` discriminant, which is what a ClickHouse row actually
    /// carries — one byte, not a string. Pinned by the DDL and never
    /// renumbered.
    #[must_use]
    pub const fn as_discriminant(self) -> i8 {
        self as i8
    }

    /// The inverse of `as_discriminant`. `None` for a value outside the
    /// pinned set, for the same reason `from_wire` refuses an unknown name.
    #[must_use]
    pub fn from_discriminant(d: i8) -> Option<Self> {
        Some(match d {
            0 => Self::Unclassified,
            1 => Self::Minimal,
            2 => Self::Transparency,
            3 => Self::High,
            _ => return None,
        })
    }

    /// The inverse of `as_str`. `None` rather than a default: a row carrying
    /// a class this build does not know about must not be read as
    /// `Unclassified`, which would silently downgrade a high-risk system's
    /// evidence to a lower one.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "unclassified" => Self::Unclassified,
            "minimal" => Self::Minimal,
            "transparency" => Self::Transparency,
            "high" => Self::High,
            _ => return None,
        })
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
    /// This configuration change **may** constitute a substantial
    /// modification. Review required.
    ///
    /// Carried on `config.generation.applied` only. It flags; a human decides;
    /// the decision is logged. No code may branch on this as though it were a
    /// determination — under the Act, a substantial modification can reset a
    /// grandfathering position, and that is a legal call (PRD §6.5).
    SubstantialCandidate,
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
            Self::SubstantialCandidate => "substantial_candidate",
        }
    }

    /// The inverse of `as_str`. See `RiskClass::from_wire`.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "stale_config" => Self::StaleConfig,
            "unpinned_model" => Self::UnpinnedModel,
            "pin_overridden" => Self::PinOverridden,
            "no_policy_engine" => Self::NoPolicyEngine,
            "telemetry_dropped" => Self::TelemetryDropped,
            "substantial_candidate" => Self::SubstantialCandidate,
            _ => return None,
        })
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
        assert_eq!(
            RiskFlag::SubstantialCandidate.as_str(),
            "substantial_candidate"
        );
        assert_eq!(ChangeClass::Substantial.as_str(), "substantial");
    }

    /// Every variant must survive `as_str` → `from_wire`. A variant added
    /// without a matching parse arm reads back as `None`, which is how an
    /// event would become unreadable from its own store.
    #[test]
    fn every_wire_form_round_trips() {
        for c in [
            RiskClass::Unclassified,
            RiskClass::Minimal,
            RiskClass::Transparency,
            RiskClass::High,
        ] {
            assert_eq!(RiskClass::from_wire(c.as_str()), Some(c));
        }
        for f in [
            RiskFlag::StaleConfig,
            RiskFlag::UnpinnedModel,
            RiskFlag::PinOverridden,
            RiskFlag::NoPolicyEngine,
            RiskFlag::TelemetryDropped,
            RiskFlag::SubstantialCandidate,
        ] {
            assert_eq!(RiskFlag::from_wire(f.as_str()), Some(f));
        }
        for c in [
            RiskClass::Unclassified,
            RiskClass::Minimal,
            RiskClass::Transparency,
            RiskClass::High,
        ] {
            assert_eq!(RiskClass::from_discriminant(c.as_discriminant()), Some(c));
        }
        assert_eq!(RiskClass::from_discriminant(4), None);
        assert_eq!(RiskClass::from_wire("not-a-class"), None);
        assert_eq!(RiskFlag::from_wire("not-a-flag"), None);
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
