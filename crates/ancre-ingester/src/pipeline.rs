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

        // Everything that needs the store happens first, before a single event
        // is chained. Both of the calls below can fail, and a half-chained
        // batch that then defers is exactly the state the rollback exists to
        // avoid — so it is cheaper to never enter it.
        let mut ids_by_chain: HashMap<ChainId, Vec<uuid::Uuid>> = HashMap::new();
        for emitted in &batch {
            ids_by_chain
                .entry(ChainId {
                    tenant_id: emitted.tenant_id.to_string(),
                    system_id: emitted.system_id.to_string(),
                })
                .or_default()
                .push(emitted.event_id);
        }

        for (chain, ids) in &ids_by_chain {
            if !self.writers.contains_key(chain) {
                // Resuming needs the store. If it is down, the batch cannot be
                // chained *at all* — guessing a starting seq would fork the
                // chain the moment the store came back. Defer and let the bus
                // redeliver, exactly as a failed insert does.
                let Ok(resumed) = self.store.head(chain).await else {
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

            // The durable dedupe check. Same failure handling as `head`: a
            // store that cannot answer "have I seen these" must not be
            // answered with "probably not", because the cost of guessing wrong
            // is a double-chained event that no later check can undo.
            if self.absorb_stored(chain, ids).await.is_err() {
                report.deferred += 1;
                return Ok(report);
            }
        }

        for emitted in batch {
            let chain = ChainId {
                tenant_id: emitted.tenant_id.to_string(),
                system_id: emitted.system_id.to_string(),
            };

            let writer = self
                .writers
                .get_mut(&chain)
                .expect("a writer was built for every chain in the batch");

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

    /// Fold the store's answer to "do you already hold these?" into the
    /// writer's dedupe memory, so `append` refuses them exactly as it refuses
    /// an id this process wrote itself.
    ///
    /// Skipped entirely while the writer's window is still a complete record
    /// of its chain — a fresh chain under a window that has never evicted,
    /// which is every chain in a new deployment. That is what keeps this from
    /// being a round trip per batch forever.
    async fn absorb_stored(
        &mut self,
        chain: &ChainId,
        ids: &[uuid::Uuid],
    ) -> Result<(), IngestError> {
        let unknown: Vec<uuid::Uuid> = {
            let writer = self
                .writers
                .get(chain)
                .expect("a writer was built for every chain in the batch");
            if writer.window_is_complete() {
                return Ok(());
            }
            ids.iter()
                .copied()
                .filter(|id| !writer.has_seen(*id))
                .collect()
        };

        if unknown.is_empty() {
            return Ok(());
        }

        let hits = self.store.stored(chain, &unknown).await?;
        if let Some(writer) = self.writers.get_mut(chain) {
            for (id, seq) in hits {
                writer.remember_stored(id, seq);
            }
        }
        Ok(())
    }

    /// Rebuild a writer for every chain the store already holds.
    ///
    /// Call once at startup. Without it, `heartbeats` only covers chains this
    /// process has already seen traffic for — so a system that goes quiet
    /// across a restart stops emitting the daily heartbeat that makes its
    /// silence countable, and "no traffic" becomes indistinguishable from "no
    /// such system" all over again (mvp-plan §8.4). The heartbeat exists to
    /// prevent exactly that, so a heartbeat that only covers busy chains is
    /// the one shape it must not have.
    ///
    /// Returns the number of chains seeded. Failure is the caller's to decide
    /// on: an ingester that cannot reach the store at startup can still serve
    /// traffic once it comes back, and traffic builds its own writers.
    pub async fn seed_from_store(&mut self) -> Result<usize, IngestError> {
        let chains = self.store.chains().await?;
        let mut seeded = 0;
        for (chain, last_seq, head) in chains {
            if self.writers.contains_key(&chain) {
                continue;
            }
            self.writers.insert(
                chain.clone(),
                ChainWriter::resume_from(chain, &self.node_id, last_seq, head),
            );
            seeded += 1;
        }
        Ok(seeded)
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
    ///
    /// After `seed_from_store` that is every chain in the store, including the
    /// ones with no traffic — which is the whole point. It also means a
    /// restart mid-day meets chains whose heartbeat is already written and
    /// whose writers have no memory of it, so today's ids go through the
    /// durable check first. Skipping that would append a second heartbeat at a
    /// new seq every time the process restarted.
    pub async fn heartbeats(&mut self, now: Timestamp) -> Result<u64, IngestError> {
        let pending: Vec<(ChainId, uuid::Uuid)> = self
            .writers
            .values()
            .filter_map(|w| w.pending_heartbeat(now).map(|id| (w.chain.clone(), id)))
            .collect();
        for (chain, id) in pending {
            self.absorb_stored(&chain, &[id]).await?;
        }

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
