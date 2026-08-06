//! The ClickHouse `EventStore`.
//!
//! Two jobs, and the second is the one that can quietly destroy the product:
//!
//! 1. Batch-insert chained events over the native protocol.
//! 2. Read the chain head back, so a restarting ingester resumes where the
//!    store actually is rather than where it thinks it was.
//!
//! The dangerous part is the **flattening**. `AuditEvent` is nested; the table
//! is flat and frozen (mvp-plan §4). Every conversion in this file is
//! therefore hash-critical: if a round trip through ClickHouse changes any
//! hashed field by so much as a whitespace, the chain still verifies inside
//! the ingester and fails for the auditor — the worst possible place to find
//! out. `round_trips_without_changing_the_event_hash` is the test that owns
//! this, and it runs without a database.

use ancre_canon::Hash32;
use ancre_chain::ChainId;
use ancre_types::{
    AuditEvent, EmittedEvent, EventType, Metrics, Outcome, Pins, RiskClass, RiskFlag, Timestamp,
};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::chain_writer::IngestError;
use crate::sink::EventStore;

/// The table, field for field with `deploy/compose/init/clickhouse/001_audit_events.sql`.
///
/// Column order matters: `?fields` expands in declaration order, so this must
/// stay aligned with the DDL. The types are the ones the client maps directly
/// — `[u8; 32]` for `FixedString(32)`, raw `i64` for `DateTime64(6)` (already
/// microseconds, which is exactly what `Timestamp` holds), `String` for
/// `LowCardinality(String)`, `Vec<String>` for `Array(String)`.
#[derive(Debug, Row, Serialize, Deserialize)]
pub struct AuditEventRow {
    // identity
    tenant_id: String,
    system_id: String,
    seq: u64,
    #[serde(with = "clickhouse::serde::uuid")]
    event_id: Uuid,
    trace_id: String,
    attempt_seq: u16,

    // chain
    prev_hash: [u8; 32],
    event_hash: [u8; 32],
    canon_version: String,

    // time / origin
    occurred_at: i64,
    ingested_at: i64,
    node_id: String,

    // event
    event_type: String,
    outcome: String,

    // pins
    config_generation: u64,
    config_hash: [u8; 32],
    system_version: String,
    ifu_version: String,
    model_id: String,
    model_version: String,
    prompt_id: String,
    prompt_version: String,
    policy_id: String,
    policy_version: String,
    gateway_version: String,
    /// `Enum8` is a single byte on the wire, not a name — see
    /// `RiskClass::as_discriminant`. Sending the string form makes the server
    /// reject the whole batch with a row-size mismatch.
    risk_class: i8,
    resolved_stale: u8,
    risk_flags: Vec<String>,

    // payload
    request_digest: [u8; 32],
    response_digest: [u8; 32],
    payload_ref: String,
    subject_key_id: String,

    // metrics
    provider: String,
    http_status: u16,
    latency_ms: u32,
    ttft_ms: u32,
    tokens_in: u32,
    tokens_out: u32,
    error_code: String,
}

impl From<&AuditEvent> for AuditEventRow {
    fn from(e: &AuditEvent) -> Self {
        let m = &e.emitted.metrics;
        let p = &e.emitted.pins;
        Self {
            tenant_id: e.emitted.tenant_id.to_string(),
            system_id: e.emitted.system_id.to_string(),
            seq: e.seq,
            event_id: e.emitted.event_id,
            trace_id: e.emitted.trace_id.to_string(),
            attempt_seq: e.emitted.attempt_seq,

            prev_hash: *e.prev_hash.as_bytes(),
            event_hash: *e.event_hash.as_bytes(),
            canon_version: e.canon_version.to_string(),

            occurred_at: e.emitted.occurred_at.as_micros(),
            ingested_at: e.ingested_at.as_micros(),
            node_id: e.emitted.node_id.to_string(),

            event_type: e.emitted.event_type.as_str().to_string(),
            outcome: e.emitted.outcome.as_str().to_string(),

            config_generation: p.config_generation,
            config_hash: *p.config_hash.as_bytes(),
            system_version: p.system_version.to_string(),
            ifu_version: p.ifu_version.to_string(),
            model_id: p.model_id.to_string(),
            model_version: p.model_version.to_string(),
            prompt_id: p.prompt_id.to_string(),
            prompt_version: p.prompt_version.to_string(),
            policy_id: p.policy_id.to_string(),
            policy_version: p.policy_version.to_string(),
            gateway_version: p.gateway_version.to_string(),
            risk_class: p.risk_class.as_discriminant(),
            resolved_stale: u8::from(p.resolved_stale),
            // Sorted on the way in, matching what `hashed_view` hashes. The
            // set is the evidence; the order it was collected in is not, and
            // storing it unsorted would make a round trip depend on it.
            risk_flags: {
                let mut f: Vec<String> = p
                    .risk_flags
                    .iter()
                    .map(|r| r.as_str().to_string())
                    .collect();
                f.sort_unstable();
                f
            },

            request_digest: *e.emitted.request_digest.as_bytes(),
            response_digest: *e.emitted.response_digest.as_bytes(),
            payload_ref: e.payload_ref.clone(),
            subject_key_id: e.subject_key_id.clone(),

            provider: m.provider.to_string(),
            http_status: m.http_status,
            latency_ms: m.latency_ms,
            ttft_ms: m.ttft_ms,
            tokens_in: m.tokens_in,
            tokens_out: m.tokens_out,
            error_code: m.error_code.to_string(),
        }
    }
}

