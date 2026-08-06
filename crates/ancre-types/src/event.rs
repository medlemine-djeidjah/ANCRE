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

use ancre_canon::Hash32;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::pins::Pins;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Error,
    Denied,
    Interrupted,
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
    pub occurred_at: time::OffsetDateTime,
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
    pub ingested_at: time::OffsetDateTime,

    #[serde(flatten)]
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

impl AuditEvent {
    /// The exact byte sequence that gets hashed.
    ///
    /// `hashed_body` is every field **except** `event_hash` and `ingested_at`.
    /// `seq` and `prev_hash` *are* hashed — that is what makes reordering
    /// detectable (mvp-plan §4).
    pub fn hashed_body(&self) -> Result<Vec<u8>, ancre_canon::CanonError> {
        todo!("M1: canonical encode of every field but event_hash and ingested_at")
    }
}
