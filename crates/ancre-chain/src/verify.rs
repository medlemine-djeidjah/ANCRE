//! Chain verification.
//!
//! Must run offline, on an auditor's laptop, with no network and no ClickHouse.

use ancre_canon::Hash32;
use ancre_types::AuditEvent;

/// A verification failure, always naming the `seq` it was found at. "The chain
/// is invalid" is useless to an auditor; "event 41 207 does not hash to the
/// value event 41 208 claims for it" is a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `event_hash` does not match a recomputation of `hashed_body`.
    HashMismatch {
        seq: u64,
        stored: Hash32,
        computed: Hash32,
    },
    /// `prev_hash` does not match the previous event's `event_hash`.
    BrokenLink {
        seq: u64,
        expected: Hash32,
        found: Hash32,
    },
    /// A `seq` is missing. The heartbeat event makes this distinguishable from
    /// a system that simply had no traffic.
    Gap { after: u64, before: u64 },
    /// The same `seq` appears twice with different hashes.
    Fork { seq: u64 },
    /// Rule set this build cannot evaluate. Not a violation of the chain — a
    /// limit of the verifier, and it must report itself as exactly that.
    UnknownCanonVersion { seq: u64, version: String },
}

#[derive(Debug, Default)]
pub struct ChainReport {
    pub events_checked: u64,
    pub seq_from: u64,
    pub seq_to: u64,
    pub violations: Vec<Violation>,
    /// Checkpoints whose signature verified against a trusted public key.
    pub checkpoints_verified: u32,
}

impl ChainReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Verify a contiguous range. Streaming — an evidence pack can be tens of
/// millions of events and must never need to be resident.
///
/// `expected_prev` is the `event_hash` of the event before `events` starts, or
/// `GENESIS` at the head of a chain. Verifying a *subset* against a checkpoint
/// root is what an auditor actually does; see `checkpoint::verify_inclusion`.
pub fn verify_range<I>(_events: I, _expected_prev: Hash32) -> ChainReport
where
    I: IntoIterator<Item = AuditEvent>,
{
    todo!("M1: fold over events, recompute, link-check, collect violations")
}

#[cfg(test)]
mod tests {
    // M1 done-when (mvp-plan §5): a deliberately corrupted event inside a
    // 100k-event fixture is caught, and the report names its seq.
    //
    // Also: reordering two events must produce a violation, since seq and
    // prev_hash are both hashed. If it doesn't, the chain rule is wrong.
}