impl TryFrom<AuditEventRow> for AuditEvent {
    type Error = IngestError;

    /// Refuses rather than guessing.
    ///
    /// An enum value this build does not recognise means the row was written
    /// by a different rule set. Substituting a default would produce an event
    /// that verifies against a hash computed from different bytes — a silent
    /// corruption that surfaces as "your chain is broken" in front of an
    /// auditor. `canon_version` exists so this can be diagnosed; refusing is
    /// what makes it diagnosable.
    fn try_from(r: AuditEventRow) -> Result<Self, IngestError> {
        let unknown = |field: &str, value: &str| {
            IngestError::Store(format!(
                "row at seq {}: unknown {field} {value:?} — written under a \
                 different rule set (canon_version {:?})",
                r.seq, r.canon_version
            ))
        };

        let event_type = EventType::from_wire(&r.event_type)
            .ok_or_else(|| unknown("event_type", &r.event_type))?;
        let outcome =
            Outcome::from_wire(&r.outcome).ok_or_else(|| unknown("outcome", &r.outcome))?;
        let risk_class = RiskClass::from_discriminant(r.risk_class)
            .ok_or_else(|| unknown("risk_class", &r.risk_class.to_string()))?;
        let mut risk_flags = smallvec::SmallVec::new();
        for f in &r.risk_flags {
            risk_flags.push(RiskFlag::from_wire(f).ok_or_else(|| unknown("risk_flag", f))?);
        }

        let system_id: std::sync::Arc<str> = r.system_id.as_str().into();

        Ok(Self {
            seq: r.seq,
            prev_hash: Hash32::from_bytes(r.prev_hash),
            event_hash: Hash32::from_bytes(r.event_hash),
            canon_version: r.canon_version.as_str().into(),
            ingested_at: Timestamp::from_micros(r.ingested_at),
            emitted: EmittedEvent {
                tenant_id: r.tenant_id.as_str().into(),
                system_id: std::sync::Arc::clone(&system_id),
                event_id: r.event_id,
                trace_id: r.trace_id.as_str().into(),
                attempt_seq: r.attempt_seq,
                occurred_at: Timestamp::from_micros(r.occurred_at),
                node_id: r.node_id.as_str().into(),
                event_type,
                outcome,
                pins: Pins {
                    config_generation: r.config_generation,
                    config_hash: Hash32::from_bytes(r.config_hash),
                    // The table has one `system_id` column, and only the
                    // event's copy is hashed (`HashedBody`), so this is
                    // reconstructed from it. Two different values could not
                    // have been distinguished by any verifier anyway.
                    system_id,
                    system_version: r.system_version.as_str().into(),
                    ifu_version: r.ifu_version.as_str().into(),
                    model_id: r.model_id.as_str().into(),
                    model_version: r.model_version.as_str().into(),
                    prompt_id: r.prompt_id.as_str().into(),
                    prompt_version: r.prompt_version.as_str().into(),
                    policy_id: r.policy_id.as_str().into(),
                    policy_version: r.policy_version.as_str().into(),
                    gateway_version: r.gateway_version.as_str().into(),
                    risk_class,
                    resolved_stale: r.resolved_stale != 0,
                    risk_flags,
                },
                request_digest: Hash32::from_bytes(r.request_digest),
                response_digest: Hash32::from_bytes(r.response_digest),
                metrics: Metrics {
                    provider: r.provider.as_str().into(),
                    http_status: r.http_status,
                    latency_ms: r.latency_ms,
                    ttft_ms: r.ttft_ms,
                    tokens_in: r.tokens_in,
                    tokens_out: r.tokens_out,
                    error_code: r.error_code.as_str().into(),
                },
            },
            payload_ref: r.payload_ref,
            subject_key_id: r.subject_key_id,
        })
    }
}

