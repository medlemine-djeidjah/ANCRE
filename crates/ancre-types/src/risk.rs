//! Risk classification and flags.

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
    /// Cold start and stale-config both fail closed for High. Configurable,
    /// defaulted on, and any attempt to turn it off is itself a governance
    /// event (resolver spec §6).
    #[must_use]
    pub fn fails_closed_by_default(self) -> bool {
        matches!(self, Self::High)
    }
}

/// Countable gaps. Each variant is a finding in a readiness report, which is
/// why they are an enum and not free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
