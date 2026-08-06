//! Seq allocation and chaining.
//!
//! **The ingester owns `seq` and the chain, not the gateway** (PRD §9). The
//! gateway emits unordered events with monotonic local timestamps and no
//! ordering claim. This keeps coordination entirely off the hot path, and it
//! means a gateway node dying mid-flight cannot leave a gap in a chain — there
//! is no chain position for it to die in the middle of.

use std::collections::HashMap;

use ancre_canon::{GENESIS, Hash32};
use ancre_chain::{ChainId, seal};
use ancre_types::{AuditEvent, EmittedEvent, EventType, Metrics, Outcome, Pins, Timestamp};
use uuid::Uuid;

/// How many recent `event_id`s to remember for deduplication.
///
/// JetStream redelivers on ack timeout, which in practice means seconds — so a
/// window of the last N events catches every realistic redelivery. It is a
/// *window*, not a guarantee: the durable check is the unique index on
/// `event_id` in the store. See `ChainWriter::append`.
pub const DEDUPE_WINDOW: usize = 100_000;

/// Per-chain cursor. One writer per `(tenant_id, system_id)` — two processes
/// chaining the same chain would fork it, so this is a single-writer design
/// and horizontal scale comes from partitioning chains across ingesters, not
/// from sharing one.
#[derive(Debug)]
pub struct ChainWriter {
    pub chain: ChainId,
    next_seq: u64,
    head_hash: Hash32,
    node_id: std::sync::Arc<str>,
    /// `event_id` → the seq it was already written at.
    seen: HashMap<Uuid, u64>,
    /// Insertion order, so the window can be trimmed without scanning.
    seen_order: std::collections::VecDeque<Uuid>,
    last_heartbeat_day: Option<i64>,
}

impl ChainWriter {
    /// A fresh chain, anchored at `GENESIS`.
    #[must_use]
    pub fn new(chain: ChainId, node_id: &str) -> Self {
        Self::resume_from(chain, node_id, 0, GENESIS)
    }

    /// Resume from what the store already holds: the max `seq` for this chain
    /// and its `event_hash`.
    ///
    /// Getting this wrong is how a restart forks a chain, so the two values
    /// must come from the same row — never from two queries.
    #[must_use]
    pub fn resume_from(chain: ChainId, node_id: &str, last_seq: u64, head_hash: Hash32) -> Self {
        Self {
            chain,
            next_seq: last_seq + 1,
            head_hash,
            node_id: std::sync::Arc::from(node_id),
            seen: HashMap::new(),
            seen_order: std::collections::VecDeque::new(),
            last_heartbeat_day: None,
        }
    }

    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    #[must_use]
    pub fn head_hash(&self) -> Hash32 {
        self.head_hash
    }

    /// Chain one event.
    ///
    /// JetStream is at-least-once, so redelivery is normal, not exceptional.
    /// A duplicate `event_id` returns `Duplicate` rather than allocating a new
    /// `seq` — getting this wrong does not error, it silently doubles every
    /// event while the chain still verifies perfectly. That is the worst shape
    /// a bug can have here, so redelivery is tested explicitly.
    pub fn append(&mut self, event: EmittedEvent) -> Result<AuditEvent, IngestError> {
        if let Some(&seq) = self.seen.get(&event.event_id) {
            return Err(IngestError::Duplicate(event.event_id, seq));
        }
        self.chain_event(event)
    }

    fn chain_event(&mut self, emitted: EmittedEvent) -> Result<AuditEvent, IngestError> {
        let event_id = emitted.event_id;
        let seq = self.next_seq;

        let mut event = AuditEvent {
            seq,
            prev_hash: self.head_hash,
            event_hash: GENESIS,
            canon_version: std::sync::Arc::from(ancre_canon::CANON_VERSION),
            ingested_at: Timestamp::now(),
            emitted,
            payload_ref: String::new(),
            subject_key_id: String::new(),
        };

        let hash = seal(&mut event)?;

        // Only advance after sealing succeeds. An event that cannot be encoded
        // must not consume a seq, or the chain acquires a gap that looks
        // exactly like a deleted event.
        self.head_hash = hash;
        self.next_seq += 1;
        self.remember(event_id, seq);

        Ok(event)
    }