/// The table this writes to. Not configurable: an ingester pointed at a table
/// with a different schema is a corrupted chain, not a deployment option.
const TABLE: &str = "audit_events";

pub struct ClickHouseStore {
    client: clickhouse::Client,
}

impl std::fmt::Debug for ClickHouseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseStore").finish_non_exhaustive()
    }
}

impl ClickHouseStore {
    /// `url` is the HTTP endpoint, typically port 8123.
    #[must_use]
    pub fn new(url: &str, database: &str) -> Self {
        Self {
            client: clickhouse::Client::default()
                .with_url(url)
                .with_database(database),
        }
    }

    #[must_use]
    pub fn with_credentials(mut self, user: &str, password: &str) -> Self {
        self.client = self.client.with_user(user).with_password(password);
        self
    }

    /// Read one event back. For the verifier's export path and for tests.
    pub async fn get(&self, chain: &ChainId, seq: u64) -> Result<Option<AuditEvent>, IngestError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM audit_events \
                 WHERE tenant_id = ? AND system_id = ? AND seq = ?",
            )
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .bind(seq)
            .fetch_all::<AuditEventRow>()
            .await
            .map_err(|e| store_err(&e))?;

        rows.into_iter()
            .next()
            .map(AuditEvent::try_from)
            .transpose()
    }

    /// A whole chain, in `seq` order, for export and verification.
    ///
    /// The `ORDER BY` is load-bearing: `verify_range` walks the chain link by
    /// link, so an unordered read reports violations that are an artefact of
    /// the query rather than the data.
    pub async fn range(
        &self,
        chain: &ChainId,
        seq_from: u64,
        seq_to: u64,
    ) -> Result<Vec<AuditEvent>, IngestError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM audit_events \
                 WHERE tenant_id = ? AND system_id = ? AND seq BETWEEN ? AND ? \
                 ORDER BY seq",
            )
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .bind(seq_from)
            .bind(seq_to)
            .fetch_all::<AuditEventRow>()
            .await
            .map_err(|e| store_err(&e))?;

        rows.into_iter().map(AuditEvent::try_from).collect()
    }
}

/// The head query's shape. A named row rather than a tuple, because the
/// client's `Row` derive is what carries the column names into the wire
/// format — a tuple has none to carry.
///
/// The fields are **not** named `seq` and `event_hash`. Aliasing `max(seq)`
/// back to `seq` makes ClickHouse resolve the `seq` inside `argMax(...)` to
/// the alias rather than the column, and it refuses the query as an aggregate
/// inside an aggregate.
#[derive(Debug, Row, Deserialize)]
struct HeadRow {
    head_seq: u64,
    head_hash: [u8; 32],
}

fn store_err(e: &clickhouse::error::Error) -> IngestError {
    IngestError::Store(e.to_string())
}

