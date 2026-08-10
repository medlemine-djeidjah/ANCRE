//! Gateway → NATS JetStream → ingester, against a real NATS.
//!
//! The unit tests either side prove each half in isolation. What only a real
//! server shows is the part that matters most: that a batch the store refuses
//! comes **back**, intact, and that the chain it eventually forms has no gap.
//!
//! Skipped unless `ANCRE_TEST_NATS` is set:
//!
//! ```sh
//! docker run -d --name ancre-nats -p 14222:4222 nats:2.10-alpine -js -sd /data
//! ANCRE_TEST_NATS=nats://127.0.0.1:14222 cargo test -p ancre-ingester --test bus
//! ```

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ancre_canon::{GENESIS, Hash32};
use ancre_chain::{ChainId, verify_range};
use ancre_gateway::bus::NatsSink;
use ancre_gateway::telemetry::EventSink;
use ancre_ingester::bus::{ConsumerConfig, NatsConsumer};
use ancre_ingester::chain_writer::IngestError;
use ancre_ingester::pipeline::Ingester;
use ancre_ingester::sink::{EventStore, RetryPolicy};
use ancre_types::{AuditEvent, EmittedEvent};

fn url() -> Option<String> {
    std::env::var("ANCRE_TEST_NATS").ok()
}

macro_rules! url_or_skip {
    () => {
        match url() {
            Some(u) => u,
            None => {
                eprintln!("skipped: ANCRE_TEST_NATS is not set");
                return;
            }
        }
    };
}

/// A store that can be knocked over, so the nak path is exercised against the
/// real broker rather than a mock of it.
#[derive(Debug, Default)]
struct FlakyStore {
    rows: Mutex<Vec<AuditEvent>>,
    down: AtomicBool,
}

impl FlakyStore {
    fn kill(&self) {
        self.down.store(true, Ordering::SeqCst);
    }
    fn revive(&self) {
        self.down.store(false, Ordering::SeqCst);
    }
    fn rows(&self, chain: &ChainId) -> Vec<AuditEvent> {
        let mut rows: Vec<_> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| *r.emitted.system_id == *chain.system_id)
            .cloned()
            .collect();
        rows.sort_by_key(|r| r.seq);
        rows
    }
}

impl EventStore for &FlakyStore {
    async fn insert(&self, batch: &[AuditEvent]) -> Result<(), IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        self.rows.lock().unwrap().extend_from_slice(batch);
        Ok(())
    }

    async fn head(&self, chain: &ChainId) -> Result<Option<(u64, Hash32)>, IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        Ok(self.rows(chain).last().map(|r| (r.seq, r.event_hash)))
    }

    async fn stored(
        &self,
        chain: &ChainId,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, u64)>, IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        Ok(self
            .rows(chain)
            .into_iter()
            .filter(|r| ids.contains(&r.emitted.event_id))
            .map(|r| (r.emitted.event_id, r.seq))
            .collect())
    }

    async fn chains(&self) -> Result<Vec<(ChainId, u64, Hash32)>, IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        let mut heads: std::collections::HashMap<ChainId, (u64, Hash32)> =
            std::collections::HashMap::new();
        for r in self.rows.lock().unwrap().iter() {
            let chain = ChainId {
                tenant_id: r.emitted.tenant_id.to_string(),
                system_id: r.emitted.system_id.to_string(),
            };
            let head = heads.entry(chain).or_insert((0, ancre_canon::GENESIS));
            if r.seq >= head.0 {
                *head = (r.seq, r.event_hash);
            }
        }
        Ok(heads
            .into_iter()
            .map(|(c, (seq, hash))| (c, seq, hash))
            .collect())
    }
}

/// A distinct system per test. Combined with `config_for`, each test gets its
/// own chain, its own subject and its own durable consumer.
fn chain(name: &str) -> ChainId {
    ChainId {
        tenant_id: "acme".into(),
        system_id: format!("{name}-{}", std::process::id()),
    }
}

