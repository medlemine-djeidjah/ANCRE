//! The audit event. Field-for-field with the ClickHouse schema in mvp-plan §4.
//!
//! **This struct is frozen in week 1.** Adding a hashed field later changes the
//! canonical encoding, which means old chains verify under a different rule
//! set — you would have to re-key or maintain a rule-set registry forever.
//! `canon_version` exists so that if you must break it, you break it loudly.
//!
//! Columns V1 needs (`payload_ref`, `subject_key_id`) exist now and are empty
//! strings, so V1 does not bump `canon_version`.

use std::sync::Arc;

use ancre_canon::{Bytes, CanonError, Hash32};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::pins::Pins;
use crate::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "llm.request")]
    LlmRequest,
    #[serde(rename = "provider.failover")]
    ProviderFailover,
    #[serde(rename = "config.generation.applied")]
    ConfigGenerationApplied,
    #[serde(rename = "pin.overridden")]
    PinOverridden,
    #[serde(rename = "telemetry.dropped")]
    TelemetryDropped,
    /// One per chain per day, even with no traffic. Without it, "this system
    /// served nothing" and "this system does not exist" look identical in the
    /// record — and absence of evidence has to stay countable (mvp-plan §8.4).
    #[serde(rename = "chain.heartbeat")]
    ChainHeartbeat,
}

impl EventType {
    /// Frozen wire form. This, not the serde attribute, is what gets hashed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LlmRequest => "llm.request",
            Self::ProviderFailover => "provider.failover",
            Self::ConfigGenerationApplied => "config.generation.applied",
            Self::PinOverridden => "pin.overridden",
            Self::TelemetryDropped => "telemetry.dropped",
            Self::ChainHeartbeat => "chain.heartbeat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Error,
    Denied,
    Interrupted,
}

impl Outcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied",
            Self::Interrupted => "interrupted",
        }
    }
}

/// What the gateway emits: everything except the chain fields.
///
/// The gateway does **not** assign `seq` and does **not** compute the chain
/// (PRD §9). It emits unordered events with monotonic local timestamps; the
/// ingester serialises and chains them. This keeps coordination off the hot
/// path and means a node dying mid-flight cannot create a gap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmittedEvent {
    pub tenant_id: Arc<str>,
    pub system_id: Arc<str>,
    pub event_id: Uuid,
    pub trace_id: Arc<str>,
    pub attempt_seq: u16,
    pub occurred_at: Timestamp,
    pub node_id: Arc<str>,
    pub event_type: EventType,
    pub outcome: Outcome,
    pub pins: Pins,
    pub request_digest: Hash32,
    pub response_digest: Hash32,
    pub metrics: Metrics,
}

/// The chained, stored event. Produced by the ingester from an `EmittedEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Gapless per `(tenant_id, system_id)`, allocated by the ingester.
    pub seq: u64,
    pub prev_hash: Hash32,
    pub event_hash: Hash32,
    pub canon_version: Arc<str>,
    /// Not hashed — it is the ingester's own clock, not a property of the
    /// event, and hashing it would make replay non-reproducible.
    pub ingested_at: Timestamp,

    pub emitted: EmittedEvent,

    /// `''` in the MVP. Crypto-shredded payload store is V1-6.
    pub payload_ref: String,
    /// `''` in the MVP. Per-subject key id for GDPR Art. 17 erasure.
    pub subject_key_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub provider: Arc<str>,
    pub http_status: u16,
    pub latency_ms: u32,
    pub ttft_ms: u32,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub error_code: Arc<str>,
}