impl EventStore for ClickHouseStore {
    /// One `INSERT` per batch over the native protocol.
    ///
    /// `end()` is what actually commits, and its result is the one that
    /// matters — a `write` that succeeded into a buffer proves nothing. The
    /// caller treats any error here as "this batch was not stored", rolls the
    /// chain back and leaves the batch unacked, so a partial insert costs a
    /// redelivery and never a gap.
    async fn insert(&self, batch: &[AuditEvent]) -> Result<(), IngestError> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut insert = self
            .client
            .insert::<AuditEventRow>(TABLE)
            .map_err(|e| store_err(&e))?;
        for event in batch {
            insert
                .write(&AuditEventRow::from(event))
                .await
                .map_err(|e| store_err(&e))?;
        }
        insert.end().await.map_err(|e| store_err(&e))
    }

    /// The chain head: `(seq, event_hash)` of the highest-`seq` row.
    ///
    /// **One query, not two.** Two queries — `max(seq)` then "the hash at that
    /// seq" — can observe different states if a batch lands between them, and
    /// resuming from a `seq` that does not belong to its `head_hash` forks the
    /// chain silently. `argMax` keeps them together.
    async fn head(&self, chain: &ChainId) -> Result<Option<(u64, Hash32)>, IngestError> {
        let rows = self
            .client
            .query(
                "SELECT max(seq) AS head_seq, argMax(event_hash, seq) AS head_hash \
                 FROM audit_events \
                 WHERE tenant_id = ? AND system_id = ?",
            )
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .fetch_all::<HeadRow>()
            .await
            .map_err(|e| store_err(&e))?;

        // An empty chain aggregates to (0, zeroes) rather than to no rows.
        Ok(rows
            .into_iter()
            .next()
            .filter(|h| h.head_seq > 0)
            .map(|h| (h.head_seq, Hash32::from_bytes(h.head_hash))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancre_canon::GENESIS;

    fn event(seq: u64) -> AuditEvent {
        let mut e = ancre_types::fixtures::event(seq, GENESIS);
        e.emitted.pins.risk_flags = smallvec::smallvec![
            RiskFlag::UnpinnedModel,
            RiskFlag::StaleConfig,
            RiskFlag::PinOverridden,
        ];
        ancre_chain::seal(&mut e).unwrap();
        e
    }

    /// The test this module exists for.
    ///
    /// Every hashed field has to survive nesting → flat row → nesting
    /// untouched. If it does not, the chain verifies inside the ingester and
    /// fails on the auditor's laptop, which is the worst place to discover it.
    #[test]
    fn round_trips_without_changing_the_event_hash() {
        for seq in 1..=200 {
            let original = event(seq);
            let back = AuditEvent::try_from(AuditEventRow::from(&original)).unwrap();

            assert_eq!(
                ancre_chain::event_hash(&back).unwrap(),
                original.event_hash,
                "seq {seq}: the stored form does not rehash to the stored hash"
            );
            assert_eq!(back.hashed_body().unwrap(), original.hashed_body().unwrap());
        }
    }

    /// Not just the hashed set — everything an auditor is shown.
    #[test]
    fn round_trips_the_unhashed_fields_too() {
        let original = event(7);
        let back = AuditEvent::try_from(AuditEventRow::from(&original)).unwrap();

        assert_eq!(back.ingested_at, original.ingested_at);
        assert_eq!(back.event_hash, original.event_hash);
        assert_eq!(back.prev_hash, original.prev_hash);
        assert_eq!(back.emitted.event_id, original.emitted.event_id);
        assert_eq!(&*back.canon_version, &*original.canon_version);
        assert_eq!(back.payload_ref, original.payload_ref);
        assert_eq!(back.subject_key_id, original.subject_key_id);
    }

    /// Risk flags are a set. Two nodes that collected them in different orders
    /// must produce the same stored row, or the round trip depends on
    /// discovery order.
    #[test]
    fn risk_flags_are_stored_sorted() {
        let e = event(1);
        let row = AuditEventRow::from(&e);
        let mut sorted = row.risk_flags.clone();
        sorted.sort_unstable();
        assert_eq!(row.risk_flags, sorted);
        assert_eq!(row.risk_flags.len(), 3);
    }

    /// A row written under a rule set this build does not know must not be
    /// reinterpreted. Guessing here produces an event whose hash cannot match
    /// its own stored `event_hash`.
    #[test]
    fn an_unrecognised_enum_is_refused_rather_than_defaulted() {
        for (field, bad) in [
            ("event_type", "llm.request.v2"),
            ("outcome", "partially-ok"),
            ("risk_class", "9"),
        ] {
            let mut row = AuditEventRow::from(&event(1));
            match field {
                "event_type" => row.event_type = bad.into(),
                "outcome" => row.outcome = bad.into(),
                _ => row.risk_class = bad.parse().unwrap(),
            }

            let err = AuditEvent::try_from(row).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(field) && msg.contains(bad), "{msg}");
        }
    }

    #[test]
    fn an_unrecognised_risk_flag_is_refused() {
        let mut row = AuditEventRow::from(&event(1));
        row.risk_flags = vec!["invented_flag".into()];
        assert!(AuditEvent::try_from(row).is_err());
    }

    #[test]
    fn the_bool_column_survives_both_ways() {
        let mut e = event(1);
        e.emitted.pins.resolved_stale = true;
        ancre_chain::seal(&mut e).unwrap();

        let back = AuditEvent::try_from(AuditEventRow::from(&e)).unwrap();
        assert!(back.emitted.pins.resolved_stale);
        assert_eq!(ancre_chain::event_hash(&back).unwrap(), e.event_hash);
    }
}
