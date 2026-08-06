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
    /// True if any pin is `unknown` or an unresolved alias — i.e. this event
    /// cannot fully reconstruct the decision that produced it.
    #[must_use]
    pub fn has_gap(&self) -> bool {
        todo!("M2: scan the pin fields for UNKNOWN / UNRESOLVED_PREFIX")
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
