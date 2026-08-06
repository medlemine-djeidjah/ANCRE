//! The ClickHouse store, against a real ClickHouse.
//!
//! The unit tests in `src/clickhouse.rs` prove the flattening is
//! self-consistent. They cannot prove that ClickHouse *accepts* it — that
//! `Enum8` takes a string, that `FixedString(32)` takes a byte array, that
//! `DateTime64(6)` takes raw microseconds without rounding them. Those are the
//! failures that only appear against the real thing, and D9 in
//! `docs/deferred.md` is exactly this gap for the chaos gate.
//!
//! Skipped unless `ANCRE_TEST_CLICKHOUSE` is set, so `cargo test` on a machine
//! with no Docker stays green:
//!
//! ```sh
//! docker run -d --name ancre-ch -p 18123:8123 \
//!   -e CLICKHOUSE_USER=ancre -e CLICKHOUSE_PASSWORD=ancre -e CLICKHOUSE_DB=ancre \
//!   -v "$PWD/deploy/compose/init/clickhouse:/docker-entrypoint-initdb.d:ro" \
//!   clickhouse/clickhouse-server:24.8
//! ANCRE_TEST_CLICKHOUSE=http://127.0.0.1:18123 cargo test -p ancre-ingester --test clickhouse
//! ```

use ancre_canon::GENESIS;
use ancre_chain::{ChainId, verify_range};
use ancre_ingester::clickhouse::ClickHouseStore;
use ancre_ingester::pipeline::Ingester;
use ancre_ingester::sink::EventStore;
use ancre_types::{AuditEvent, EmittedEvent, RiskFlag};

/// `None` when the environment variable is unset, so the test suite is a no-op
/// rather than a failure on a machine without Docker.
fn store() -> Option<ClickHouseStore> {
    let url = std::env::var("ANCRE_TEST_CLICKHOUSE").ok()?;
    Some(ClickHouseStore::new(&url, "ancre").with_credentials("ancre", "ancre"))
}

macro_rules! store_or_skip {
    () => {
        match store() {
            Some(s) => s,
            None => {
                eprintln!("skipped: ANCRE_TEST_CLICKHOUSE is not set");
                return;
            }
        }
    };
}

/// Each test gets its own chain, so they can run concurrently against one
/// server without seeing each other's rows.
fn chain(name: &str) -> ChainId {
    ChainId {
        tenant_id: "acme".into(),
        system_id: format!("{name}-{}", std::process::id()),
    }
}

fn traffic(chain: &ChainId, n: u64) -> Vec<EmittedEvent> {
    (1..=n)
        .map(|i| {
            let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
            e.system_id = std::sync::Arc::from(chain.system_id.as_str());
            e.pins.system_id = std::sync::Arc::from(chain.system_id.as_str());
            e
        })
        .collect()
}

/// The whole point: what comes back out of ClickHouse must rehash to the hash
/// it was stored with. A type mapping that rounds a timestamp or reorders an
/// array breaks the chain for the auditor and nobody else.
#[tokio::test]
async fn a_stored_chain_reads_back_and_still_verifies() {
    let store = store_or_skip!();
    let chain = chain("roundtrip");

    let mut ing = Ingester::new(&store, "ing-1");
    let report = ing.ingest(traffic(&chain, 500)).await.unwrap();
    assert_eq!(report.inserted, 500);

    let rows = store.range(&chain, 1, 500).await.unwrap();
    assert_eq!(rows.len(), 500);

    for e in &rows {
        assert_eq!(
            ancre_chain::event_hash(e).unwrap(),
            e.event_hash,
            "seq {}: the row does not rehash to its own stored hash",
            e.seq
        );
    }

    let verdict = verify_range(rows, GENESIS);
    assert!(verdict.is_clean(), "{:?}", verdict.violations);
    assert_eq!(verdict.seq_to, 500);
}

/// `head` is what a restarting ingester resumes from. If it is wrong the chain
/// forks, and a fork verifies perfectly on each side of the split.
#[tokio::test]
async fn head_returns_the_highest_seq_with_its_own_hash() {
    let store = store_or_skip!();
    let chain = chain("head");

    assert!(
        store.head(&chain).await.unwrap().is_none(),
        "an empty chain must report no head, not seq 0"
    );

    let mut ing = Ingester::new(&store, "ing-1");
    ing.ingest(traffic(&chain, 120)).await.unwrap();

    let (seq, hash) = store.head(&chain).await.unwrap().expect("a head");
    assert_eq!(seq, 120);

    let last = store.get(&chain, 120).await.unwrap().expect("seq 120");
    assert_eq!(hash, last.event_hash, "the seq and hash must be one row");
}

