//! M4's acceptance criterion (mvp-plan §5):
//!
//! > Kill ClickHouse for ten minutes under load; the gateway is unaffected,
//! > the ingester catches up, and the chain verifies with no gap.
//!
//! The store here is a fake that can be told to fail, so the outage is
//! instantaneous rather than ten real minutes. What that costs in fidelity it
//! buys back in being run on every commit — an acceptance test nobody runs is
//! a paragraph, not a test.
//!
//! The gateway half of the criterion is covered in `ancre-gateway`: telemetry
//! is a bounded channel the request path never awaits.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ancre_canon::{GENESIS, Hash32};
use ancre_chain::{ChainId, verify_range};
use ancre_ingester::chain_writer::IngestError;
use ancre_ingester::pipeline::Ingester;
use ancre_ingester::sink::{EventStore, RetryPolicy};
use ancre_types::{AuditEvent, EmittedEvent};

/// An in-memory store that can be knocked over on command.
#[derive(Debug, Default)]
struct FlakyStore {
    rows: Mutex<Vec<AuditEvent>>,
    down: AtomicBool,
    inserts_attempted: AtomicU64,
    contacts: AtomicU64,
    /// Durable dedupe lookups, so a test can assert the fast path skips them.
    lookups: AtomicU64,
}

impl FlakyStore {
    fn kill(&self) {
        self.down.store(true, Ordering::SeqCst);
    }

    fn revive(&self) {
        self.down.store(false, Ordering::SeqCst);
    }

    fn rows(&self) -> Vec<AuditEvent> {
        self.rows.lock().unwrap().clone()
    }

    fn chain_rows(&self, chain: &ChainId) -> Vec<AuditEvent> {
        let mut rows: Vec<_> = self
            .rows()
            .into_iter()
            .filter(|r| {
                *r.emitted.tenant_id == *chain.tenant_id && *r.emitted.system_id == *chain.system_id
            })
            .collect();
        rows.sort_by_key(|r| r.seq);
        rows
    }
}

impl EventStore for &FlakyStore {
    async fn insert(&self, batch: &[AuditEvent]) -> Result<(), IngestError> {
        self.inserts_attempted.fetch_add(1, Ordering::SeqCst);
        self.contacts.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        // All or nothing, like a real batch insert.
        self.rows.lock().unwrap().extend_from_slice(batch);
        Ok(())
    }

    async fn head(&self, chain: &ChainId) -> Result<Option<(u64, Hash32)>, IngestError> {
        self.contacts.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        Ok(self.chain_rows(chain).last().map(|r| (r.seq, r.event_hash)))
    }

    async fn stored(
        &self,
        chain: &ChainId,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, u64)>, IngestError> {
        self.contacts.fetch_add(1, Ordering::SeqCst);
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        Ok(self
            .chain_rows(chain)
            .into_iter()
            .filter(|r| ids.contains(&r.emitted.event_id))
            .map(|r| (r.emitted.event_id, r.seq))
            .collect())
    }

    async fn chains(&self) -> Result<Vec<(ChainId, u64, Hash32)>, IngestError> {
        self.contacts.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        let mut heads: HashMap<ChainId, (u64, Hash32)> = HashMap::new();
        for r in self.rows() {
            let chain = ChainId {
                tenant_id: r.emitted.tenant_id.to_string(),
                system_id: r.emitted.system_id.to_string(),
            };
            let head = heads.entry(chain).or_insert((0, GENESIS));
            if r.seq >= head.0 {
                *head = (r.seq, r.event_hash);
            }
        }
        Ok(heads
            .into_iter()
            .map(|(chain, (seq, hash))| (chain, seq, hash))
            .collect())
    }
}

fn chain_id(system: &str) -> ChainId {
    ChainId {
        tenant_id: "acme".into(),
        system_id: system.into(),
    }
}

/// Events from the gateway, for one system.
fn traffic(system: &str, n: u64) -> Vec<EmittedEvent> {
    (1..=n)
        .map(|i| {
            let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
            e.system_id = std::sync::Arc::from(system);
            e.pins.system_id = std::sync::Arc::from(system);
            e
        })
        .collect()
}

