//! The event store.
//!
//! A trait, so the pipeline can be tested against a store that fails on
//! command. "ClickHouse is down for ten minutes and the chain still verifies
//! afterwards" is the M4 acceptance criterion, and it should be checkable
//! without a ten-minute outage.

use ancre_types::AuditEvent;

use crate::chain_writer::IngestError;

pub trait EventStore: Send + Sync {
    /// Batch-insert over the native protocol.
    ///
    /// **All or nothing.** A partial insert would leave the chain's head
    /// disagreeing with the store, and the next restart would resume from the
    /// wrong place and fork the chain.
    fn insert(
        &self,
        batch: &[AuditEvent],
    ) -> impl std::future::Future<Output = Result<(), IngestError>> + Send;

    /// The last row for a chain: `(seq, event_hash)`, from one query.
    ///
    /// One query and not two, because two could observe different states and
    /// resume from a `seq` that does not belong to that `head_hash` — which
    /// forks the chain silently.
    fn head(
        &self,
        chain: &ancre_chain::ChainId,
    ) -> impl std::future::Future<Output = Result<Option<(u64, ancre_canon::Hash32)>, IngestError>> + Send;
}

/// A borrowed store is a store.
///
/// `Ingester` takes its store by value so a caller can hand it an owned
/// client; this lets the same caller keep the client and lend it out — which
/// is what any process that both ingests and reads back (the checkpointer, an
/// evidence export) needs to do.
impl<S: EventStore> EventStore for &S {
    fn insert(
        &self,
        batch: &[AuditEvent],
    ) -> impl std::future::Future<Output = Result<(), IngestError>> + Send {
        (**self).insert(batch)
    }

    fn head(
        &self,
        chain: &ancre_chain::ChainId,
    ) -> impl std::future::Future<Output = Result<Option<(u64, ancre_canon::Hash32)>, IngestError>> + Send
    {
        (**self).head(chain)
    }
}

/// Retry policy for a store that is down.
///
/// Backs off, and **never gives up by discarding**. The bus is the buffer: if
/// the store cannot take a batch, the right move is to stop acking so
/// JetStream redelivers, not to drop the events and carry on looking healthy.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub initial: std::time::Duration,
    pub max: std::time::Duration,
    /// Attempts before the batch is handed back to the caller unacked.
    pub attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: std::time::Duration::from_millis(100),
            max: std::time::Duration::from_secs(10),
            attempts: 8,
        }
    }
}

impl RetryPolicy {
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> std::time::Duration {
        let shift = attempt.min(20);
        self.initial.saturating_mul(1u32 << shift).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_then_caps() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for(0), std::time::Duration::from_millis(100));
        assert_eq!(p.delay_for(1), std::time::Duration::from_millis(200));
        assert_eq!(p.delay_for(3), std::time::Duration::from_millis(800));
        assert_eq!(p.delay_for(20), p.max, "must cap rather than overflow");
        assert_eq!(p.delay_for(u32::MAX), p.max);
    }
}
