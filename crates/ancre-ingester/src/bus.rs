//! The NATS JetStream consumer.
//!
//! Pull a batch, chain it, store it, then decide the batch's fate in one move:
//! **ack everything or nak everything.** That is not a simplification — it is
//! the contract `Ingester::ingest` already implements. A batch that fails to
//! store is rolled back across every chain it touched, so the events in it
//! were never written and every one of them has to come back. Acking a subset
//! would leave the store missing events the bus believes were consumed, and
//! the chain would resume past a gap it can never fill.
//!
//! The consumer is durable: its position survives a restart, which is what
//! makes "the ingester catches up" true after a crash rather than aspirational.

use std::time::Duration;

use ancre_types::EmittedEvent;
use async_nats::jetstream;
use futures_util::StreamExt;

use crate::chain_writer::IngestError;
use crate::pipeline::{IngestReport, Ingester};
use crate::sink::EventStore;

/// Must match the gateway's. Both call `get_or_create_stream` with the same
/// config, so whichever starts first wins and neither needs ordering.
pub const STREAM: &str = "ANCRE_EVENTS";
pub const SUBJECT_ROOT: &str = "ancre.events";
/// Default durable consumer name — one ingester taking the whole stream. See
/// `ConsumerConfig::filter_subject` for how that grows to several.
pub const CONSUMER: &str = "ancre-ingester";

#[derive(Debug, Clone)]
pub struct ConsumerConfig {
    /// Durable consumer name. Its position survives a restart, which is what
    /// makes "the ingester catches up" true after a crash.
    pub durable_name: String,
    /// Which chains this ingester is responsible for.
    ///
    /// **This is the scale path.** One writer per chain is a correctness
    /// requirement — two processes chaining the same `(tenant, system)` would
    /// fork it — so growing past one ingester means splitting the subject
    /// space, not sharing a consumer. The subject already carries the chain,
    /// so that is a config change and not a wire change.
    pub filter_subject: String,
    /// Upper bound on one pull. The same figure as the gateway's batcher, so
    /// a full gateway batch usually arrives as one ingester batch.
    pub max_messages: usize,
    /// How long a pull waits before returning what it has. Bounds how long an
    /// event sits unstored on a quiet system.
    pub max_wait: Duration,
    /// How long JetStream waits before redelivering a nak'd batch. Long enough
    /// that a store outage is not a hot loop, short enough that recovery is
    /// not perceptibly delayed.
    pub redelivery_delay: Duration,
}

impl Default for ConsumerConfig {
    fn default() -> Self {
        Self {
            durable_name: CONSUMER.to_string(),
            filter_subject: format!("{SUBJECT_ROOT}.>"),
            max_messages: 512,
            max_wait: Duration::from_millis(500),
            redelivery_delay: Duration::from_secs(2),
        }
    }
}

/// Cumulative, for the log line and for the metric that says whether the
/// ingester is keeping up.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ConsumeReport {
    pub batches: u64,
    pub messages: u64,
    pub inserted: u64,
    pub duplicates: u64,
    /// Batches handed back for redelivery. Non-zero means the store is
    /// refusing writes — the number to alert on.
    pub redelivered: u64,
    /// Messages that could not be decoded at all. See `run`.
    pub undecodable: u64,
}

pub struct NatsConsumer<S: EventStore> {
    consumer: jetstream::consumer::PullConsumer,
    ingester: Ingester<S>,
    config: ConsumerConfig,
}

impl<S: EventStore> std::fmt::Debug for NatsConsumer<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsConsumer")
            .field("durable_name", &self.config.durable_name)
            .field("filter_subject", &self.config.filter_subject)
            .finish_non_exhaustive()
    }
}

