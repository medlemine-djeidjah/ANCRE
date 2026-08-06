//! Checkpoint scheduling.
//!
//! Every N = 10 000 events or T = 5 minutes, per chain: read the range, compute
//! the tree root, sign it, store it (mvp-plan §4).
//!
//! The signing itself lives in `ancre-chain` and is verifiable offline. What is
//! here is the part that decides *when*, and the only interesting property of
//! that decision is the one it cannot make: **falling behind is not an error.**
//! A checkpoint covering a larger range is still a valid checkpoint. But the
//! gap between a chain's head and its last signed checkpoint is exactly the
//! window in which tampering would go unattested, so the lag is measured and
//! reported, and it is the thing to alert on.

use std::time::Duration;

use ancre_canon::Hash32;
use ancre_chain::{ChainId, Checkpoint, CheckpointSigner};
use ancre_types::Timestamp;

use crate::registry::ControlError;

/// Seal after this many unsealed events, whatever the clock says.
pub const CHECKPOINT_EVERY_N: u64 = 10_000;
/// Seal after this long, whatever the traffic is.
pub const CHECKPOINT_EVERY_T: Duration = Duration::from_secs(300);

/// The chain, as the checkpointer needs to see it. ClickHouse in production.
pub trait ChainSource: Send + Sync {
    /// Chains with at least one event. Ordered, so two control-plane replicas
    /// walk them in the same sequence.
    ///
    /// ```sql
    /// SELECT DISTINCT tenant_id, system_id FROM audit_events
    ///  ORDER BY tenant_id, system_id;
    /// ```
    fn chains(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ChainId>, ControlError>> + Send;

    /// Highest `seq` written for a chain.
    fn head_seq(
        &self,
        chain: &ChainId,
    ) -> impl std::future::Future<Output = Result<Option<u64>, ControlError>> + Send;

    /// Event hashes for `seq_from..=seq_to`, in `seq` order.
    ///
    /// ```sql
    /// SELECT event_hash FROM audit_events
    ///  WHERE tenant_id = $1 AND system_id = $2 AND seq BETWEEN $3 AND $4
    ///  ORDER BY seq;
    /// ```
    ///
    /// The `ORDER BY` is load-bearing: the tree root is order-dependent, so an
    /// unordered read produces a signature over a root no verifier will ever
    /// reproduce — and the failure surfaces at audit time, not at write time.
    fn leaves(
        &self,
        chain: &ChainId,
        seq_from: u64,
        seq_to: u64,
    ) -> impl std::future::Future<Output = Result<Vec<Hash32>, ControlError>> + Send;
}

/// Where signed checkpoints live. Postgres in production.
///
/// ```sql
/// CREATE TABLE checkpoints (
///   tenant_id  text   NOT NULL,
///   system_id  text   NOT NULL,
///   seq_from   bigint NOT NULL,
///   seq_to     bigint NOT NULL,
///   root_hash  bytea  NOT NULL,
///   built_at   timestamptz NOT NULL,
///   key_id     text   NOT NULL,
///   signature  bytea  NOT NULL,
///   PRIMARY KEY (tenant_id, system_id, seq_to)
/// );
/// ```
///
/// Postgres and not ClickHouse on purpose: the checkpoints are the attestation
/// over the event store, and storing them in the store they attest to hands
/// anyone who can rewrite the events the ability to re-sign them too.
pub trait CheckpointStore: Send + Sync {
    /// The last `seq` covered by a checkpoint for this chain, and when it was
    /// sealed. `None` if the chain has never been checkpointed.
    fn last_sealed(
        &self,
        chain: &ChainId,
    ) -> impl std::future::Future<Output = Result<Option<(u64, Timestamp)>, ControlError>> + Send;

    fn put(
        &self,
        checkpoint: Checkpoint,
    ) -> impl std::future::Future<Output = Result<(), ControlError>> + Send;

    /// Every checkpoint for a chain, oldest first. Served to the verifier.
    fn list(
        &self,
        chain: &ChainId,
    ) -> impl std::future::Future<Output = Result<Vec<Checkpoint>, ControlError>> + Send;
}

/// A public key and the window it was valid in.
///
/// Rotation must not invalidate old checkpoints, so the export carries every
/// key that was ever valid — an auditor verifying a two-year-old range needs
/// the key that signed it, not the current one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublicKeyRecord {
    pub key_id: String,
    /// Hex, because this is copied by hand out of an evidence pack.
    pub public_key: String,
    pub valid_from: Timestamp,
    /// `None` while this is the active key.
    pub valid_to: Option<Timestamp>,
}