fn traffic(chain: &ChainId, range: std::ops::RangeInclusive<u64>) -> Vec<EmittedEvent> {
    range
        .map(|i| {
            let mut e = ancre_types::fixtures::event(i, GENESIS).emitted;
            e.system_id = std::sync::Arc::from(chain.system_id.as_str());
            e.pins.system_id = std::sync::Arc::from(chain.system_id.as_str());
            e.event_id = uuid::Uuid::new_v4();
            e
        })
        .collect()
}

/// Its own durable consumer, filtered to its own chain, so tests sharing one
/// broker cannot consume each other's messages. This is the same mechanism a
/// second ingester would use to take a slice of the subject space.
fn config_for(chain: &ChainId) -> ConsumerConfig {
    ConsumerConfig {
        durable_name: format!(
            "test-{}",
            chain
                .system_id
                .replace(|c: char| !c.is_ascii_alphanumeric(), "-")
        ),
        filter_subject: ancre_gateway::bus::subject_for(&chain.tenant_id, &chain.system_id),
        ..ConsumerConfig::default()
    }
}

fn quick_retry() -> RetryPolicy {
    RetryPolicy {
        initial: std::time::Duration::from_micros(10),
        max: std::time::Duration::from_micros(50),
        attempts: 2,
    }
}

/// Drain until the store holds `want` rows or we run out of patience.
async fn drain_until<S: EventStore>(
    consumer: &mut NatsConsumer<S>,
    done: impl Fn() -> bool,
    pulls: usize,
) {
    for _ in 0..pulls {
        consumer.pull_once().await.unwrap();
        if done() {
            return;
        }
    }
}

#[tokio::test]
async fn events_published_by_the_gateway_arrive_and_chain() {
    let url = url_or_skip!();
    let chain = chain("happy");

    let sink = NatsSink::connect(&url).await.unwrap();
    sink.publish(traffic(&chain, 1..=200)).await.unwrap();

    let store = FlakyStore::default();
    let mut consumer = NatsConsumer::from_context(
        async_nats::jetstream::new(async_nats::connect(&url).await.unwrap()),
        Ingester::new(&store, "ing-1").with_retry(quick_retry()),
        config_for(&chain),
    )
    .await
    .unwrap();

    drain_until(&mut consumer, || store.rows(&chain).len() >= 200, 20).await;

    let rows = store.rows(&chain);
    assert_eq!(rows.len(), 200);
    assert_eq!(rows[0].seq, 1);
    let report = verify_range(rows, GENESIS);
    assert!(report.is_clean(), "{:?}", report.violations);
    assert_eq!(report.seq_to, 200);
}

/// The property the whole ack contract exists for. The store refuses a batch;
/// nothing may be acked, and everything must come back.
#[tokio::test]
async fn a_batch_the_store_refuses_is_redelivered_and_leaves_no_gap() {
    let url = url_or_skip!();
    let chain = chain("nak");

    let sink = NatsSink::connect(&url).await.unwrap();
    sink.publish(traffic(&chain, 1..=100)).await.unwrap();

    let store = FlakyStore::default();
    let mut consumer = NatsConsumer::from_context(
        async_nats::jetstream::new(async_nats::connect(&url).await.unwrap()),
        Ingester::new(&store, "ing-1").with_retry(quick_retry()),
        ConsumerConfig {
            redelivery_delay: std::time::Duration::from_millis(50),
            ..config_for(&chain)
        },
    )
    .await
    .unwrap();

    // The store is down: pull, fail, nak.
    store.kill();
    let report = consumer.pull_once().await.unwrap();
    assert!(report.messages > 0, "messages must have been pulled");
    assert_eq!(report.inserted, 0);
    assert_eq!(report.redelivered, 1, "the batch must be handed back");
    assert!(store.rows(&chain).is_empty());

    // The store comes back. JetStream redelivers what was nak'd.
    store.revive();
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    drain_until(&mut consumer, || store.rows(&chain).len() >= 100, 30).await;

    let rows = store.rows(&chain);
    assert_eq!(rows.len(), 100, "every nak'd event must come back");
    assert_eq!(rows[0].seq, 1, "the chain must still start at 1");
    assert!(
        verify_range(rows, GENESIS).is_clean(),
        "a redelivered batch must not leave a gap"
    );
}

