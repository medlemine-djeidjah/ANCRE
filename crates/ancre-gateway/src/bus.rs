//! The NATS JetStream `EventSink`.
//!
//! The gateway's only job here is to hand events off durably and then forget
//! about them. It is still **never awaited by the request** — the `Batcher`
//! owns this on its own task, and a bus that is down fills the bounded
//! channel, drops events, counts the drops and keeps serving (PRD §8).
//!
//! Two properties are worth more than the code that provides them:
//!
//! - **Publish is acknowledged.** A fire-and-forget publish would report
//!   success for events NATS never stored, and the drop counter — the thing
//!   that makes a gap countable — would read zero while evidence vanished.
//! - **Every message carries `Nats-Msg-Id`.** JetStream dedupes on it within
//!   its duplicate window, so a retried publish after a timeout does not put
//!   the same event on the stream twice. The ingester dedupes again on
//!   `event_id`; this is the cheaper first line.

use std::sync::Arc;

use ancre_types::EmittedEvent;
use async_nats::jetstream;

use crate::telemetry::{EventSink, SinkError};

/// The stream every gateway publishes into.
pub const STREAM: &str = "ANCRE_EVENTS";
/// Subject root. Chains are `(tenant, system)`, so the subject carries both —
/// which is what lets a future deployment partition chains across ingesters by
/// subject filter without changing the wire format.
pub const SUBJECT_ROOT: &str = "ancre.events";

/// Subject for a chain: `ancre.events.<tenant>.<system>`.
///
/// Tokens are sanitised because `.`, ` ` and `*` are structural in a NATS
/// subject and a tenant id containing one would silently change the subject's
/// shape. Two ids that sanitise to the same token collide on one subject, and
/// that is harmless on purpose: **the ingester reads chain identity from the
/// payload, never from the subject.** The subject is routing, not evidence.
#[must_use]
pub fn subject_for(tenant_id: &str, system_id: &str) -> String {
    format!(
        "{SUBJECT_ROOT}.{}.{}",
        sanitise(tenant_id),
        sanitise(system_id)
    )
}

fn sanitise(token: &str) -> String {
    let cleaned: String = token
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".into()
    } else {
        cleaned
    }
}

pub struct NatsSink {
    context: jetstream::Context,
}

impl std::fmt::Debug for NatsSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsSink").finish_non_exhaustive()
    }
}

impl NatsSink {
    /// Connect and ensure the stream exists.
    ///
    /// Creating it here as well as in the ingester is deliberate: whichever
    /// process starts first wins, and neither has to be ordered after the
    /// other in a compose file. `get_or_create_stream` is idempotent, and both
    /// pass the same config.
    pub async fn connect(url: &str) -> Result<Self, SinkError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| SinkError::Unavailable(e.to_string()))?;
        let context = jetstream::new(client);
        ensure_stream(&context)
            .await
            .map_err(|e| SinkError::Unavailable(e.to_string()))?;
        Ok(Self { context })
    }

    #[must_use]
    pub fn from_context(context: jetstream::Context) -> Self {
        Self { context }
    }
}

/// The stream config, in one place so the gateway and the ingester cannot
/// disagree about it.
///
/// `Limits` retention rather than `WorkQueue`: the durable record is
/// ClickHouse, and a stream that deletes on ack leaves nothing to replay if an
/// ingester acks a batch it then fails to store. `max_age` bounds the buffer
/// so a long ClickHouse outage degrades into a countable gap rather than a
/// full disk.
pub async fn ensure_stream(context: &jetstream::Context) -> Result<(), async_nats::Error> {
    context
        .get_or_create_stream(jetstream::stream::Config {
            name: STREAM.to_string(),
            subjects: vec![format!("{SUBJECT_ROOT}.>")],
            retention: jetstream::stream::RetentionPolicy::Limits,
            storage: jetstream::stream::StorageType::File,
            max_age: std::time::Duration::from_secs(7 * 24 * 3600),
            // Publisher-side dedupe on `Nats-Msg-Id`. Generous, because the
            // thing it guards against is a publish retried after a timeout.
            duplicate_window: std::time::Duration::from_secs(300),
            ..Default::default()
        })
        .await?;
    Ok(())
}