impl<S: EventStore> NatsConsumer<S> {
    pub async fn connect(
        url: &str,
        ingester: Ingester<S>,
        config: ConsumerConfig,
    ) -> Result<Self, IngestError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| IngestError::Store(format!("nats: {e}")))?;
        Self::from_context(jetstream::new(client), ingester, config).await
    }

    pub async fn from_context(
        context: jetstream::Context,
        ingester: Ingester<S>,
        config: ConsumerConfig,
    ) -> Result<Self, IngestError> {
        let stream = context
            .get_or_create_stream(jetstream::stream::Config {
                name: STREAM.to_string(),
                subjects: vec![format!("{SUBJECT_ROOT}.>")],
                retention: jetstream::stream::RetentionPolicy::Limits,
                storage: jetstream::stream::StorageType::File,
                max_age: Duration::from_secs(7 * 24 * 3600),
                duplicate_window: Duration::from_secs(300),
                ..Default::default()
            })
            .await
            .map_err(|e| IngestError::Store(format!("nats stream: {e}")))?;

        let consumer = stream
            .get_or_create_consumer(
                &config.durable_name,
                jetstream::consumer::pull::Config {
                    durable_name: Some(config.durable_name.clone()),
                    filter_subject: config.filter_subject.clone(),
                    // Explicit, because the whole ack/nak contract depends on
                    // it. With any other policy a redelivery would not happen
                    // and a rolled-back batch would be lost.
                    ack_policy: jetstream::consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| IngestError::Store(format!("nats consumer: {e}")))?;

        Ok(Self {
            consumer,
            ingester,
            config,
        })
    }

    /// One pull, chained and stored. Returns what it did.
    ///
    /// A batch that the store refuses is nak'd whole and comes back; nothing
    /// in it was written, because `ingest` rewound every chain it touched.
    pub async fn pull_once(&mut self) -> Result<ConsumeReport, IngestError> {
        let mut report = ConsumeReport::default();

        let mut messages = self
            .consumer
            .batch()
            .max_messages(self.config.max_messages)
            .expires(self.config.max_wait)
            .messages()
            .await
            .map_err(|e| IngestError::Store(format!("nats fetch: {e}")))?;

        let mut batch: Vec<EmittedEvent> = Vec::new();
        let mut pending = Vec::new();
        let mut undecodable = Vec::new();

        while let Some(message) = messages.next().await {
            let message = message.map_err(|e| IngestError::Store(format!("nats message: {e}")))?;
            match crate::bus::decode(&message.payload) {
                Ok(event) => {
                    batch.push(event);
                    pending.push(message);
                }
                Err(_) => undecodable.push(message),
            }
        }

        // A message that will never decode must not be redelivered forever:
        // it would block the consumer behind a poison pill and stop every
        // chain. Ack it, count it, and let the count be the alarm — an
        // undecodable event is a gap, and a countable gap is the design.
        for message in undecodable {
            report.undecodable += 1;
            let _ = message.ack().await;
        }

        if batch.is_empty() {
            return Ok(report);
        }

        report.batches = 1;
        report.messages = pending.len() as u64;

        let ingested: IngestReport = self.ingester.ingest(batch).await?;
        report.inserted = ingested.inserted;
        report.duplicates = ingested.duplicates;

        if ingested.deferred > 0 {
            report.redelivered = 1;
            for message in pending {
                let _ = message
                    .ack_with(jetstream::AckKind::Nak(Some(self.config.redelivery_delay)))
                    .await;
            }
        } else {
            for message in pending {
                message
                    .ack()
                    .await
                    .map_err(|e| IngestError::Store(format!("nats ack: {e}")))?;
            }
        }

        Ok(report)
    }

    /// Pull until `shutdown` resolves.
    pub async fn run(mut self, shutdown: impl Future<Output = ()> + Send) -> ConsumeReport {
        let mut total = ConsumeReport::default();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => return total,
                result = self.pull_once() => match result {
                    Ok(r) => {
                        total.batches += r.batches;
                        total.messages += r.messages;
                        total.inserted += r.inserted;
                        total.duplicates += r.duplicates;
                        total.redelivered += r.redelivered;
                        total.undecodable += r.undecodable;
                        if r.redelivered > 0 {
                            tracing::warn!(
                                messages = r.messages,
                                "batch deferred: the store refused it, handed back for redelivery",
                            );
                        }
                        if r.undecodable > 0 {
                            tracing::error!(
                                count = r.undecodable,
                                "undecodable messages acked and skipped — this is a gap in the record",
                            );
                        }
                    }
                    Err(e) => {
                        // The bus itself is unreachable. Back off rather than
                        // spin; the events are durable on the stream and
                        // nothing is lost by waiting.
                        tracing::warn!(error = %e, "pull failed; retrying");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                },
            }
        }
    }

    #[must_use]
    pub fn ingester(&self) -> &Ingester<S> {
        &self.ingester
    }
}

/// Decode an event off the bus.
///
/// The encoder lives in `ancre-gateway`; this is the other half of the same
/// wire format, and `the_wire_format_round_trips_every_hashed_field` there is
/// what holds them together.
pub fn decode(payload: &[u8]) -> Result<EmittedEvent, serde_json::Error> {
    serde_json::from_slice(payload)
}