/// `Nats-Msg-Id` dedupes at the broker, so a publish retried after a timeout
/// does not put the same event on the stream twice.
#[tokio::test]
async fn republishing_the_same_events_does_not_duplicate_them_on_the_stream() {
    let url = url_or_skip!();
    let chain = chain("dedupe");

    let sink = NatsSink::connect(&url).await.unwrap();
    let batch = traffic(&chain, 1..=50);
    sink.publish(batch.clone()).await.unwrap();
    sink.publish(batch).await.unwrap();

    let store = FlakyStore::default();
    let mut consumer = NatsConsumer::from_context(
        async_nats::jetstream::new(async_nats::connect(&url).await.unwrap()),
        Ingester::new(&store, "ing-1").with_retry(quick_retry()),
        config_for(&chain),
    )
    .await
    .unwrap();

    drain_until(&mut consumer, || store.rows(&chain).len() >= 50, 20).await;
    // Two more pulls: if the broker had accepted the republish, they would
    // arrive here and the ingester's own dedupe would have to catch them.
    consumer.pull_once().await.unwrap();
    consumer.pull_once().await.unwrap();

    let rows = store.rows(&chain);
    assert_eq!(rows.len(), 50, "the record must not double");
    assert!(verify_range(rows, GENESIS).is_clean());
}

/// A message that will never decode must not block the consumer behind it.
/// Every chain would stop, which is a far larger gap than the one bad event.
#[tokio::test]
async fn an_undecodable_message_is_counted_and_skipped_rather_than_blocking() {
    let url = url_or_skip!();
    let chain = chain("poison");

    let client = async_nats::connect(&url).await.unwrap();
    let context = async_nats::jetstream::new(client);
    ancre_gateway::bus::ensure_stream(&context).await.unwrap();

    context
        .publish(
            ancre_gateway::bus::subject_for(&chain.tenant_id, &chain.system_id),
            "{not valid json".into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    let sink = NatsSink::from_context(context.clone());
    sink.publish(traffic(&chain, 1..=20)).await.unwrap();

    let store = FlakyStore::default();
    let mut consumer = NatsConsumer::from_context(
        context,
        Ingester::new(&store, "ing-1").with_retry(quick_retry()),
        config_for(&chain),
    )
    .await
    .unwrap();

    let mut undecodable = 0;
    for _ in 0..20 {
        undecodable += consumer.pull_once().await.unwrap().undecodable;
        if store.rows(&chain).len() >= 20 {
            break;
        }
    }

    assert!(undecodable >= 1, "the poison message must be counted");
    assert_eq!(
        store.rows(&chain).len(),
        20,
        "the good events behind it must still land"
    );
}

/// A restart must resume where the durable consumer left off, not replay the
/// stream from the beginning.
#[tokio::test]
async fn a_restarted_consumer_resumes_rather_than_replaying() {
    let url = url_or_skip!();
    let chain = chain("resume");

    let sink = NatsSink::connect(&url).await.unwrap();
    sink.publish(traffic(&chain, 1..=80)).await.unwrap();

    let store = FlakyStore::default();
    {
        let mut first = NatsConsumer::from_context(
            async_nats::jetstream::new(async_nats::connect(&url).await.unwrap()),
            Ingester::new(&store, "ing-1").with_retry(quick_retry()),
            config_for(&chain),
        )
        .await
        .unwrap();
        drain_until(&mut first, || store.rows(&chain).len() >= 80, 20).await;
    }
    assert_eq!(store.rows(&chain).len(), 80);

    // A new process against the same durable consumer.
    let mut second = NatsConsumer::from_context(
        async_nats::jetstream::new(async_nats::connect(&url).await.unwrap()),
        Ingester::new(&store, "ing-2").with_retry(quick_retry()),
        config_for(&chain),
    )
    .await
    .unwrap();
    second.pull_once().await.unwrap();
    second.pull_once().await.unwrap();

    assert_eq!(
        store.rows(&chain).len(),
        80,
        "a restart must not replay events the consumer already acked"
    );
    assert!(verify_range(store.rows(&chain), GENESIS).is_clean());
}