/// One tick's work.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SealReport {
    pub chains_examined: usize,
    pub checkpoints_written: usize,
    pub events_sealed: u64,
    /// Largest unsealed run left behind, across all chains. The metric to
    /// alert on — see the module docs.
    pub max_lag: u64,
}

pub struct Checkpointer<S, C> {
    source: S,
    store: C,
    signer: CheckpointSigner,
    every_n: u64,
    every_t: Duration,
}

impl<S, C> std::fmt::Debug for Checkpointer<S, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpointer")
            .field("key_id", &self.signer.key_id())
            .field("every_n", &self.every_n)
            .field("every_t", &self.every_t)
            .finish_non_exhaustive()
    }
}

impl<S: ChainSource, C: CheckpointStore> Checkpointer<S, C> {
    pub fn new(source: S, store: C, signer: CheckpointSigner) -> Self {
        Self {
            source,
            store,
            signer,
            every_n: CHECKPOINT_EVERY_N,
            every_t: CHECKPOINT_EVERY_T,
        }
    }

    #[must_use]
    pub fn with_policy(mut self, every_n: u64, every_t: Duration) -> Self {
        self.every_n = every_n;
        self.every_t = every_t;
        self
    }

    /// Seal every chain that is due. Idempotent: a chain with nothing new is
    /// skipped, so running this more often than the policy costs reads and
    /// writes nothing.
    ///
    /// `now` is passed in rather than read from the clock so the time-based
    /// trigger is testable without sleeping for five minutes. It is compared
    /// against `built_at` on the last stored checkpoint, which the signer
    /// stamps from the wall clock — so `now` must come from that same clock.
    /// Passing a fabricated one only moves the next seal earlier or later; it
    /// cannot make a checkpoint claim a time it was not signed at, because the
    /// signed body carries the signer's own timestamp.
    pub async fn tick(&self, now: Timestamp) -> Result<SealReport, ControlError> {
        let mut report = SealReport::default();

        for chain in self.source.chains().await? {
            report.chains_examined += 1;
            let Some(head) = self.source.head_seq(&chain).await? else {
                continue;
            };

            let last = self.store.last_sealed(&chain).await?;
            let seq_from = last.map_or(1, |(seq, _)| seq + 1);
            if seq_from > head {
                continue;
            }

            let pending = head - seq_from + 1;
            let elapsed = last.map(|(_, at)| micros_between(at, now));
            let due = pending >= self.every_n || elapsed.is_none_or(|e| e >= self.every_t);
            if !due {
                report.max_lag = report.max_lag.max(pending);
                continue;
            }

            let leaves = self.source.leaves(&chain, seq_from, head).await?;
            if leaves.len() as u64 != pending {
                // The range moved, or a read came back short. Signing it
                // anyway would attest a root that covers a different set of
                // events than the one the body claims — the signature would be
                // valid and the statement false, which is worse than no
                // checkpoint.
                return Err(ControlError::Invalid(format!(
                    "chain {chain}: expected {pending} leaves for {seq_from}..={head}, read {}",
                    leaves.len()
                )));
            }

            let cp =
                self.signer
                    .seal_range(&chain.tenant_id, &chain.system_id, seq_from, &leaves)?;
            self.store.put(cp).await?;

            report.checkpoints_written += 1;
            report.events_sealed += pending;
        }

        Ok(report)
    }

    /// Unsealed events per chain, for the metric.
    pub async fn lag(&self) -> Result<Vec<(ChainId, u64)>, ControlError> {
        let mut out = Vec::new();
        for chain in self.source.chains().await? {
            let head = self.source.head_seq(&chain).await?.unwrap_or(0);
            let sealed = self
                .store
                .last_sealed(&chain)
                .await?
                .map_or(0, |(seq, _)| seq);
            out.push((chain, head.saturating_sub(sealed)));
        }
        Ok(out)
    }

    #[must_use]
    pub fn signer(&self) -> &CheckpointSigner {
        &self.signer
    }

