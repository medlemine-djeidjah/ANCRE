//! Sealed chain fixtures, behind the `fixtures` feature.
//!
//! `ancre-types` builds unsealed events; sealing needs the chain rule, so the
//! chain-shaped helpers live here.

use ancre_canon::{GENESIS, Hash32};
use ancre_types::AuditEvent;

use crate::seal;

/// A valid chain of `n` events, seq 1..=n, anchored at `GENESIS`.
#[must_use]
pub fn chain(n: u64) -> Vec<AuditEvent> {
    chain_from(1, n, GENESIS)
}

/// A valid chain segment, for testing chunked verification.
#[must_use]
pub fn chain_from(first_seq: u64, count: u64, anchor: Hash32) -> Vec<AuditEvent> {
    let mut out = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    let mut prev = anchor;
    for seq in first_seq..first_seq + count {
        let mut e = ancre_types::fixtures::event(seq, prev);
        seal(&mut e).expect("fixture events must always encode");
        prev = e.event_hash;
        out.push(e);
    }
    out
}
