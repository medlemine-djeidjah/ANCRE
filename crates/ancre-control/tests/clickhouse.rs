//! The ClickHouse `ChainSource`, against a real ClickHouse.
//!
//! Chains are seeded through the **ingester**, not through hand-written
//! inserts. The property under test is that the control plane seals what the
//! ingester actually wrote, and a test that writes its own rows proves only
//! that this file agrees with itself.
//!
//! Skipped unless `ANCRE_TEST_CLICKHOUSE` is set. The tick test additionally
//! needs `ANCRE_TEST_POSTGRES`, because a checkpoint has nowhere to go without
//! it:
//!
//! ```sh
//! docker run -d --name ancre-ch -p 18123:8123 \
//!   -e CLICKHOUSE_USER=ancre -e CLICKHOUSE_PASSWORD=ancre -e CLICKHOUSE_DB=ancre \
//!   -v "$PWD/deploy/compose/init/clickhouse:/docker-entrypoint-initdb.d:ro" \
//!   clickhouse/clickhouse-server:24.8
//! ANCRE_TEST_CLICKHOUSE=http://127.0.0.1:18123 cargo test -p ancre-control --test clickhouse
//! ```

use ancre_canon::GENESIS;
use ancre_chain::{ChainId, CheckpointSigner, verify_checkpoint};
use ancre_control::checkpointer::{ChainSource, CheckpointStore, Checkpointer};
use ancre_control::clickhouse::ClickHouseChains;
use ancre_control::export::ChainExport;
use ancre_control::postgres::PgStore;
use ancre_ingester::clickhouse::ClickHouseStore;
use ancre_ingester::pipeline::Ingester;
use ancre_types::{EmittedEvent, Timestamp};

fn chains() -> Option<ClickHouseChains> {
    let url = std::env::var("ANCRE_TEST_CLICKHOUSE").ok()?;
    Some(ClickHouseChains::new(&url, "ancre").with_credentials("ancre", "ancre"))
}

fn writer() -> ClickHouseStore {
    let url = std::env::var("ANCRE_TEST_CLICKHOUSE").expect("checked by the caller");
    ClickHouseStore::new(&url, "ancre").with_credentials("ancre", "ancre")
}

macro_rules! chains_or_skip {
    () => {
        match chains() {
            Some(c) => c,
            None => {
                eprintln!("skipped: ANCRE_TEST_CLICKHOUSE is not set");
                return;
            }
        }
    };
}