    /// Give the pieces back. The checkpoint store is also read by the API, and
    /// handing it over is cheaper than putting an `Arc` around a type whose
    /// only sharing requirement is this one.
    #[must_use]
    pub fn into_parts(self) -> (S, C, CheckpointSigner) {
        (self.source, self.store, self.signer)
    }
}

/// Saturating, because a checkpoint timestamped in the future — clock skew
/// between control-plane replicas — must read as "no time has passed" and not
/// wrap into a very large interval that makes everything look due.
fn micros_between(from: Timestamp, to: Timestamp) -> Duration {
    let delta = to.as_micros().saturating_sub(from.as_micros());
    Duration::from_micros(u64::try_from(delta).unwrap_or(0))
}

/// Retention floor. Un-lowerable by configuration — Articles 19 and 26(6) put
/// a ≥6-month duty on providers and deployers, and a product that lets a
/// customer set 30 days has helped them break the law with a form field.
///
/// V1-7. The constant lives here now so no MVP code path can quietly assume a
/// shorter one.
pub const RETENTION_FLOOR_DAYS: u32 = 180;
// Checked at compile time, not in a test: a runtime assertion on a constant is
// one someone can delete along with the test that runs it.
const _: () = assert!(RETENTION_FLOOR_DAYS >= 180);
pub const RETENTION_DEFAULT_CEILING_YEARS: u32 = 7;

#[cfg(feature = "testing")]
pub mod testing {
    //! In-memory chain and checkpoint stores.

    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{
        ChainId, ChainSource, Checkpoint, CheckpointStore, ControlError, Hash32, Timestamp,
    };

    #[derive(Debug, Default)]
    pub struct MemoryChain {
        /// chain → leaf hashes, seq 1..=len.
        chains: Mutex<HashMap<ChainId, Vec<Hash32>>>,
    }

    impl MemoryChain {
        /// Append `n` events to a chain. The hashes only have to be distinct
        /// and reproducible — the tree does not care what they mean.
        pub fn append(&self, chain: &ChainId, n: u64) {
            let mut chains = self.chains.lock().unwrap();
            let leaves = chains.entry(chain.clone()).or_default();
            let base = leaves.len() as u64;
            for i in 0..n {
                leaves.push(ancre_canon::hash_bytes(
                    format!("{chain}/{}", base + i + 1).as_bytes(),
                ));
            }
        }
    }

    impl ChainSource for MemoryChain {
        async fn chains(&self) -> Result<Vec<ChainId>, ControlError> {
            let mut ids: Vec<_> = self.chains.lock().unwrap().keys().cloned().collect();
            ids.sort();
            Ok(ids)
        }

        async fn head_seq(&self, chain: &ChainId) -> Result<Option<u64>, ControlError> {
            Ok(self
                .chains
                .lock()
                .unwrap()
                .get(chain)
                .map(|l| l.len() as u64))
        }

        async fn leaves(
            &self,
            chain: &ChainId,
            seq_from: u64,
            seq_to: u64,
        ) -> Result<Vec<Hash32>, ControlError> {
            let chains = self.chains.lock().unwrap();
            let all = chains
                .get(chain)
                .ok_or_else(|| ControlError::Db(format!("no such chain: {chain}")))?;
            let from = usize::try_from(seq_from.saturating_sub(1)).unwrap_or(usize::MAX);
            let to = usize::try_from(seq_to).unwrap_or(usize::MAX).min(all.len());
            Ok(all.get(from..to).unwrap_or_default().to_vec())
        }
    }

    #[derive(Debug, Default)]
    pub struct MemoryCheckpoints {
        by_chain: Mutex<HashMap<ChainId, Vec<Checkpoint>>>,
    }

    impl MemoryCheckpoints {
        #[must_use]
        pub fn count(&self) -> usize {
            self.by_chain.lock().unwrap().values().map(Vec::len).sum()
        }
    }

    impl CheckpointStore for MemoryCheckpoints {
        async fn last_sealed(
            &self,
            chain: &ChainId,
        ) -> Result<Option<(u64, Timestamp)>, ControlError> {
            Ok(self
                .by_chain
                .lock()
                .unwrap()
                .get(chain)
                .and_then(|cps| cps.last())
                .map(|cp| (cp.body.seq_to, cp.body.built_at)))
        }

