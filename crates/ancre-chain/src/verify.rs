//! Chain verification.
//!
//! Must run offline, on an auditor's laptop, with no network and no
//! ClickHouse. Streaming: an evidence pack can be tens of millions of events
//! and must never need to be resident.

use ancre_canon::{GENESIS, Hash32, tree_root};
use ancre_types::AuditEvent;

use crate::event_hash;

/// A verification failure, always naming the `seq` it was found at.
///
/// "The chain is invalid" is useless to an auditor. "Event 41 207 does not
/// hash to the value event 41 208 claims for it" is a finding they can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `event_hash` does not match a recomputation over the event's own body.
    /// The event was altered after it was sealed.
    HashMismatch {
        seq: u64,
        stored: Hash32,
        computed: Hash32,
    },
    /// `prev_hash` does not match the previous event's `event_hash`. An event
    /// was removed, inserted, or reordered.
    BrokenLink {
        seq: u64,
        expected: Hash32,
        found: Hash32,
    },
    /// A `seq` is missing. The daily heartbeat event makes this
    /// distinguishable from a system that simply had no traffic.
    Gap { after: u64, before: u64 },
    /// `seq` did not advance. Two events claim the same position.
    Fork { seq: u64 },
    /// Rule set this build cannot evaluate.
    ///
    /// **Not a violation of the chain** — a limit of the verifier, and it
    /// reports itself as exactly that. Collapsing "I cannot check this" into
    /// "this is invalid" would be dishonest in the direction that costs the
    /// most credibility.
    UnknownCanonVersion { seq: u64, version: String },
    /// The event could not be canonically encoded at all.
    Unencodable { seq: u64, reason: String },
}

impl Violation {
    #[must_use]
    pub fn seq(&self) -> u64 {
        match self {
            Self::HashMismatch { seq, .. }
            | Self::BrokenLink { seq, .. }
            | Self::Fork { seq }
            | Self::UnknownCanonVersion { seq, .. }
            | Self::Unencodable { seq, .. } => *seq,
            Self::Gap { after, .. } => *after,
        }
    }

    /// True when this says something about the *verifier*, not the chain.
    #[must_use]
    pub fn is_verifier_limit(&self) -> bool {
        matches!(self, Self::UnknownCanonVersion { .. })
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HashMismatch {
                seq,
                stored,
                computed,
            } => write!(
                f,
                "event {seq} was altered after it was sealed: it records hash {stored} \
                 but its contents hash to {computed}"
            ),
            Self::BrokenLink {
                seq,
                expected,
                found,
            } => write!(
                f,
                "event {seq} does not follow the one before it: it points back to {found}, \
                 but the previous event hashes to {expected}"
            ),
            Self::Gap { after, before } => write!(
                f,
                "events {} to {} are missing from the record",
                after + 1,
                before - 1
            ),
            Self::Fork { seq } => write!(f, "two different events both claim position {seq}"),
            Self::UnknownCanonVersion { seq, version } => write!(
                f,
                "event {seq} was sealed under rule set '{version}', which this \
                 verifier does not implement — it cannot be checked either way"
            ),
            Self::Unencodable { seq, reason } => {
                write!(
                    f,
                    "event {seq} could not be re-encoded for checking: {reason}"
                )
            }
        }
    }
}

#[derive(Debug)]
pub struct ChainReport {
    pub events_checked: u64,
    pub seq_from: u64,
    pub seq_to: u64,
    pub violations: Vec<Violation>,
    /// `event_hash` of the last event seen. Feeds the next range when a pack
    /// is verified in chunks.
    pub head_hash: Hash32,
    /// Tree root over the range, for checking against a signed checkpoint.
    pub root_hash: Hash32,
    pub checkpoints_verified: u32,
}

impl ChainReport {
    /// No violations at all, including verifier limits. This is the only
    /// condition under which the CLI exits 0.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    /// True when nothing is wrong with the chain but part of it could not be
    /// evaluated. Exit code 2, not 1.
    #[must_use]
    pub fn is_inconclusive(&self) -> bool {
        !self.violations.is_empty() && self.violations.iter().all(Violation::is_verifier_limit)
    }
}