/// A chain of its own per test, so they can share one server without seeing
/// each other's rows — `chains()` reads the whole table.
fn chain(name: &str) -> ChainId {
    ChainId {
        tenant_id: "acme".into(),
        system_id: format!("ctl-{name}-{}", std::process::id()),
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

async fn seed(chain: &ChainId, n: u64) {
    let store = writer();
    let mut ing = Ingester::new(&store, "ing-1");
    let report = ing.ingest(traffic(chain, n)).await.unwrap();
    assert_eq!(report.inserted, n);
}

#[tokio::test]
async fn an_empty_chain_has_no_head_rather_than_seq_zero() {
    let source = chains_or_skip!();
    assert!(
        source.head_seq(&chain("empty")).await.unwrap().is_none(),
        "seq is 1-based: 0 must read as 'nothing here', not 'one event'"
    );
}

#[tokio::test]
async fn a_written_chain_is_discoverable_with_its_head() {
    let source = chains_or_skip!();
    let chain = chain("discover");
    seed(&chain, 40).await;

    assert_eq!(source.head_seq(&chain).await.unwrap(), Some(40));

    let listed = source.chains().await.unwrap();
    assert!(listed.contains(&chain), "the chain must be discoverable");

    // Ordered, so two control-plane replicas walk them in the same sequence.
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted);
}

/// The `ORDER BY` is the whole contract: the tree root is order-dependent, so
/// leaves read in the wrong order produce a signature over a root no verifier
/// will ever reproduce.
#[tokio::test]
async fn leaves_come_back_in_seq_order_and_match_the_stored_events() {
    let source = chains_or_skip!();
    let chain = chain("leaves");
    seed(&chain, 100).await;

    let leaves = source.leaves(&chain, 1, 100).await.unwrap();
    assert_eq!(leaves.len(), 100);

    let events = writer().range(&chain, 1, 100).await.unwrap();
    let expected: Vec<_> = events.iter().map(|e| e.event_hash).collect();
    assert_eq!(leaves, expected);
}

#[tokio::test]
async fn a_sub_range_is_inclusive_at_both_ends() {
    let source = chains_or_skip!();
    let chain = chain("subrange");
    seed(&chain, 50).await;

    let leaves = source.leaves(&chain, 10, 20).await.unwrap();
    assert_eq!(leaves.len(), 11, "BETWEEN is inclusive at both ends");

    let all = source.leaves(&chain, 1, 50).await.unwrap();
    assert_eq!(leaves, all[9..20]);
}

/// End to end, across both datastores: the ingester writes a chain into
/// ClickHouse, the control plane seals it, the signature is stored in Postgres,
/// and what comes back verifies offline against the public key alone.
#[tokio::test]
async fn a_chain_written_by_the_ingester_is_sealed_and_verifies_offline() {
    let source = chains_or_skip!();
    let Ok(url) = std::env::var("ANCRE_TEST_POSTGRES") else {
        eprintln!("skipped: ANCRE_TEST_POSTGRES is not set");
        return;
    };
    let store = PgStore::connect(&url).await.unwrap();

    let chain = chain("sealed");
    seed(&chain, 250).await;

    let signer = CheckpointSigner::from_bytes([5u8; 32], "cp-seal".into());
    let verifying = signer.verifying_key();
    let checkpointer = Checkpointer::new(source, store.clone(), signer);

    let report = checkpointer.tick(Timestamp::now()).await.unwrap();
    assert!(report.checkpoints_written >= 1);

    let sealed = store.list(&chain).await.unwrap();
    assert_eq!(sealed.len(), 1, "one range, sealed once");
    assert_eq!(sealed[0].body.seq_from, 1);
    assert_eq!(sealed[0].body.seq_to, 250);
    assert!(
        verify_checkpoint(&sealed[0], &verifying).is_ok(),
        "an auditor has the public key and nothing else"
    );

    // The root must be the tree over what ClickHouse holds, not over anything
    // this test assembled — recomputed from a second read.
    let leaves = checkpointer
        .into_parts()
        .0
        .leaves(&chain, 1, 250)
        .await
        .unwrap();
    assert_eq!(sealed[0].body.root_hash, ancre_canon::tree_root(&leaves));

    // Ticking again seals nothing: the chain has not moved.
    let store2 = PgStore::connect(&url).await.unwrap();
    let again = Checkpointer::new(
        chains().unwrap(),
        store2,
        CheckpointSigner::from_bytes([5u8; 32], "cp-seal".into()),
    )
    .tick(Timestamp::now())
    .await
    .unwrap();
    assert_eq!(
        store.list(&chain).await.unwrap().len(),
        1,
        "a chain with nothing new must not be resealed (report: {again:?})"
    );
}

/// The product's last mile, against both real datastores: the ingester writes a
/// chain into ClickHouse, the export endpoint streams it as JSONL, and what
/// comes out the other end verifies as a chain.
///
/// Deliberately larger than one page, because the paging is where an export
/// silently loses or repeats events — and either produces a body that parses
/// perfectly and attests the wrong thing.
#[tokio::test]
async fn an_exported_chain_streams_as_jsonl_and_verifies() {
    let source = chains_or_skip!();
    let chain = chain("export");
    let n = 2_500;
    seed(&chain, n).await;

    let events = source.events(&chain, 1, n).await.unwrap();
    assert_eq!(events.len() as u64, n);

    // Through the serialisation the endpoint uses, then through the parse the
    // verifier uses. Both halves, or this proves only that ClickHouse round
    // trips.
    let jsonl = ancre_control::export::to_jsonl(&events).unwrap();
    let parsed: Vec<ancre_types::AuditEvent> = String::from_utf8(jsonl)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("every line must parse"))
        .collect();

    assert_eq!(parsed.len() as u64, n);
    for (i, event) in parsed.iter().enumerate() {
        assert_eq!(event.seq, i as u64 + 1, "seq order is load-bearing");
    }

    let report = ancre_chain::verify_range(parsed, GENESIS);
    assert!(
        report.is_clean(),
        "the exported chain must verify: {:?}",
        report.violations
    );
    assert_eq!(report.seq_to, n);
}

/// Paging must not drop or duplicate an event at a page boundary. Read in
/// pages the way the endpoint does and compare against one flat read.
#[tokio::test]
async fn paging_reassembles_exactly_the_flat_read() {
    let source = chains_or_skip!();
    let chain = chain("paging");
    let n = 1_000;
    seed(&chain, n).await;

    let flat = source.events(&chain, 1, n).await.unwrap();

    let page = 137; // deliberately not a divisor of n
    let mut paged = Vec::new();
    let mut next = 1;
    while next <= n {
        let last = (next + page - 1).min(n);
        paged.extend(source.events(&chain, next, last).await.unwrap());
        next = last + 1;
    }

    assert_eq!(paged.len(), flat.len());
    for (a, b) in paged.iter().zip(&flat) {
        assert_eq!(a.seq, b.seq);
        assert_eq!(a.event_hash, b.event_hash);
    }
}