        async fn put(&self, checkpoint: Checkpoint) -> Result<(), ControlError> {
            let chain = ChainId {
                tenant_id: checkpoint.body.tenant_id.clone(),
                system_id: checkpoint.body.system_id.clone(),
            };
            self.by_chain
                .lock()
                .unwrap()
                .entry(chain)
                .or_default()
                .push(checkpoint);
            Ok(())
        }

        async fn list(&self, chain: &ChainId) -> Result<Vec<Checkpoint>, ControlError> {
            Ok(self
                .by_chain
                .lock()
                .unwrap()
                .get(chain)
                .cloned()
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{MemoryChain, MemoryCheckpoints};
    use super::*;

    fn chain(system: &str) -> ChainId {
        ChainId {
            tenant_id: "acme".into(),
            system_id: system.into(),
        }
    }

    fn checkpointer(
        source: MemoryChain,
        store: MemoryCheckpoints,
    ) -> Checkpointer<MemoryChain, MemoryCheckpoints> {
        Checkpointer::new(
            source,
            store,
            CheckpointSigner::from_bytes([7u8; 32], "cp-test".into()),
        )
    }

    /// The wall clock, because that is what the signer stamps checkpoints
    /// with — a fixed constant here would sit hours behind `built_at`, and no
    /// timer would ever fire. See `tick`.
    fn t0() -> Timestamp {
        Timestamp::now()
    }

    fn after(base: Timestamp, secs: i64) -> Timestamp {
        Timestamp::from_micros(base.as_micros() + secs * 1_000_000)
    }

    #[tokio::test]
    async fn the_first_tick_seals_from_seq_one() {
        let source = MemoryChain::default();
        source.append(&chain("hr-screening"), 50);
        let cp = checkpointer(source, MemoryCheckpoints::default());

        let report = cp.tick(t0()).await.unwrap();
        assert_eq!(report.checkpoints_written, 1);
        assert_eq!(report.events_sealed, 50);

        let cps = cp.store.list(&chain("hr-screening")).await.unwrap();
        assert_eq!(cps[0].body.seq_from, 1);
        assert_eq!(cps[0].body.seq_to, 50);
    }

    #[tokio::test]
    async fn a_sealed_range_verifies_under_the_signing_key() {
        let source = MemoryChain::default();
        source.append(&chain("hr-screening"), 500);
        let cp = checkpointer(source, MemoryCheckpoints::default());
        cp.tick(t0()).await.unwrap();

        let cps = cp.store.list(&chain("hr-screening")).await.unwrap();
        assert!(ancre_chain::verify_checkpoint(&cps[0], &cp.signer().verifying_key()).is_ok());
    }

    /// Ranges must abut exactly. A one-event overlap or gap between
    /// consecutive checkpoints leaves events either attested twice under
    /// different roots or not at all.
    #[tokio::test]
    async fn consecutive_checkpoints_abut_without_a_gap_or_an_overlap() {
        let source = MemoryChain::default();
        let c = chain("hr-screening");
        source.append(&c, 100);
        let cp = checkpointer(source, MemoryCheckpoints::default()).with_policy(10, Duration::ZERO);

        cp.tick(t0()).await.unwrap();
        cp.source.append(&c, 100);
        cp.tick(after(t0(), 1)).await.unwrap();

        let cps = cp.store.list(&c).await.unwrap();
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0].body.seq_to, 100);
        assert_eq!(cps[1].body.seq_from, 101);
        assert_eq!(cps[1].body.seq_to, 200);
    }

    #[tokio::test]
    async fn a_chain_with_nothing_new_is_not_resealed() {
        let source = MemoryChain::default();
        source.append(&chain("hr-screening"), 30);
        let cp = checkpointer(source, MemoryCheckpoints::default());

        cp.tick(t0()).await.unwrap();
        let second = cp.tick(after(t0(), 3600)).await.unwrap();

        assert_eq!(second.checkpoints_written, 0);
        assert_eq!(cp.store.count(), 1, "an empty range must not be signed");
    }

