//! Deterministic event fixtures, behind the `fixtures` feature.
//!
//! Shared by the chain tests, the verifier's integration tests and the
//! benches, so that all three exercise the same shapes. Feature-gated so none
//! of it can end up compiled into a gateway or ingester binary.
//!
//! Everything here is a function of `seq` alone: the same call produces the
//! same bytes on every machine, every run. A fixture with a `now()` or a
//! random UUID in it would make a hash-comparison test that passes locally and
//! fails in CI for reasons nobody can reproduce.

use std::sync::Arc;

use smallvec::smallvec;
use uuid::Uuid;

use crate::event::{AuditEvent, EmittedEvent, EventType, Metrics, Outcome};
use crate::pins::Pins;
use crate::risk::RiskClass;
use crate::time::Timestamp;

/// A fixed instant, so fixtures never depend on the clock.
pub const BASE_MICROS: i64 = 1_754_400_000_000_000;

/// One millisecond per event. Saturating, so a fixture built with an absurd
/// seq produces a clamped timestamp rather than a wrapped one.
fn offset(seq: u64) -> i64 {
    i64::try_from(seq)
        .unwrap_or(i64::MAX / 1_000)
        .saturating_mul(1_000)
}

#[must_use]
pub fn pins(_seq: u64) -> Pins {
    Pins {
        config_generation: 41,
        config_hash: ancre_canon::hash_bytes(b"snapshot-41"),
        system_id: "hr-screening".into(),
        system_version: "2.1.0".into(),
        ifu_version: "b3:aa11".into(),
        model_id: "gpt-4o".into(),
        model_version: "gpt-4o-2024-08-06".into(),
        prompt_id: "screen-cv".into(),
        prompt_version: "b3:9f2c".into(),
        // No policy engine in the MVP — `none`, never NULL, never "".
        policy_id: "none".into(),
        policy_version: "none".into(),
        gateway_version: "0.1.0+abc123".into(),
        risk_class: RiskClass::High,
        resolved_stale: false,
        risk_flags: smallvec![],
    }
}

/// One event, unsealed: `event_hash` is left at `GENESIS` for the caller to
/// stamp via `ancre_chain::seal`. Types cannot compute the chain themselves —
/// that would be a circular dependency, and it is the ingester's job anyway.
#[must_use]
pub fn event(seq: u64, prev_hash: ancre_canon::Hash32) -> AuditEvent {
    AuditEvent {
        seq,
        prev_hash,
        event_hash: ancre_canon::GENESIS,
        canon_version: ancre_canon::CANON_VERSION.into(),
        ingested_at: Timestamp::from_micros(BASE_MICROS + offset(seq)),
        emitted: EmittedEvent {
            tenant_id: "acme".into(),
            system_id: "hr-screening".into(),
            event_id: Uuid::from_u128(u128::from(seq)),
            trace_id: Arc::from(format!("trace-{seq}")),
            attempt_seq: 0,
            occurred_at: Timestamp::from_micros(BASE_MICROS + offset(seq) - 400),
            node_id: "gw-1".into(),
            event_type: EventType::LlmRequest,
            outcome: Outcome::Ok,
            pins: pins(seq),
            request_digest: ancre_canon::hash_bytes(format!("req-{seq}").as_bytes()),
            response_digest: ancre_canon::hash_bytes(format!("resp-{seq}").as_bytes()),
            metrics: Metrics {
                provider: "openai".into(),
                http_status: 200,
                latency_ms: 400 + u32::try_from(seq % 100).unwrap_or(0),
                ttft_ms: 80,
                tokens_in: 1200,
                tokens_out: 300,
                error_code: "".into(),
            },
        },
        // Present and empty in the MVP, so V1's crypto-shredding does not have
        // to bump canon_version.
        payload_ref: String::new(),
        subject_key_id: String::new(),
    }
}