impl EventSink for NatsSink {
    /// Publish a batch, then wait for every acknowledgement.
    ///
    /// Publishes are issued first and awaited afterwards, so a batch of 512
    /// costs one round trip rather than 512. The whole batch fails if any
    /// single ack does — the caller's only correct response is to count the
    /// batch as dropped, and reporting partial success would leave the drop
    /// counter understating the gap.
    async fn publish(&self, batch: Vec<EmittedEvent>) -> Result<(), SinkError> {
        let mut acks = Vec::with_capacity(batch.len());

        for event in &batch {
            let payload = serde_json::to_vec(event)
                .map_err(|e| SinkError::Unavailable(format!("encoding event: {e}")))?;

            let mut headers = async_nats::HeaderMap::new();
            headers.insert("Nats-Msg-Id", event.event_id.to_string().as_str());

            let ack = self
                .context
                .publish_with_headers(
                    subject_for(&event.tenant_id, &event.system_id),
                    headers,
                    payload.into(),
                )
                .await
                .map_err(|e| SinkError::Unavailable(e.to_string()))?;
            acks.push(ack);
        }

        for ack in acks {
            ack.await
                .map_err(|e| SinkError::Unavailable(e.to_string()))?;
        }
        Ok(())
    }
}

/// Decode an event off the bus. Here rather than in the ingester so both ends
/// of the wire format live in one file.
pub fn decode(payload: &[u8]) -> Result<EmittedEvent, serde_json::Error> {
    serde_json::from_slice(payload)
}

/// A shared sink, so `Batcher` can own one while the process keeps a handle.
pub type SharedSink = Arc<NatsSink>;

impl EventSink for SharedSink {
    async fn publish(&self, batch: Vec<EmittedEvent>) -> Result<(), SinkError> {
        (**self).publish(batch).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancre_canon::GENESIS;

    fn event() -> EmittedEvent {
        ancre_types::fixtures::event(1, GENESIS).emitted
    }

    #[test]
    fn a_subject_names_the_chain() {
        assert_eq!(
            subject_for("acme", "hr-screening"),
            "ancre.events.acme.hr-screening"
        );
    }

    /// A dot in a tenant id would add a token and change the subject's shape,
    /// which is how a wildcard subscription silently stops matching.
    #[test]
    fn structural_characters_cannot_leak_into_a_subject() {
        for bad in ["a.b", "a b", "a*b", "a>b", "a\tb"] {
            let subject = subject_for(bad, "sys");
            assert_eq!(
                subject.split('.').count(),
                4,
                "{subject} has the wrong token count"
            );
            assert!(
                !subject.contains('*') && !subject.contains('>'),
                "{subject}"
            );
        }
    }

    #[test]
    fn an_empty_token_still_produces_a_valid_subject() {
        assert_eq!(subject_for("", ""), "ancre.events._._");
    }

    /// The bus carries JSON, and the ingester hashes what comes off it. If a
    /// round trip changed one hashed field, every chain would be built from
    /// bytes the gateway never saw.
    #[test]
    fn the_wire_format_round_trips_every_hashed_field() {
        let original = event();
        let back = decode(&serde_json::to_vec(&original).unwrap()).unwrap();

        // Seal both into an event and compare the hashes — the only
        // comparison that covers the whole hashed set at once.
        let seal = |emitted: EmittedEvent| {
            let mut e = ancre_types::fixtures::event(1, GENESIS);
            e.emitted = emitted;
            ancre_chain::seal(&mut e).unwrap()
        };
        assert_eq!(seal(original), seal(back));
    }

    #[test]
    fn a_truncated_payload_is_refused_rather_than_partly_decoded() {
        let bytes = serde_json::to_vec(&event()).unwrap();
        assert!(decode(&bytes[..bytes.len() / 2]).is_err());
    }
}