/// A restart resumes from the store, not from memory. This is the sequence
/// that forks a chain if `head` and `resume_from` disagree.
#[tokio::test]
async fn a_restarted_ingester_continues_the_stored_chain() {
    let store = store_or_skip!();
    let chain = chain("restart");

    {
        let mut first = Ingester::new(&store, "ing-1");
        first.ingest(traffic(&chain, 60)).await.unwrap();
    }

    let mut second = Ingester::new(&store, "ing-2");
    let more: Vec<EmittedEvent> = (500..560)
        .map(|i| {
            let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
            e.system_id = std::sync::Arc::from(chain.system_id.as_str());
            e.pins.system_id = std::sync::Arc::from(chain.system_id.as_str());
            e
        })
        .collect();
    second.ingest(more).await.unwrap();

    let rows = store.range(&chain, 1, 120).await.unwrap();
    assert_eq!(rows.len(), 120);
    assert!(
        verify_range(rows, GENESIS).is_clean(),
        "a restart must not fork the chain"
    );
}

/// Every column type in the frozen schema, exercised at its edges: the enum,
/// the array, the bool, both fixed strings, and a microsecond timestamp that
/// must not be rounded to a second.
#[tokio::test]
async fn every_column_type_survives_the_server() {
    let store = store_or_skip!();
    let chain = chain("types");

    let mut emitted = traffic(&chain, 1);
    emitted[0].pins.risk_flags = smallvec::smallvec![
        RiskFlag::UnpinnedModel,
        RiskFlag::StaleConfig,
        RiskFlag::SubstantialCandidate,
    ];
    emitted[0].pins.resolved_stale = true;
    emitted[0].occurred_at = ancre_types::Timestamp::from_micros(1_754_400_000_123_456);
    emitted[0].metrics.error_code = "".into();

    let mut ing = Ingester::new(&store, "ing-1");
    ing.ingest(emitted).await.unwrap();

    let back = store.get(&chain, 1).await.unwrap().expect("seq 1");

    assert_eq!(
        back.emitted.occurred_at.as_micros(),
        1_754_400_000_123_456,
        "DateTime64(6) must keep microseconds"
    );
    assert!(back.emitted.pins.resolved_stale);
    assert_eq!(back.emitted.pins.risk_flags.len(), 3);
    assert!(
        back.emitted
            .pins
            .risk_flags
            .contains(&RiskFlag::SubstantialCandidate)
    );
    assert_eq!(back.emitted.pins.risk_class, ancre_types::RiskClass::High);
    assert_eq!(&*back.emitted.metrics.error_code, "");
    assert_eq!(
        ancre_chain::event_hash(&back).unwrap(),
        back.event_hash,
        "the hash must survive the server, not just the mapping"
    );
}

/// JetStream is at-least-once, so the same batch arriving twice is normal.
/// Through a real store this is the check that the record does not double —
/// the failure that does not error and still verifies.
#[tokio::test]
async fn redelivery_through_the_real_store_does_not_double_the_record() {
    let store = store_or_skip!();
    let chain = chain("redelivery");

    let batch = traffic(&chain, 40);
    let mut ing = Ingester::new(&store, "ing-1");
    assert_eq!(ing.ingest(batch.clone()).await.unwrap().inserted, 40);

    let again = ing.ingest(batch).await.unwrap();
    assert_eq!(again.duplicates, 40);
    assert_eq!(again.inserted, 0);

    let rows = store.range(&chain, 1, 1_000).await.unwrap();
    assert_eq!(rows.len(), 40, "redelivery must not double the record");
    assert!(verify_range(rows, GENESIS).is_clean());
}

/// An empty batch must not open a transaction or disturb the table.
#[tokio::test]
async fn an_empty_insert_is_a_no_op() {
    let store = store_or_skip!();
    let chain = chain("empty");

    let mut ing = Ingester::new(&store, "ing-1");
    ing.ingest(traffic(&chain, 10)).await.unwrap();

    let none: Vec<AuditEvent> = Vec::new();
    store.insert(&none).await.unwrap();

    let rows = store.range(&chain, 1, 1_000).await.unwrap();
    assert_eq!(rows.len(), 10);
}
