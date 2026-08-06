//! Consume → chain → store.
//!
//! The loop that has to survive a store outage without losing an event or
//! forking a chain.

use std::collections::HashMap;

use ancre_chain::ChainId;
use ancre_types::{AuditEvent, EmittedEvent, Timestamp};

use crate::chain_writer::{ChainWriter, IngestError};
use crate::sink::{EventStore, RetryPolicy};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub chained: u64,
    pub inserted: u64,
    pub duplicates: u64,
    /// Batches handed back unacked, for the bus to redeliver.
    pub deferred: u64,
    pub retries: u64,
}

/// One ingester instance. Owns a `ChainWriter` per chain it is responsible for.
pub struct Ingester<S: EventStore> {
    store: S,
    node_id: String,
    writers: HashMap<ChainId, ChainWriter>,
    retry: RetryPolicy,
}

impl<S: EventStore> std::fmt::Debug for Ingester<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ingester")
            .field("node_id", &self.node_id)
            .field("chains", &self.writers.len())
            .finish_non_exhaustive()
    }
}

impl<S: EventStore> Ingester<S> {
    pub fn new(store: S, node_id: impl Into<String>) -> Self {
        Self {
            store,
            node_id: node_id.into(),
            writers: HashMap::new(),
            retry: RetryPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Chain and store one batch of emitted events.
    ///
    /// On a store failure the whole batch is **rolled back** — the chain
    /// writers are rewound to where they were before the batch — and the
    /// caller is told to leave it unacked so the bus redelivers. Advancing the
    /// chain for events that were never stored is how a restart resumes from a
    /// head the store has never seen, which forks the chain silently.
    pub async fn ingest(&mut self, batch: Vec<EmittedEvent>) -> Result<IngestReport, IngestError> {
        let mut report = IngestReport::default();
        let mut chained: Vec<AuditEvent> = Vec::with_capacity(batch.len());

        // Where each writer stood before this batch, so a failure can undo it.
        let mut rollback: HashMap<ChainId, (u64, ancre_canon::Hash32)> = HashMap::new();

        for emitted in batch {
            let chain = ChainId {
                tenant_id: emitted.tenant_id.to_string(),
                system_id: emitted.system_id.to_string(),
            };

            if !self.writers.contains_key(&chain) {
                // Resuming needs the store. If it is down, the batch cannot be
                // chained *at all* — guessing a starting seq would fork the
                // chain the moment the store came back. Defer and let the bus
                // redeliver, exactly as a failed insert does.
                let Ok(resumed) = self.store.head(&chain).await else {
                    self.rewind(rollback);
                    report.deferred += 1;
                    return Ok(report);
                };
                let writer = match resumed {
                    Some((seq, head)) => {
                        ChainWriter::resume_from(chain.clone(), &self.node_id, seq, head)
                    }
                    None => ChainWriter::new(chain.clone(), &self.node_id),
                };
                self.writers.insert(chain.clone(), writer);
            }

            let writer = self
                .writers
                .get_mut(&chain)
                .expect("just inserted if missing");

            rollback
                .entry(chain.clone())
                .or_insert_with(|| (writer.next_seq(), writer.head_hash()));

            match writer.append(emitted) {
                Ok(event) => {
                    report.chained += 1;
                    chained.push(event);
                }
                // Redelivery. Normal, not an error — JetStream is at-least-once.
                Err(IngestError::Duplicate(..)) => report.duplicates += 1,
                Err(e) => return Err(e),
            }
        }

        if chained.is_empty() {
            return Ok(report);
        }

        if self.insert_with_retry(&chained, &mut report).await.is_ok() {
            report.inserted += chained.len() as u64;
        } else {
            self.rewind(rollback);
            report.deferred += 1;
        }
        Ok(report)
    }

    /// Put every touched chain back where it was before the batch.
    ///
    /// Events that were chained but never stored must not leave the writer
    /// advanced past them: the next restart would resume from a head the store
    /// has never seen, and the chain forks silently.
    fn rewind(&mut self, rollback: HashMap<ChainId, (u64, ancre_canon::Hash32)>) {
        for (chain, (seq, head)) in rollback {
            if let Some(writer) = self.writers.get_mut(&chain) {
                writer.rewind_to(seq, head);
            }
        }
    }

    async fn insert_with_retry(
        &self,
        batch: &[AuditEvent],
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        let mut last = None;
        for attempt in 0..self.retry.attempts {
            match self.store.insert(batch).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    report.retries += 1;
                    tokio::time::sleep(self.retry.delay_for(attempt)).await;
                }
            }
        }
        Err(last.unwrap_or_else(|| IngestError::Store("no attempts configured".into())))
    }

    /// Daily heartbeats for every chain this ingester knows about.
    pub async fn heartbeats(&mut self, now: Timestamp) -> Result<u64, IngestError> {
        let mut written = Vec::new();
        for writer in self.writers.values_mut() {
            if let Some(Ok(event)) = writer.heartbeat(now) {
                written.push(event);
            }
        }
        if written.is_empty() {
            return Ok(0);
        }
        self.store.insert(&written).await?;
        Ok(written.len() as u64)
    }

    #[must_use]
    pub fn chain_count(&self) -> usize {
        self.writers.len()
    }
}
