//! The pin set stamped onto every audit event.

use std::sync::Arc;

use ancre_canon::Hash32;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::risk::{RiskClass, RiskFlag};

/// The value written when a pin cannot be determined. Never NULL, never "".
pub const UNKNOWN: &str = "unknown";

/// Prefix for a model alias the provider would not resolve to a pinned id.
pub const UNRESOLVED_PREFIX: &str = "unresolved:";

/// Resolved **once**, at request admission, then carried immutably.
///
/// Re-resolving before writing the audit event is the bug that eats the whole
/// design: a streaming completion can run for 90 seconds, and a reload in that
/// window would make the event report a configuration the request never used
/// (resolver spec §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pins {
    pub config_generation: u64,
    pub config_hash: Hash32,
    pub system_id: Arc<str>,
    pub system_version: Arc<str>,
    pub ifu_version: Arc<str>,
    pub model_id: Arc<str>,
    pub model_version: Arc<str>,
    pub prompt_id: Arc<str>,
    pub prompt_version: Arc<str>,
    pub policy_id: Arc<str>,
    pub policy_version: Arc<str>,
    pub gateway_version: Arc<str>,
    pub risk_class: RiskClass,
    pub resolved_stale: bool,
    pub risk_flags: SmallVec<[RiskFlag; 4]>,
}

impl Pins {
    /// Every pin `unknown`, for the null-baseline build only.
    ///
    /// The overhead claim is a **delta** measured against an identical binary
    /// with pinning compiled out. Measuring against direct-to-provider instead
    /// would fold network variance into the number and produce something that
    /// falls apart the first time a prospect reproduces it (mvp-plan §5, M3).
    ///
    /// These pins are honest about being nothing: `has_gap()` is true, so if
    /// this ever reached production traffic the events would say so loudly
    /// rather than looking like real evidence.
    #[must_use]
    pub fn null_baseline() -> Self {
        let unknown: Arc<str> = Arc::from(UNKNOWN);
        Self {
            config_generation: 0,
            config_hash: ancre_canon::GENESIS,
            system_id: Arc::clone(&unknown),
            system_version: Arc::clone(&unknown),
            ifu_version: Arc::clone(&unknown),
            model_id: Arc::clone(&unknown),
            model_version: Arc::clone(&unknown),
            prompt_id: Arc::clone(&unknown),
            prompt_version: Arc::clone(&unknown),
            policy_id: Arc::clone(&unknown),
            policy_version: Arc::clone(&unknown),
            gateway_version: unknown,
            risk_class: RiskClass::Unclassified,
            resolved_stale: false,
            risk_flags: SmallVec::new(),
        }
    }

    /// True if any pin is `unknown` or an unresolved alias — i.e. this event
    /// cannot fully reconstruct the decision that produced it.
    ///
    /// This is the countable gap. `unknown` shows up in a `GROUP BY` and turns
    /// into a line item on an invoice; NULL would just hide (PRD §6.3).
    ///
    /// Note what is *not* a gap: `none`. A `policy_version` of `none` states
    /// that no policy engine is configured, which is a fact about the system,
    /// not a missing measurement.
    #[must_use]
    pub fn has_gap(&self) -> bool {
        [
            &self.system_version,
            &self.ifu_version,
            &self.model_id,
            &self.model_version,
            &self.prompt_id,
            &self.prompt_version,
            &self.policy_id,
            &self.policy_version,
            &self.gateway_version,
        ]
        .iter()
        .any(|v| &***v == UNKNOWN || v.starts_with(UNRESOLVED_PREFIX))
    }
}

/// Per-request context. Built at admission, read by every downstream emitter.
#[derive(Debug)]
pub struct RequestCtx {
    pub trace_id: Arc<str>,
    /// Failover emits one event per attempt, each with its own model pin, all
    /// sharing `trace_id` (resolver spec §5). The MVP designs the field in;
    /// the failover logic itself is November (mvp-plan §0).
    pub attempt_seq: u16,
    pub pins: Pins,
    pub started: std::time::Instant,
}
