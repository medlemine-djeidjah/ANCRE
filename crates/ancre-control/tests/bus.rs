//! Snapshot publication, against a real NATS.
//!
//! The unit tests prove the envelope survives JSON. What only a real broker
//! shows is the property the whole design rests on: that a gateway which
//! connects **after** the last publish still gets the current generation. A
//! stream that kept nothing, or kept everything, would each fail differently —
//! the first leaves a late node cold until the next edit, the second lets a
//! superseded generation be replayed into a live fleet.
//!
//! One test rather than five, deliberately. The subject and stream names are
//! constants — a gateway has to find the configuration without being told
//! where it is — so isolating the cases would mean publishing somewhere the
//! production code never publishes, and the transport is exactly what is under
//! test here. The sequence below is therefore a single walk over one stream.
//!
//! Skipped unless `ANCRE_TEST_NATS` is set:
//!
//! ```sh
//! docker run -d --name ancre-nats -p 14222:4222 nats:2.10-alpine -js -sd /data
//! ANCRE_TEST_NATS=nats://127.0.0.1:14222 cargo test -p ancre-control --test bus
//! ```

use ancre_control::bus::{NatsSnapshotBus, STREAM, decode};
use ancre_control::envelope::SnapshotBus;
use ancre_types::SnapshotEnvelope;
use async_nats::jetstream;
use futures_util::StreamExt as _;

fn envelope(generation: u64) -> SnapshotEnvelope {
    let mut spec = ancre_resolver::testing::spec(generation);
    spec.generation = generation;
    SnapshotEnvelope::seal_now(spec).unwrap()
}

/// Read the stream the way a gateway does: an ordered ephemeral consumer over
/// everything the stream currently holds.
async fn on_the_stream(context: &jetstream::Context) -> Vec<SnapshotEnvelope> {
    let stream = context.get_stream(STREAM).await.expect("get stream");
    let consumer = stream
        .create_consumer(jetstream::consumer::pull::Config {
            deliver_policy: jetstream::consumer::DeliverPolicy::All,
            ..Default::default()
        })
        .await
        .expect("consumer");

    let mut out = Vec::new();
    let mut batch = consumer
        .fetch()
        .max_messages(64)
        .expires(std::time::Duration::from_secs(2))
        .messages()
        .await
        .expect("fetch");
    while let Some(Ok(msg)) = batch.next().await {
        out.push(decode(&msg.payload).expect("decode"));
    }
    out
}

async fn message_count(context: &jetstream::Context) -> u64 {
    context
        .get_stream(STREAM)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .state
        .messages
}

#[tokio::test]
async fn publication_against_a_real_broker() {
    let Ok(url) = std::env::var("ANCRE_TEST_NATS") else {
        eprintln!("skipped: ANCRE_TEST_NATS is not set");
        return;
    };

    // Start from nothing, so the assertions below are about what this test
    // published and not about what a previous run left behind.
    let client = async_nats::connect(&url).await.expect("connect");
    let context = jetstream::new(client);
    let _ = context.delete_stream(STREAM).await;

    // Connecting creates the stream, and connecting again must not fail: the
    // control plane and every gateway call `get_or_create_stream` with the
    // same config, and nothing orders them in a compose file.
    let bus = NatsSnapshotBus::connect(&url).await.expect("first connect");
    NatsSnapshotBus::connect(&url)
        .await
        .expect("a second connect must be idempotent");

    // Published before anything is listening. This is the case a fleet scaled
    // up on a quiet afternoon runs into: without retention, those nodes stay
    // cold until someone edits the registry.
    let first = envelope(41);
    bus.publish(first.clone()).await.expect("publish");

    let seen = on_the_stream(&context).await;
    assert_eq!(seen.len(), 1, "a late consumer must find the generation");
    assert_eq!(seen[0].generation, 41);
    assert_eq!(seen[0].content_hash, first.content_hash);
    assert!(
        seen[0].verify().is_ok(),
        "what a gateway installs must verify without asking the sender"
    );

    // A history would let a superseded generation be replayed into a live
    // fleet, and the envelope's own hash cannot catch that — an old envelope
    // is internally consistent.
    for generation in 42..=46 {
        bus.publish(envelope(generation)).await.expect("publish");
    }
    let seen = on_the_stream(&context).await;
    assert_eq!(seen.len(), 1, "the stream must collapse to the latest");
    assert_eq!(seen[0].generation, 46);

    // A publish retried after a timeout must not land twice. The dedupe id
    // carries the generation and its content hash, so this is the same
    // message as far as the broker is concerned.
    let repeat = envelope(47);
    for _ in 0..3 {
        bus.publish(repeat.clone()).await.expect("publish");
    }
    assert_eq!(
        message_count(&context).await,
        1,
        "three publishes of one generation, one message"
    );
    assert_eq!(on_the_stream(&context).await[0].generation, 47);
}

/// A broker that is not there must surface as an error at connect. The caller
/// burns a generation on every publish, so a bus that reported success while
/// sending nothing would leave the fleet on the previous generation with
/// nothing anywhere to say so.
#[tokio::test]
async fn a_broker_that_is_not_there_fails_at_connect() {
    let err = NatsSnapshotBus::connect("nats://127.0.0.1:1")
        .await
        .expect_err("a broker that is not there must not connect");
    assert!(matches!(err, ancre_control::ControlError::Bus(_)), "{err}");
}