/// Fast retries, so the test is not a sleep.
fn quick_retry() -> RetryPolicy {
    RetryPolicy {
        initial: std::time::Duration::from_micros(10),
        max: std::time::Duration::from_micros(50),
        attempts: 3,
    }
}

#[tokio::test]
async fn the_store_going_down_and_coming_back_leaves_a_chain_with_no_gap() {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1").with_retry(quick_retry());
    let chain = chain_id("hr-screening");

    // Before the outage.
    let before = traffic("hr-screening", 300);
    let r = ing.ingest(before).await.unwrap();
    assert_eq!(r.inserted, 300);
    assert_eq!(r.deferred, 0);

    // The store dies. The bus keeps delivering.
    store.kill();
    // Distinct ids per batch, the way real traffic would be.
    let during: Vec<Vec<EmittedEvent>> = (0..5u64)
        .map(|b| {
            (0..100u64)
                .map(|i| {
                    let mut e = ancre_types::fixtures::event(1_000 + b * 100 + i, GENESIS).emitted;
                    e.system_id = std::sync::Arc::from("hr-screening");
                    e.pins.system_id = std::sync::Arc::from("hr-screening");
                    e
                })
                .collect()
        })
        .collect();

    let mut deferred_batches = Vec::new();
    for batch in during {
        let r = ing.ingest(batch.clone()).await.unwrap();
        assert_eq!(
            r.inserted, 0,
            "nothing can be stored while the store is down"
        );
        assert_eq!(
            r.deferred, 1,
            "the batch must be handed back for redelivery"
        );
        deferred_batches.push(batch);
    }

    assert_eq!(
        store.chain_rows(&chain).len(),
        300,
        "the store must hold exactly what it held before the outage"
    );

    // The store comes back. The bus redelivers everything unacked.
    store.revive();
    for batch in deferred_batches {
        let r = ing.ingest(batch).await.unwrap();
        assert_eq!(r.inserted, 100);
        assert_eq!(r.deferred, 0);
    }

    // After the outage.
    let after: Vec<EmittedEvent> = (2_000..2_100)
        .map(|i| {
            let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
            e.system_id = std::sync::Arc::from("hr-screening");
            e.pins.system_id = std::sync::Arc::from("hr-screening");
            e
        })
        .collect();
    ing.ingest(after).await.unwrap();

    // The whole point.
    let rows = store.chain_rows(&chain);
    assert_eq!(rows.len(), 900, "300 + 500 replayed + 100 after");

    let report = verify_range(rows, GENESIS);
    assert!(
        report.is_clean(),
        "the chain must verify with no gap after the outage: {:?}",
        report.violations
    );
    assert_eq!(report.seq_from, 1);
    assert_eq!(report.seq_to, 900);
}

/// The failure that does not announce itself: chaining a batch, failing to
/// store it, then resuming from a head the store has never seen.
#[tokio::test]
async fn a_failed_insert_rewinds_the_chain_rather_than_advancing_past_it() {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1").with_retry(quick_retry());

    ing.ingest(traffic("hr-screening", 10)).await.unwrap();

    store.kill();
    let doomed: Vec<EmittedEvent> = (100..110)
        .map(|i| {
            let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
            e.system_id = std::sync::Arc::from("hr-screening");
            e.pins.system_id = std::sync::Arc::from("hr-screening");
            e
        })
        .collect();
    let r = ing.ingest(doomed.clone()).await.unwrap();
    assert_eq!(r.deferred, 1);

    store.revive();

    // The redelivery must land at seq 11, not seq 21. If the writer had
    // advanced past the failed batch, these events would be chained at 21..30
    // and seq 11..20 would never exist — a permanent gap.
    let r = ing.ingest(doomed).await.unwrap();
    assert_eq!(r.inserted, 10);

    let rows = store.chain_rows(&chain_id("hr-screening"));
    assert_eq!(rows.last().unwrap().seq, 20);
    assert!(verify_range(rows, GENESIS).is_clean());
}

/// JetStream is at-least-once, so the same batch arriving twice is normal.
#[tokio::test]
async fn redelivery_after_a_successful_insert_is_a_no_op() {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1");

    let batch = traffic("hr-screening", 50);
    assert_eq!(ing.ingest(batch.clone()).await.unwrap().inserted, 50);

    let again = ing.ingest(batch).await.unwrap();
    assert_eq!(again.duplicates, 50);
    assert_eq!(again.inserted, 0);
    assert_eq!(again.chained, 0);

    let rows = store.chain_rows(&chain_id("hr-screening"));
    assert_eq!(rows.len(), 50, "redelivery must not double the record");
    assert!(verify_range(rows, GENESIS).is_clean());
}