/// The exact set of fields that gets hashed, in one explicit place.
///
/// Written out by hand rather than derived with `skip` attributes, for two
/// reasons that both bite later:
///
/// - a field added to `AuditEvent` cannot silently join the hashed set; it has
///   to be added here, which is a diff a reviewer will notice
/// - the map keys are literals in this file, so they cannot be changed by
///   renaming a Rust field
///
/// Every column in mvp-plan §4 except `event_hash` (which this produces) and
/// `ingested_at` (the ingester's own clock). `seq` and `prev_hash` *are*
/// included — that is what makes reordering detectable.
#[derive(Debug, Serialize)]
pub struct HashedBody<'a> {
    // identity
    pub tenant_id: &'a str,
    pub system_id: &'a str,
    pub seq: u64,
    pub event_id: Bytes<'a>,
    pub trace_id: &'a str,
    pub attempt_seq: u16,

    // chain
    //
    // `prev_hash` and `canon_version` also prefix the hash input in the chain
    // rule (mvp-plan §4), so they are absorbed twice. Redundant, harmless, and
    // faithful to the frozen spec — which is worth more in week 1 than saving
    // 40 bytes of hashing.
    pub prev_hash: Hash32,
    pub canon_version: &'a str,

    // time / origin
    pub occurred_at: i64,
    pub node_id: &'a str,

    // event
    pub event_type: &'a str,
    pub outcome: &'a str,

    // pins. never null; unknown is the literal string "unknown"
    pub config_generation: u64,
    pub config_hash: Hash32,
    pub system_version: &'a str,
    pub ifu_version: &'a str,
    pub model_id: &'a str,
    pub model_version: &'a str,
    pub prompt_id: &'a str,
    pub prompt_version: &'a str,
    pub policy_id: &'a str,
    pub policy_version: &'a str,
    pub gateway_version: &'a str,
    pub risk_class: &'a str,
    pub resolved_stale: bool,
    pub risk_flags: Vec<&'static str>,

    // payload
    pub request_digest: Hash32,
    pub response_digest: Hash32,
    pub payload_ref: &'a str,
    pub subject_key_id: &'a str,

    // metrics
    pub provider: &'a str,
    pub http_status: u16,
    pub latency_ms: u32,
    pub ttft_ms: u32,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub error_code: &'a str,
}

impl AuditEvent {
    /// Borrowed view of everything that gets hashed.
    #[must_use]
    pub fn hashed_view(&self) -> HashedBody<'_> {
        let e = &self.emitted;
        let p = &e.pins;
        let m = &e.metrics;
        HashedBody {
            tenant_id: &e.tenant_id,
            system_id: &e.system_id,
            seq: self.seq,
            event_id: Bytes(e.event_id.as_bytes()),
            trace_id: &e.trace_id,
            attempt_seq: e.attempt_seq,

            prev_hash: self.prev_hash,
            canon_version: &self.canon_version,

            occurred_at: e.occurred_at.as_micros(),
            node_id: &e.node_id,

            event_type: e.event_type.as_str(),
            outcome: e.outcome.as_str(),

            config_generation: p.config_generation,
            config_hash: p.config_hash,
            system_version: &p.system_version,
            ifu_version: &p.ifu_version,
            model_id: &p.model_id,
            model_version: &p.model_version,
            prompt_id: &p.prompt_id,
            prompt_version: &p.prompt_version,
            policy_id: &p.policy_id,
            policy_version: &p.policy_version,
            gateway_version: &p.gateway_version,
            risk_class: p.risk_class.as_str(),
            resolved_stale: p.resolved_stale,
            // Sorted, so that two nodes that collected the same flags in a
            // different order still hash identically. The set is what matters;
            // the discovery order is not evidence of anything.
            risk_flags: {
                let mut f: Vec<&'static str> = p.risk_flags.iter().map(|r| r.as_str()).collect();
                f.sort_unstable();
                f
            },

            request_digest: e.request_digest,
            response_digest: e.response_digest,
            payload_ref: &self.payload_ref,
            subject_key_id: &self.subject_key_id,

            provider: &m.provider,
            http_status: m.http_status,
            latency_ms: m.latency_ms,
            ttft_ms: m.ttft_ms,
            tokens_in: m.tokens_in,
            tokens_out: m.tokens_out,
            error_code: &m.error_code,
        }
    }

    /// The exact byte sequence that gets hashed.
    pub fn hashed_body(&self) -> Result<Vec<u8>, CanonError> {
        ancre_canon::encode(&self.hashed_view())
    }
}