/// Verify a contiguous range.
///
/// `expected_prev` is the `event_hash` of the event immediately before the
/// range, or `GENESIS` at the head of a chain. Verification continues past a
/// violation rather than stopping at the first one: an auditor needs the whole
/// list of what is wrong, not the earliest thing that is wrong.
///
/// After a broken link, the walk re-anchors on the event it just read, so one
/// tampered event produces one finding instead of cascading through every
/// event after it.
pub fn verify_range<I>(events: I, expected_prev: Hash32) -> ChainReport
where
    I: IntoIterator<Item = AuditEvent>,
{
    let mut report = ChainReport {
        events_checked: 0,
        seq_from: 0,
        seq_to: 0,
        violations: Vec::new(),
        head_hash: expected_prev,
        root_hash: GENESIS,
        checkpoints_verified: 0,
    };
    let mut expected = expected_prev;
    let mut last_seq: Option<u64> = None;
    // The one thing here that is not streaming. 32 bytes per event, so a
    // 10M-event range costs 320MB — fine for a pack, not fine forever.
    // TODO(D1): fold leaves into the tree incrementally so the whole range
    // never has to be resident.
    let mut leaves: Vec<Hash32> = Vec::new();

    for event in events {
        if report.events_checked == 0 {
            report.seq_from = event.seq;
        }
        report.seq_to = event.seq;
        report.events_checked += 1;

        // Position: gaps and forks, before anything cryptographic. A missing
        // event is the finding an auditor most often actually has.
        if let Some(last) = last_seq {
            if event.seq == last {
                report.violations.push(Violation::Fork { seq: event.seq });
            } else if event.seq > last + 1 {
                report.violations.push(Violation::Gap {
                    after: last,
                    before: event.seq,
                });
            }
        }
        last_seq = Some(event.seq);

        // Link.
        if event.prev_hash != expected {
            report.violations.push(Violation::BrokenLink {
                seq: event.seq,
                expected,
                found: event.prev_hash,
            });
        }

        // Contents. A rule set we do not implement is reported as such and the
        // event is not judged.
        if str::eq(&event.canon_version, ancre_canon::CANON_VERSION) {
            match event_hash(&event) {
                Ok(computed) if computed != event.event_hash => {
                    report.violations.push(Violation::HashMismatch {
                        seq: event.seq,
                        stored: event.event_hash,
                        computed,
                    });
                }
                Ok(_) => {}
                Err(e) => report.violations.push(Violation::Unencodable {
                    seq: event.seq,
                    reason: e.to_string(),
                }),
            }
        } else {
            report.violations.push(Violation::UnknownCanonVersion {
                seq: event.seq,
                version: event.canon_version.to_string(),
            });
        }

        leaves.push(event.event_hash);
        // Re-anchor on what this event actually claims, so a single tampered
        // event does not make every subsequent link look broken.
        expected = event.event_hash;
        report.head_hash = event.event_hash;
    }

    report.root_hash = tree_root(&leaves);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancre_canon::GENESIS;
    use ancre_chain::fixtures::{chain, chain_from};
    use ancre_types::AuditEvent;

    /// One event, sealed, anchored where the caller says.
    fn event(seq: u64, prev: Hash32) -> AuditEvent {
        chain_from(seq, 1, prev).pop().unwrap()
    }

    #[test]
    fn a_clean_chain_verifies() {
        let report = verify_range(chain(1000), GENESIS);
        assert!(report.is_clean(), "{:?}", report.violations);
        assert_eq!(report.events_checked, 1000);
        assert_eq!(report.seq_from, 1);
        assert_eq!(report.seq_to, 1000);
    }

    /// M1's done-when, at the size the plan states (mvp-plan §5).
    #[test]
    fn a_corrupted_event_in_a_100k_chain_is_caught_and_named() {
        let mut events = chain(100_000);
        // Change one field of one event, leaving its stored hash alone —
        // exactly what someone editing a row in ClickHouse would do.
        events[41_206].emitted.metrics.http_status = 500;

        let report = verify_range(events, GENESIS);

        assert!(!report.is_clean());
        assert_eq!(report.events_checked, 100_000);

        let hash_mismatches: Vec<_> = report
            .violations
            .iter()
            .filter(|v| matches!(v, Violation::HashMismatch { .. }))
            .collect();
        assert_eq!(hash_mismatches.len(), 1, "{:?}", report.violations);
        assert_eq!(hash_mismatches[0].seq(), 41_207);
        assert!(hash_mismatches[0].to_string().contains("41207"));
    }

    #[test]
    fn one_tampered_event_produces_one_finding_not_a_cascade() {
        let mut events = chain(500);
        events[100].emitted.metrics.tokens_out = 9999;
        let report = verify_range(events, GENESIS);
        // Re-anchoring means the 399 events after it are not each reported.
        assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    }

    #[test]
    fn a_removed_event_is_caught_as_a_gap_and_a_broken_link() {
        let mut events = chain(100);
        events.remove(49); // seq 50
        let report = verify_range(events, GENESIS);

        assert!(report.violations.iter().any(|v| matches!(
            v,
            Violation::Gap {
                after: 49,
                before: 51
            }
        )));
        assert!(
            report
                .violations
                .iter()
                .any(|v| matches!(v, Violation::BrokenLink { seq: 51, .. }))
        );
    }

    #[test]
    fn reordering_two_events_is_detected() {
        let mut events = chain(100);
        events.swap(40, 41);
        let report = verify_range(events, GENESIS);
        assert!(!report.is_clean());
    }

    #[test]
    fn a_duplicated_event_is_a_fork() {
        let mut events = chain(50);
        let dup = events[20].clone();
        events.insert(21, dup);
        let report = verify_range(events, GENESIS);
        assert!(
            report
                .violations
                .iter()
                .any(|v| matches!(v, Violation::Fork { seq: 21 }))
        );
    }

    #[test]
    fn an_unknown_rule_set_is_inconclusive_not_invalid() {
        let mut events = chain(10);
        events[5].canon_version = "ancre-canon/99".into();
        let report = verify_range(events, GENESIS);

        assert!(!report.is_clean());
        assert!(
            report.is_inconclusive(),
            "an unimplemented rule set must not read as tampering: {:?}",
            report.violations
        );
    }

    #[test]
    fn tampering_and_an_unknown_rule_set_together_is_not_inconclusive() {
        let mut events = chain(10);
        events[5].canon_version = "ancre-canon/99".into();
        events[7].emitted.metrics.http_status = 500;
        let report = verify_range(events, GENESIS);
        assert!(!report.is_inconclusive());
    }

    #[test]
    fn the_range_root_matches_a_tree_over_the_same_event_hashes() {
        let events = chain(37);
        let expected = tree_root(&events.iter().map(|e| e.event_hash).collect::<Vec<_>>());
        let report = verify_range(events, GENESIS);
        assert_eq!(report.root_hash, expected);
    }

    #[test]
    fn an_empty_range_is_clean_and_says_so() {
        let report = verify_range(Vec::new(), GENESIS);
        assert!(report.is_clean());
        assert_eq!(report.events_checked, 0);
        assert_eq!(report.head_hash, GENESIS);
    }

    #[test]
    fn a_range_verified_in_two_chunks_matches_one_pass() {
        let events = chain(200);
        let (a, b) = events.split_at(120);

        let first = verify_range(a.to_vec(), GENESIS);
        assert!(first.is_clean());
        let second = verify_range(b.to_vec(), first.head_hash);
        assert!(second.is_clean(), "{:?}", second.violations);
        assert_eq!(second.seq_from, 121);
    }

    #[test]
    fn risk_flag_ordering_does_not_change_the_hash() {
        use ancre_types::RiskFlag;
        use smallvec::smallvec;
        let mut a = event(1, GENESIS);
        let mut b = a.clone();
        a.emitted.pins.risk_flags = smallvec![RiskFlag::StaleConfig, RiskFlag::UnpinnedModel];
        b.emitted.pins.risk_flags = smallvec![RiskFlag::UnpinnedModel, RiskFlag::StaleConfig];
        assert_eq!(
            crate::event_hash(&a).unwrap(),
            crate::event_hash(&b).unwrap()
        );
    }
}
