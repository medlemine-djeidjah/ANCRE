//! Hash chaining, verification, and signed checkpoints.
//!
//! ClickHouse has `ALTER DELETE`. Append-only is therefore enforced by this
//! crate's hash chain, not by the storage engine — which is the entire reason
//! the chain exists. Say "tamper-evident", never "tamper-proof" (PRD §5).

pub mod checkpoint;
pub mod verify;

use ancre_canon::{Hash32, Hasher};
use ancre_types::AuditEvent;

pub use checkpoint::{Checkpoint, CheckpointSigner, VerifyingKeyBytes};
pub use verify::{ChainReport, Violation, verify_range};

/// `chain_id = (tenant_id, system_id)` for the MVP (mvp-plan §8.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainId {
    pub tenant_id: String,
    pub system_id: String,
}

/// The chain rule:
///
/// ```text
/// event_hash = HASH( canon_version ‖ prev_hash ‖ canonical_cbor(hashed_body) )
/// ```
///
/// Concatenation is length-prefixed, not raw — otherwise a `canon_version`
/// ending in hex and a `prev_hash` beginning with it can be shifted between
/// the two fields to produce a collision. Cheap to get right now, invisible
/// and fatal to get wrong.
pub fn event_hash(_event: &AuditEvent) -> Result<Hash32, ChainError> {
    todo!("M1: length-prefixed absorb of canon_version, prev_hash, hashed_body")
}

/// Absorb a length-prefixed field into a hasher. The primitive `event_hash`
/// is built from; separated out so the verifier can reuse it byte-for-byte.
pub fn absorb(_h: &mut Hasher, _field: &[u8]) {
    todo!("M1: u32-LE length prefix, then bytes")
}

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("canonical encoding failed: {0}")]
    Canon(String),
    #[error("unknown canon_version {0}: this verifier cannot check that rule set")]
    UnknownCanonVersion(String),
    #[error("signature verification failed")]
    BadSignature,
}
