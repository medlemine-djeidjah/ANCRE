//! Seq allocation and chaining.

use ancre_canon::Hash32;
use ancre_chain::ChainId;
use ancre_types::{AuditEvent, EmittedEvent};

/// Per-chain cursor. One writer per `(tenant_id, system_id)` — two processes
/// chaining the same chain would fork it, so this is a single-writer design
/// and horizontal scale comes from partitioning chains across ingesters, not
/// from sharing one.
#[derive(Debug)]
pub struct ChainWriter {
    pub chain: ChainId,
    pub next_seq: u64,
    pub head_hash: Hash32,
}

impl ChainWriter {
    /// Resume from ClickHouse on startup: read the max `seq` and its
    /// `event_hash` for this chain. On an empty chain, `seq = 0` and
    /// `head_hash = GENESIS`.
    pub async fn resume(_chain: ChainId) -> Result<Self, IngestError> {
        todo!("M4: SELECT seq, event_hash ORDER BY seq DESC LIMIT 1")
    }

    /// JetStream is at-least-once, so redelivery is normal, not exceptional.
    /// Idempotency is keyed on `event_id`: an already-chained `event_id`
    /// returns its existing event rather than allocating a new `seq`.
    ///
    /// Getting this wrong doesn't error — it silently doubles events and the
    /// chain still verifies. Test redelivery explicitly.
    pub fn append(&mut self, _event: EmittedEvent) -> Result<AuditEvent, IngestError> {
        todo!("M4: dedupe on event_id, assign seq, compute event_hash, advance head")
    }

    /// One per chain per day, traffic or no traffic (mvp-plan §8.4).
    ///
    /// Without it, a system that served nothing and a system that does not
    /// exist are indistinguishable in the record — and "we have no evidence
    /// for this period" must never be the same shape as "there was nothing to
    /// record".
    pub fn heartbeat(&mut self) -> Result<AuditEvent, IngestError> {
        todo!("M4")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("clickhouse: {0}")]
    ClickHouse(String),
    #[error("chain: {0}")]
    Chain(#[from] ancre_chain::ChainError),
    #[error("event {0} was already chained at seq {1}")]
    Duplicate(uuid::Uuid, u64),
}