    #[tokio::test]
    async fn the_event_count_trigger_fires_before_the_timer() {
        let source = MemoryChain::default();
        let c = chain("hr-screening");
        source.append(&c, 1);
        let cp =
            checkpointer(source, MemoryCheckpoints::default()).with_policy(100, CHECKPOINT_EVERY_T);
        cp.tick(t0()).await.unwrap(); // first seal: no previous checkpoint

        // 99 more, one second later: neither trigger has fired.
        cp.source.append(&c, 99);
        assert_eq!(
            cp.tick(after(t0(), 1)).await.unwrap().checkpoints_written,
            0
        );

        // The hundredth crosses N, with the timer nowhere near.
        cp.source.append(&c, 1);
        assert_eq!(
            cp.tick(after(t0(), 2)).await.unwrap().checkpoints_written,
            1
        );
    }

    /// A silent chain still gets sealed on the timer. Otherwise a system with
    /// one event a week would go unattested for a week — and the whole point
    /// of the timer is that low traffic is not less auditable.
    #[tokio::test]
    async fn the_timer_fires_on_a_chain_too_quiet_to_reach_n() {
        let source = MemoryChain::default();
        let c = chain("hr-screening");
        source.append(&c, 1);
        let cp = checkpointer(source, MemoryCheckpoints::default());
        cp.tick(t0()).await.unwrap();

        cp.source.append(&c, 1);
        assert_eq!(
            cp.tick(after(t0(), 60)).await.unwrap().checkpoints_written,
            0
        );
        assert_eq!(
            cp.tick(after(t0(), 301)).await.unwrap().checkpoints_written,
            1
        );
    }

    #[tokio::test]
    async fn every_chain_is_sealed_independently() {
        let source = MemoryChain::default();
        source.append(&chain("hr-screening"), 20);
        source.append(&chain("credit-scoring"), 5);
        let cp = checkpointer(source, MemoryCheckpoints::default());

        let report = cp.tick(t0()).await.unwrap();
        assert_eq!(report.chains_examined, 2);
        assert_eq!(report.checkpoints_written, 2);
        assert_eq!(report.events_sealed, 25);
    }

    /// Falling behind is not an error, but it must be visible.
    #[tokio::test]
    async fn lag_is_reported_for_a_chain_that_is_not_yet_due() {
        let source = MemoryChain::default();
        let c = chain("hr-screening");
        source.append(&c, 10);
        let cp = checkpointer(source, MemoryCheckpoints::default())
            .with_policy(1_000_000, CHECKPOINT_EVERY_T);
        cp.tick(t0()).await.unwrap();

        cp.source.append(&c, 4_000);
        let report = cp.tick(after(t0(), 1)).await.unwrap();
        assert_eq!(report.checkpoints_written, 0);
        assert_eq!(report.max_lag, 4_000);
        assert_eq!(cp.lag().await.unwrap(), vec![(c, 4_000)]);
    }

    #[tokio::test]
    async fn a_short_read_refuses_to_sign_rather_than_attesting_the_wrong_range() {
        struct ShortSource(MemoryChain);
        impl ChainSource for ShortSource {
            async fn chains(&self) -> Result<Vec<ChainId>, ControlError> {
                self.0.chains().await
            }
            async fn head_seq(&self, chain: &ChainId) -> Result<Option<u64>, ControlError> {
                self.0.head_seq(chain).await
            }
            async fn leaves(
                &self,
                chain: &ChainId,
                seq_from: u64,
                seq_to: u64,
            ) -> Result<Vec<Hash32>, ControlError> {
                // One row short, the way a range that moved under a reader
                // would come back.
                let mut l = self.0.leaves(chain, seq_from, seq_to).await?;
                l.pop();
                Ok(l)
            }
        }

        let inner = MemoryChain::default();
        inner.append(&chain("hr-screening"), 40);
        let cp = Checkpointer::new(
            ShortSource(inner),
            MemoryCheckpoints::default(),
            CheckpointSigner::from_bytes([7u8; 32], "cp-test".into()),
        );

        assert!(matches!(cp.tick(t0()).await, Err(ControlError::Invalid(_))));
        assert_eq!(cp.store.count(), 0);
    }

    /// Clock skew between replicas must not make everything look due.
    #[tokio::test]
    async fn a_checkpoint_timestamped_in_the_future_does_not_wrap_the_interval() {
        assert_eq!(micros_between(after(t0(), 60), t0()), Duration::ZERO);
    }
}
