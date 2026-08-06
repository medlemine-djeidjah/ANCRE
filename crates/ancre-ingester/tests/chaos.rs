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
