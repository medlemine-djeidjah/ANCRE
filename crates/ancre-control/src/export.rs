//! Getting a chain out, in the form the verifier already reads.
//!
//! This is the last mile of the entire product. Events that chain, verify and
//! carry a signed checkpoint are worth nothing if the only way to see them is a
//! SQL client — an auditor does not get a ClickHouse login, and "trust this
//! query" is the opposite of the claim being made.
//!
//! The format is deliberately the dullest one available: **one JSON event per
//! line**, exactly what `ancre-verify --chain` already parses and exactly what
//! `gen-fixture` already emits. No new encoding, no manifest, no framing to get
//! wrong. The whole demo becomes a pipe:
//!
//! ```sh
//! curl -s localhost:8081/v1/chains/acme/hr-screening/events | ancre-verify --chain -
//! ```
//!
//! Streamed and paged, because the one thing an export must not do is decide
//! how much memory it needs based on how long the customer has been a customer.
//! D1 is the same problem on the verification side; here it is bounded by
//! `PAGE`, so a 10-million-event chain costs the same resident memory as a
//! thousand-event one.

use ancre_chain::ChainId;
use ancre_types::AuditEvent;

use crate::registry::ControlError;

/// Events per round trip to the store. Small enough that the response starts
/// flowing immediately, large enough that a long chain is not a million
/// queries.
pub const PAGE: u64 = 10_000;

/// Reading whole events back out. Separate from `ChainSource`, which reads only
/// the hashes the checkpointer needs — export is the one path that has to
/// reconstruct entire events, and it is worth being able to see which callers
/// can do that.
pub trait ChainExport: Send + Sync + 'static {
    /// Events for `seq_from..=seq_to`, in `seq` order.
    ///
    /// The order is not a nicety: `verify_range` walks the chain link by link,
    /// so an unordered read reports violations that are an artefact of the
    /// query rather than the data — and an auditor cannot tell those apart.
    fn events(
        &self,
        chain: &ChainId,
        seq_from: u64,
        seq_to: u64,
    ) -> impl std::future::Future<Output = Result<Vec<AuditEvent>, ControlError>> + Send;
}

impl<T: ChainExport> ChainExport for std::sync::Arc<T> {
    fn events(
        &self,
        chain: &ChainId,
        seq_from: u64,
        seq_to: u64,
    ) -> impl std::future::Future<Output = Result<Vec<AuditEvent>, ControlError>> + Send {
        (**self).events(chain, seq_from, seq_to)
    }
}

/// Serialise one page as JSONL.
///
/// A serialisation failure is fatal to the export rather than skipped, for the
/// same reason the verifier refuses a line it cannot parse: an event silently
/// dropped from the middle of a chain leaves a gap, and a gap that nobody
/// reports is indistinguishable from tampering that nobody caught.
pub fn to_jsonl(events: &[AuditEvent]) -> Result<Vec<u8>, ControlError> {
    let mut out = Vec::with_capacity(events.len() * 512);
    for event in events {
        serde_json::to_writer(&mut out, event)
            .map_err(|e| ControlError::Canon(format!("event {}: {e}", event.seq)))?;
        out.push(b'\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancre_canon::GENESIS;

    fn chain(n: u64) -> Vec<AuditEvent> {
        let mut prev = GENESIS;
        (1..=n)
            .map(|seq| {
                let mut e = ancre_types::fixtures::event(seq, prev);
                ancre_chain::seal(&mut e).unwrap();
                prev = e.event_hash;
                e
            })
            .collect()
    }

    /// The format the verifier reads, round-tripped through the exact parse it
    /// performs. If these two ever disagree the export is unverifiable, and the
    /// only place that shows up is on the auditor's laptop.
    #[test]
    fn every_line_parses_back_into_the_event_it_came_from() {
        let events = chain(50);
        let jsonl = to_jsonl(&events).unwrap();
        let text = String::from_utf8(jsonl).unwrap();

        let back: Vec<AuditEvent> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line must parse"))
            .collect();

        assert_eq!(back.len(), 50);
        for (original, parsed) in events.iter().zip(&back) {
            assert_eq!(parsed.event_hash, original.event_hash);
            assert_eq!(
                ancre_chain::event_hash(parsed).unwrap(),
                original.event_hash,
                "seq {}: the exported form does not rehash to its own hash",
                original.seq
            );
        }
    }

    /// The exported chain has to verify as a chain, not merely parse.
    #[test]
    fn an_exported_chain_verifies() {
        let jsonl = to_jsonl(&chain(200)).unwrap();
        let events: Vec<AuditEvent> = String::from_utf8(jsonl)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let report = ancre_chain::verify_range(events, GENESIS);
        assert!(report.is_clean(), "{:?}", report.violations);
        assert_eq!(report.seq_to, 200);
    }

    /// Newline-delimited means exactly one newline per event and none inside
    /// one. `serde_json` does not emit raw newlines, and a line count that
    /// disagrees with the event count would silently truncate the last event.
    #[test]
    fn there_is_exactly_one_line_per_event() {
        let jsonl = to_jsonl(&chain(37)).unwrap();
        #[allow(
            clippy::naive_bytecount,
            reason = "37 events; a crate for this is absurd"
        )]
        let newlines = jsonl.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(newlines, 37);
        assert!(jsonl.ends_with(b"\n"), "the last line must be terminated");
    }

    #[test]
    fn an_empty_range_is_an_empty_body_rather_than_an_error() {
        assert!(to_jsonl(&[]).unwrap().is_empty());
    }
}
