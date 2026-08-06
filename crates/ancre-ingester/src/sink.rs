//! ClickHouse batch insert.

use ancre_types::AuditEvent;

use crate::chain_writer::IngestError;

#[derive(Debug)]
pub struct ClickHouseSink {
    _client: (),
}

impl ClickHouseSink {
    /// Batch-insert over the native protocol.
    ///
    /// Only ack the JetStream messages **after** the insert commits. Acking
    /// first turns a ClickHouse hiccup into permanent evidence loss, and the
    /// durability requirement is zero audit event loss under normal operation
    /// (PRD §8).
    pub async fn insert(&self, _batch: &[AuditEvent]) -> Result<(), IngestError> {
        todo!("M4")
    }
}

// M4 done-when (mvp-plan §5): kill ClickHouse for ten minutes under load. The
// gateway is unaffected, NATS buffers, the ingester catches up, and the chain
// verifies with no gap.