#[tokio::test]
async fn a_restart_resumes_from_the_store_without_forking_the_chain() {
    let store = FlakyStore::default();

    {
        let mut first = Ingester::new(&store, "ing-1");
        first.ingest(traffic("hr-screening", 120)).await.unwrap();
    }

    // A new process, no in-memory state at all.
    let mut second = Ingester::new(&store, "ing-2");
    let more: Vec<EmittedEvent> = (500..600)
        .map(|i| {
            let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
            e.system_id = std::sync::Arc::from("hr-screening");
            e.pins.system_id = std::sync::Arc::from("hr-screening");
            e
        })
        .collect();
    second.ingest(more).await.unwrap();

    let rows = store.chain_rows(&chain_id("hr-screening"));
    assert_eq!(rows.len(), 220);
    assert!(
        verify_range(rows, GENESIS).is_clean(),
        "a restart must not fork the chain"
    );
}

/// Chains are per `(tenant, system)`, so two systems interleaved on the bus
/// must produce two independent, gapless chains.
#[tokio::test]
async fn interleaved_systems_produce_independent_chains() {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1");

    let mut mixed = Vec::new();
    for i in 1..=200u64 {
        let system = if i % 2 == 0 {
            "hr-screening"
        } else {
            "credit-scoring"
        };
        let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
        e.system_id = std::sync::Arc::from(system);
        e.pins.system_id = std::sync::Arc::from(system);
        mixed.push(e);
    }
    ing.ingest(mixed).await.unwrap();
    assert_eq!(ing.chain_count(), 2);

    for system in ["hr-screening", "credit-scoring"] {
        let rows = store.chain_rows(&chain_id(system));
        assert_eq!(rows.len(), 100, "{system}");
        assert_eq!(rows[0].seq, 1, "{system} chains start at 1 independently");
        let report = verify_range(rows, GENESIS);
        assert!(report.is_clean(), "{system}: {:?}", report.violations);
    }
}

/// A system with no traffic must still leave something an auditor can read.
#[tokio::test]
async fn daily_heartbeats_keep_a_silent_chain_countable() {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1");

    ing.ingest(traffic("hr-screening", 5)).await.unwrap();

    let mut day = 1_754_400_000_000_000i64;
    for _ in 0..7 {
        ing.heartbeats(ancre_types::Timestamp::from_micros(day))
            .await
            .unwrap();
        // Same day again: no second heartbeat.
        assert_eq!(
            ing.heartbeats(ancre_types::Timestamp::from_micros(day + 1_000))
                .await
                .unwrap(),
            0
        );
        day += 86_400_000_000;
    }

    let rows = store.chain_rows(&chain_id("hr-screening"));
    assert_eq!(rows.len(), 12, "5 requests + 7 daily heartbeats");
    assert!(verify_range(rows, GENESIS).is_clean());
}

/// D10, and the shape of the bug matters more than the count.
///
/// A restart empties the in-memory dedupe window while the bus still holds
/// unacked messages, so *every* redelivery after a restart is one the window
/// cannot recognise. Chained a second time, they get fresh seqs, and the chain
/// verifies perfectly — it just claims twice the traffic. The checkpointer
/// then compares the leaf count against the range and refuses to sign it, so
/// the chain stops being attested and no later repair brings it back.
#[tokio::test]
async fn a_redelivery_the_window_cannot_remember_is_caught_by_the_store() {
    let store = FlakyStore::default();
    let chain = chain_id("hr-screening");
    let sent = traffic("hr-screening", 300);

    let mut first = Ingester::new(&store, "ing-1");
    let r = first.ingest(sent.clone()).await.unwrap();
    assert_eq!(r.inserted, 300);
    assert_eq!(
        store.lookups.load(Ordering::SeqCst),
        0,
        "a chain that started empty under an untrimmed window knows the answer \
         already and must not pay a round trip to be told it",
    );

    // The process dies and comes back. The window is gone; the store is not.
    drop(first);
    let mut restarted = Ingester::new(&store, "ing-2");

    let r = restarted.ingest(sent.clone()).await.unwrap();
    assert_eq!(r.duplicates, 300, "every redelivery must be recognised");
    assert_eq!(r.chained, 0);
    assert_eq!(r.inserted, 0);
    assert!(
        store.lookups.load(Ordering::SeqCst) > 0,
        "the answer can only have come from the store",
    );

    let rows = store.chain_rows(&chain);
    assert_eq!(rows.len(), 300, "the record must not describe 600 requests");
    assert_eq!(rows.last().unwrap().seq, 300);
    assert!(verify_range(rows, GENESIS).is_clean());
}

