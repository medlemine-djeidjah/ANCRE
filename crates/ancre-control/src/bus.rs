//! The NATS JetStream `SnapshotBus`.
//!
//! One subject, one message that matters: the current generation. The stream
//! keeps exactly one message per subject, so a gateway node that starts an hour
//! after the last publish reads the current configuration immediately instead
//! of waiting for the next change. Without that, a fleet scaled up on a quiet
//! afternoon would sit cold until someone edited the registry.
//!
//! The bus is **not** part of the trust boundary. Anyone who can reach it can
//! replay or edit a message, which is why the envelope carries its own content
//! hash and the receiver recomputes it before installing anything. What is
//! required here is only that a publish which reports success actually
//! happened — hence the awaited ack.

use ancre_types::{SNAPSHOT_SUBJECT, SnapshotEnvelope};
use async_nats::jetstream;

use crate::envelope::SnapshotBus;
use crate::registry::ControlError;

/// The config stream. Separate from `ANCRE_EVENTS` because the two have
/// opposite retention needs: events must be kept until an ingester has stored
/// them, configuration must be collapsed to the latest.
pub const STREAM: &str = "ANCRE_CONFIG";

pub struct NatsSnapshotBus {
    context: jetstream::Context,
}

impl std::fmt::Debug for NatsSnapshotBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsSnapshotBus").finish_non_exhaustive()
    }
}

impl NatsSnapshotBus {
    pub async fn connect(url: &str) -> Result<Self, ControlError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| ControlError::Bus(e.to_string()))?;
        let context = jetstream::new(client);
        ensure_stream(&context)
            .await
            .map_err(|e| ControlError::Bus(e.to_string()))?;
        Ok(Self { context })
    }

    #[must_use]
    pub fn from_context(context: jetstream::Context) -> Self {
        Self { context }
    }
}

/// The stream config, in one place so the control plane and a gateway cannot
/// disagree about it. `get_or_create_stream` is idempotent and both ends pass
/// the same config, so neither has to start first.
///
/// `max_messages_per_subject: 1` is what "retained" means here: the stream
/// holds the latest snapshot and nothing else. A gateway consumes it with an
/// ordered ephemeral consumer starting at `DeliverPolicy::Last`, which gets the
/// current generation on connect and every later one as it lands.
///
/// Keeping a history would be worse than useless. A replayed old generation is
/// a gateway installing configuration that has been superseded, and the
/// envelope's hash cannot catch that — an old envelope is internally
/// consistent. The stream not holding one is the cheapest available defence.
pub async fn ensure_stream(context: &jetstream::Context) -> Result<(), async_nats::Error> {
    context
        .get_or_create_stream(jetstream::stream::Config {
            name: STREAM.to_string(),
            subjects: vec![SNAPSHOT_SUBJECT.to_string()],
            retention: jetstream::stream::RetentionPolicy::Limits,
            storage: jetstream::stream::StorageType::File,
            max_messages_per_subject: 1,
            // Dedupe on `Nats-Msg-Id`, which carries the generation. A publish
            // retried after a timeout must not put the same generation on the
            // stream twice.
            duplicate_window: std::time::Duration::from_secs(120),
            ..Default::default()
        })
        .await?;
    Ok(())
}

impl SnapshotBus for NatsSnapshotBus {
    /// Publish and wait for the acknowledgement.
    ///
    /// Awaited rather than fire-and-forget because the caller has already
    /// allocated and burned a generation by this point. A send that silently
    /// failed would leave the fleet on the previous generation while the
    /// registry believes it published — and the gap would close only when
    /// something else changed, or on the next poll, up to the staleness
    /// budget later.
    async fn publish(&self, envelope: SnapshotEnvelope) -> Result<(), ControlError> {
        let payload = serde_json::to_vec(&envelope)
            .map_err(|e| ControlError::Canon(format!("encoding snapshot: {e}")))?;

        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Msg-Id", msg_id(&envelope).as_str());

        self.context
            .publish_with_headers(SNAPSHOT_SUBJECT, headers, payload.into())
            .await
            .map_err(|e| ControlError::Bus(e.to_string()))?
            .await
            .map_err(|e| ControlError::Bus(e.to_string()))?;
        Ok(())
    }
}

/// Dedupe id: the generation and the content it carries.
///
/// The generation alone would be enough while one control plane allocates
/// them. Including the hash makes the id say what it identifies, so two
/// replicas that somehow published different content under one generation
/// produce different ids rather than one silently swallowing the other — the
/// broker stops hiding the bug.
fn msg_id(envelope: &SnapshotEnvelope) -> String {
    format!(
        "{}:{}",
        envelope.generation,
        &envelope.content_hash.to_hex()[..16]
    )
}

/// Decode a snapshot off the bus. Here rather than in the gateway so both ends
/// of the wire format live in one file.
pub fn decode(payload: &[u8]) -> Result<SnapshotEnvelope, serde_json::Error> {
    serde_json::from_slice(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(generation: u64) -> SnapshotEnvelope {
        let mut spec = ancre_resolver::testing::spec(generation);
        spec.generation = generation;
        SnapshotEnvelope::seal_now(spec).unwrap()
    }

    /// The bus carries JSON, and what comes off it becomes every `config_hash`
    /// pin the fleet writes. A round trip that changed one byte of the spec
    /// would be caught by `verify()` — this checks it does not have to be.
    #[test]
    fn the_wire_format_round_trips_and_still_verifies() {
        let original = envelope(41);
        let back = decode(&serde_json::to_vec(&original).unwrap()).unwrap();

        assert_eq!(back.generation, 41);
        assert_eq!(back.content_hash, original.content_hash);
        assert!(back.verify().is_ok());
    }

    #[test]
    fn a_truncated_payload_is_refused_rather_than_partly_decoded() {
        let bytes = serde_json::to_vec(&envelope(1)).unwrap();
        assert!(decode(&bytes[..bytes.len() / 2]).is_err());
    }

    /// Two generations must never share a dedupe id, or the broker drops the
    /// second and the fleet stays on the first.
    #[test]
    fn the_dedupe_id_separates_generations_and_contents() {
        assert_ne!(msg_id(&envelope(1)), msg_id(&envelope(2)));

        let a = envelope(7);
        let mut b = a.clone();
        b.content_hash = ancre_canon::hash_bytes(b"different");
        assert_ne!(msg_id(&a), msg_id(&b));

        // The same publish retried is the same id — that is the point.
        assert_eq!(msg_id(&a), msg_id(&a.clone()));
    }
}