    /// Put the cursor back to a known-stored position.
    ///
    /// Used when a batch was chained but the store refused it. The dedupe
    /// memory for those events is dropped too, because the redelivery that
    /// follows must be allowed through rather than rejected as a duplicate.
    pub fn rewind_to(&mut self, next_seq: u64, head_hash: Hash32) {
        self.next_seq = next_seq;
        self.head_hash = head_hash;
        self.forget_after(next_seq);
    }

    /// Drop dedupe entries for seqs at or after `seq`.
    ///
    /// Used when a batch is rolled back after a store failure: those events
    /// were chained but never stored, so the redelivery that follows must be
    /// allowed through rather than rejected as a duplicate.
    pub fn forget_after(&mut self, seq: u64) {
        self.seen.retain(|_, at| *at < seq);
        let live: std::collections::HashSet<Uuid> = self.seen.keys().copied().collect();
        self.seen_order.retain(|id| live.contains(id));
    }

    fn remember(&mut self, event_id: Uuid, seq: u64) {
        self.seen.insert(event_id, seq);
        self.seen_order.push_back(event_id);
        while self.seen_order.len() > DEDUPE_WINDOW {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
    }

    /// One per chain per day, traffic or no traffic (mvp-plan §8.4).
    ///
    /// Without it, a system that served nothing and a system that does not
    /// exist are indistinguishable in the record — and "we have no evidence
    /// for this period" must never look the same as "there was nothing to
    /// record". Absence of evidence has to stay countable.
    ///
    /// Returns `None` if today's heartbeat is already written.
    pub fn heartbeat(&mut self, now: Timestamp) -> Option<Result<AuditEvent, IngestError>> {
        let day = now.as_micros().div_euclid(86_400_000_000);
        if self.last_heartbeat_day == Some(day) {
            return None;
        }
        self.last_heartbeat_day = Some(day);

        let emitted = EmittedEvent {
            tenant_id: std::sync::Arc::from(self.chain.tenant_id.as_str()),
            system_id: std::sync::Arc::from(self.chain.system_id.as_str()),
            // Deterministic from (chain, day), so two ingesters that both try
            // to write today's heartbeat collide on the dedupe check instead
            // of writing two.
            event_id: heartbeat_id(&self.chain, day),
            trace_id: std::sync::Arc::from(""),
            attempt_seq: 0,
            occurred_at: now,
            node_id: std::sync::Arc::clone(&self.node_id),
            event_type: EventType::ChainHeartbeat,
            outcome: Outcome::Ok,
            pins: Pins::null_baseline(),
            request_digest: GENESIS,
            response_digest: GENESIS,
            metrics: Metrics {
                provider: std::sync::Arc::from(""),
                http_status: 0,
                latency_ms: 0,
                ttft_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                error_code: std::sync::Arc::from(""),
            },
        };

        Some(self.append(emitted))
    }
}

/// A UUIDv5-style deterministic id derived from the chain and the day.
fn heartbeat_id(chain: &ChainId, day: i64) -> Uuid {
    let h = ancre_canon::hash_bytes(
        format!(
            "ancre-heartbeat/1|{}|{}|{day}",
            chain.tenant_id, chain.system_id
        )
        .as_bytes(),
    );
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&h.as_bytes()[..16]);
    Uuid::from_bytes(bytes)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IngestError {
    #[error("store: {0}")]
    Store(String),
    #[error("chain: {0}")]
    Chain(#[from] ancre_chain::ChainError),
    #[error("event {0} was already chained at seq {1}")]
    Duplicate(Uuid, u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancre_chain::verify_range;

    fn chain_id() -> ChainId {
        ChainId {
            tenant_id: "acme".into(),
            system_id: "hr-screening".into(),
        }
    }

    fn writer() -> ChainWriter {
        ChainWriter::new(chain_id(), "ing-1")
    }

    fn emitted(n: u64) -> EmittedEvent {
        ancre_types::fixtures::event(n, GENESIS).emitted
    }

    #[test]
    fn a_fresh_chain_starts_at_seq_one_anchored_at_genesis() {
        let mut w = writer();
        let e = w.append(emitted(1)).unwrap();

        assert_eq!(e.seq, 1);
        assert_eq!(e.prev_hash, GENESIS);
        assert_eq!(w.next_seq(), 2);
    }

    #[test]
    fn appended_events_form_a_chain_that_verifies() {
        let mut w = writer();
        let events: Vec<_> = (1..=1000).map(|i| w.append(emitted(i)).unwrap()).collect();

        let report = verify_range(events, GENESIS);
        assert!(report.is_clean(), "{:?}", report.violations);
        assert_eq!(report.events_checked, 1000);
        assert_eq!(report.head_hash, w.head_hash());
    }

    /// The bug that does not error: a redelivered event chained twice leaves a
    /// chain that verifies perfectly and describes twice as much traffic as
    /// actually happened.
    #[test]
    fn a_redelivered_event_is_refused_rather_than_chained_twice() {
        let mut w = writer();
        let e = emitted(1);

        let first = w.append(e.clone()).unwrap();
        assert_eq!(first.seq, 1);

        assert_eq!(
            w.append(e.clone()).unwrap_err(),
            IngestError::Duplicate(e.event_id, 1)
        );
        assert_eq!(w.next_seq(), 2, "a duplicate must not consume a seq");
    }

    #[test]
    fn redelivery_of_an_older_event_within_the_window_is_still_caught() {
        let mut w = writer();
        let events: Vec<_> = (1..=500).map(emitted).collect();
        for e in &events {
            w.append(e.clone()).unwrap();
        }
        // JetStream redelivers #7 after 493 others have gone past.
        assert!(matches!(
            w.append(events[6].clone()),
            Err(IngestError::Duplicate(_, 7))
        ));
    }

    #[test]
    fn resuming_from_the_store_continues_the_chain_without_a_gap() {
        let mut first = writer();
        let before: Vec<_> = (1..=100)
            .map(|i| first.append(emitted(i)).unwrap())
            .collect();

        // Restart: resume from the last row.
        let mut after = ChainWriter::resume_from(chain_id(), "ing-1", 100, first.head_hash());
        let later: Vec<_> = (101..=200)
            .map(|i| after.append(emitted(i)).unwrap())
            .collect();

        assert_eq!(later[0].seq, 101);
        assert_eq!(later[0].prev_hash, before[99].event_hash);

        let all: Vec<_> = before.into_iter().chain(later).collect();
        assert!(verify_range(all, GENESIS).is_clean());
    }

    #[test]
    fn a_heartbeat_is_written_once_per_day() {
        let mut w = writer();
        let day1 = Timestamp::from_micros(1_754_400_000_000_000);

        let hb = w.heartbeat(day1).expect("first of the day").unwrap();
        assert_eq!(hb.emitted.event_type, EventType::ChainHeartbeat);
        assert_eq!(hb.seq, 1);

        // Later the same day: nothing.
        assert!(
            w.heartbeat(Timestamp::from_micros(day1.as_micros() + 3_600_000_000))
                .is_none()
        );

        // Next day: another one.
        let day2 = Timestamp::from_micros(day1.as_micros() + 86_400_000_000);
        assert!(w.heartbeat(day2).is_some());
    }

    /// A silent system must still leave a chain that an auditor can read, or
    /// "no traffic" and "no such system" are the same shape in the record.
    #[test]
    fn a_chain_with_only_heartbeats_still_verifies() {
        let mut w = writer();
        let mut events = Vec::new();
        for day in 0..30 {
            let t = Timestamp::from_micros(1_754_400_000_000_000 + day * 86_400_000_000);
            events.push(w.heartbeat(t).unwrap().unwrap());
        }

        assert_eq!(events.len(), 30);
        assert!(verify_range(events, GENESIS).is_clean());
    }

    #[test]
    fn heartbeat_ids_are_deterministic_so_two_ingesters_cannot_write_two() {
        let day = 20_306;
        assert_eq!(
            heartbeat_id(&chain_id(), day),
            heartbeat_id(&chain_id(), day)
        );
        assert_ne!(
            heartbeat_id(&chain_id(), day),
            heartbeat_id(&chain_id(), day + 1)
        );

        let other = ChainId {
            tenant_id: "acme".into(),
            system_id: "credit-scoring".into(),
        };
        assert_ne!(heartbeat_id(&chain_id(), day), heartbeat_id(&other, day));
    }

    #[test]
    fn the_dedupe_window_is_bounded() {
        let mut w = writer();
        for i in 1..=(DEDUPE_WINDOW as u64 + 100) {
            w.append(emitted(i)).unwrap();
        }
        assert!(
            w.seen.len() <= DEDUPE_WINDOW,
            "the dedupe set must not grow without bound"
        );
    }
}