/// The same restart, then genuinely new traffic. Recognising redeliveries is
/// worth nothing if it also refuses the events that follow them.
#[tokio::test]
async fn a_restart_continues_the_chain_after_absorbing_its_redeliveries() {
    let store = FlakyStore::default();
    let chain = chain_id("hr-screening");

    let mut first = Ingester::new(&store, "ing-1");
    first.ingest(traffic("hr-screening", 100)).await.unwrap();
    drop(first);

    let mut restarted = Ingester::new(&store, "ing-2");
    // A redelivered batch with new events mixed in, which is what a partially
    // acked JetStream batch actually looks like.
    // `traffic` is deterministic in the event id, so the first 100 of a 150-run
    // are byte for byte the batch that was already stored.
    let mixed = traffic("hr-screening", 150);
    let r = restarted.ingest(mixed).await.unwrap();

    assert_eq!(r.duplicates, 100);
    assert_eq!(r.inserted, 50);

    let rows = store.chain_rows(&chain);
    assert_eq!(rows.len(), 150);
    assert_eq!(rows.last().unwrap().seq, 150);
    assert!(verify_range(rows, GENESIS).is_clean());
}

/// A store that cannot answer "have you seen these" must not be answered with
/// "probably not". Guessing costs a double-chained event that nothing can
/// undo; deferring costs a redelivery.
#[tokio::test]
async fn a_store_that_cannot_answer_the_dedupe_check_defers_the_batch() {
    let store = FlakyStore::default();
    let mut first = Ingester::new(&store, "ing-1");
    first.ingest(traffic("hr-screening", 50)).await.unwrap();
    drop(first);

    let mut restarted = Ingester::new(&store, "ing-2");
    restarted.seed_from_store().await.unwrap();

    store.kill();
    let r = restarted.ingest(traffic("hr-screening", 50)).await.unwrap();

    assert_eq!(r.deferred, 1);
    assert_eq!(r.chained, 0, "nothing may be chained on a guess");
    assert_eq!(store.chain_rows(&chain_id("hr-screening")).len(), 50);

    // And the deferral leaves nothing behind: the redelivery after the store
    // returns is still recognised as one.
    store.revive();
    let r = restarted.ingest(traffic("hr-screening", 50)).await.unwrap();
    assert_eq!(r.duplicates, 50);
    assert_eq!(r.inserted, 0);
}

/// D15. The heartbeat exists so that a silent system and a decommissioned one
/// are different shapes in the record. Before seeding, a restart forgot every
/// chain that was not currently sending traffic — so the systems whose silence
/// the heartbeat was built to make countable were exactly the ones it stopped
/// covering.
#[tokio::test]
async fn a_restart_keeps_heartbeating_the_chains_that_have_gone_quiet() {
    let store = FlakyStore::default();

    let mut first = Ingester::new(&store, "ing-1");
    for system in ["hr-screening", "credit-scoring"] {
        first.ingest(traffic(system, 10)).await.unwrap();
    }
    drop(first);

    // A new process. Neither chain sends anything ever again.
    let mut restarted = Ingester::new(&store, "ing-2");
    assert_eq!(restarted.chain_count(), 0, "a fresh process knows nothing");

    let seeded = restarted.seed_from_store().await.unwrap();
    assert_eq!(seeded, 2);
    assert_eq!(restarted.chain_count(), 2);

    let mut day = 1_754_400_000_000_000i64 + 86_400_000_000;
    for _ in 0..5 {
        let written = restarted
            .heartbeats(ancre_types::Timestamp::from_micros(day))
            .await
            .unwrap();
        assert_eq!(written, 2, "both silent chains must stay countable");
        day += 86_400_000_000;
    }

    for system in ["hr-screening", "credit-scoring"] {
        let rows = store.chain_rows(&chain_id(system));
        assert_eq!(rows.len(), 15, "{system}: 10 requests + 5 daily heartbeats");
        assert!(verify_range(rows, GENESIS).is_clean(), "{system}");
    }
}

