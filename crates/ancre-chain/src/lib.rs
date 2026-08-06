//! Hash chaining, verification, and signed checkpoints.
//!
//! ClickHouse has `ALTER DELETE`. Append-only is therefore enforced by this
//! crate's hash chain, not by the storage engine — which is the entire reason
//! the chain exists. Say "tamper-evident", never "tamper-proof" (PRD §5).

pub mod checkpoint;
#[cfg(feature = "fixtures")]
pub mod fixtures;
pub mod verify;

use ancre_canon::{Hash32, Hasher};
use ancre_types::AuditEvent;

pub use checkpoint::{
    Checkpoint, CheckpointBody, CheckpointSigner, SignatureBytes, VerifyingKeyBytes,
};
pub use verify::{ChainReport, Violation, verify_range};

/// `chain_id = (tenant_id, system_id)` for the MVP (mvp-plan §8.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChainId {
    pub tenant_id: String,
    pub system_id: String,
}

impl std::fmt::Display for ChainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.tenant_id, self.system_id)
    }
}

/// Domain tag, absorbed first so that an `event_hash` can never collide with a
/// tree node or a leaf hash from `ancre-canon`.
const EVENT_TAG: &[u8] = b"ancre-event/1";

/// Absorb a length-prefixed field.
///
/// The length prefix is the entire point. Absorbing `a ‖ b` raw means
/// `("xy", "z")` and `("x", "yz")` hash identically, so a value could be
/// shifted across a field boundary without changing the digest. A `u32`
/// little-endian prefix per field closes that, and 4 GiB is not a plausible
/// field length in an audit event.
pub fn absorb(h: &mut Hasher, field: &[u8]) {
    let len = u32::try_from(field.len()).unwrap_or(u32::MAX);
    h.update(&len.to_le_bytes());
    h.update(field);
}

/// The chain rule:
///
/// ```text
/// event_hash = HASH( tag ‖ canon_version ‖ prev_hash ‖ canonical_cbor(body) )
/// ```
///
/// with every component length-prefixed. Reordering is detectable because
/// `seq` and `prev_hash` are both inside `body` as well.
pub fn event_hash(event: &AuditEvent) -> Result<Hash32, ChainError> {
    let body = event
        .hashed_body()
        .map_err(|e| ChainError::Canon(e.to_string()))?;

    let mut h = Hasher::new();
    absorb(&mut h, EVENT_TAG);
    absorb(&mut h, event.canon_version.as_bytes());
    absorb(&mut h, event.prev_hash.as_bytes());
    absorb(&mut h, &body);
    Ok(h.finalize())
}

/// Recompute and stamp `event_hash` in place. What the ingester calls after it
/// has allocated `seq` and set `prev_hash`.
pub fn seal(event: &mut AuditEvent) -> Result<Hash32, ChainError> {
    let h = event_hash(event)?;
    event.event_hash = h;
    Ok(h)
}

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("canonical encoding failed: {0}")]
    Canon(String),
    #[error("unknown canon_version {0}: this verifier cannot check that rule set")]
    UnknownCanonVersion(String),
    #[error("signature verification failed")]
    BadSignature,
    #[error("malformed key: {0}")]
    BadKey(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancre_canon::Hasher;

    #[test]
    fn length_prefixing_prevents_field_boundary_shifting() {
        let shifted = |a: &[u8], b: &[u8]| {
            let mut h = Hasher::new();
            absorb(&mut h, a);
            absorb(&mut h, b);
            h.finalize()
        };
        // Raw concatenation would make these two identical.
        assert_ne!(shifted(b"xy", b"z"), shifted(b"x", b"yz"));
    }

    #[test]
    fn empty_fields_are_distinguishable_from_absent_ones() {
        let mut one = Hasher::new();
        absorb(&mut one, b"");
        let two = Hasher::new();
        assert_ne!(one.finalize(), two.finalize());
    }
}