/// Seeding and deduplication have to hold together: a process that restarts
/// after today's heartbeat is written comes back with a writer that has no
/// memory of it, and the id is deterministic per `(chain, day)` for exactly
/// this reason. Without the durable check, every restart would add another
/// heartbeat for the same day at a new seq.
#[tokio::test]
async fn a_restart_mid_day_does_not_write_a_second_heartbeat() {
    let store = FlakyStore::default();
    let day = ancre_types::Timestamp::from_micros(1_754_400_000_000_000);

    let mut first = Ingester::new(&store, "ing-1");
    first.ingest(traffic("hr-screening", 5)).await.unwrap();
    assert_eq!(first.heartbeats(day).await.unwrap(), 1);
    drop(first);

    // Three restarts inside the same day.
    for _ in 0..3 {
        let mut restarted = Ingester::new(&store, "ing-2");
        restarted.seed_from_store().await.unwrap();
        assert_eq!(
            restarted
                .heartbeats(ancre_types::Timestamp::from_micros(
                    day.as_micros() + 3_600_000_000
                ))
                .await
                .unwrap(),
            0,
            "today's heartbeat is already in the store",
        );
    }

    let rows = store.chain_rows(&chain_id("hr-screening"));
    assert_eq!(rows.len(), 6, "5 requests + exactly one heartbeat");
    assert_eq!(rows.last().unwrap().seq, 6);
    assert!(verify_range(rows, GENESIS).is_clean());

    // The next day still gets one.
    let mut restarted = Ingester::new(&store, "ing-2");
    restarted.seed_from_store().await.unwrap();
    let next = ancre_types::Timestamp::from_micros(day.as_micros() + 86_400_000_000);
    assert_eq!(restarted.heartbeats(next).await.unwrap(), 1);
}

/// Signing the range an outage was survived through is what makes the recovery
/// attestable rather than merely self-consistent.
#[tokio::test]
async fn the_recovered_range_can_be_checkpointed_and_a_single_event_proven() {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1").with_retry(quick_retry());

    store.kill();
    let batch = traffic("hr-screening", 200);
    ing.ingest(batch.clone()).await.unwrap();
    store.revive();
    ing.ingest(batch).await.unwrap();

    let rows = store.chain_rows(&chain_id("hr-screening"));
    let leaves: Vec<Hash32> = rows.iter().map(|r| r.event_hash).collect();

    let signer = ancre_chain::CheckpointSigner::from_bytes([5u8; 32], "cp-1".into());
    let cp = signer
        .seal_range("acme", "hr-screening", 1, &leaves)
        .unwrap();

    let proof = ancre_chain::prove_inclusion(&cp, &leaves, 137).unwrap();
    assert!(ancre_chain::verify_inclusion(&cp, &proof, &signer.verifying_key()).is_ok());
}

/// A store that never recovers must not look like a store that did.
#[tokio::test]
async fn a_permanently_dead_store_never_reports_events_as_inserted() {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1").with_retry(quick_retry());
    store.kill();

    let mut totals = HashMap::new();
    for _ in 0..10 {
        let r = ing.ingest(traffic("hr-screening", 20)).await.unwrap();
        *totals.entry("inserted").or_insert(0) += r.inserted;
        *totals.entry("deferred").or_insert(0) += r.deferred;
    }

    assert_eq!(totals["inserted"], 0);
    assert_eq!(totals["deferred"], 10);
    assert!(store.rows().is_empty());
    assert!(
        store.contacts.load(Ordering::SeqCst) > 0,
        "the ingester must keep trying rather than quietly giving up"
    );
    // Note it never even reaches `insert`: with no writer for the chain yet,
    // `head` fails first, so there is nothing to chain. Correct — guessing a
    // starting seq would fork the chain the moment the store came back.
    assert_eq!(store.inserts_attempted.load(Ordering::SeqCst), 0);
}
