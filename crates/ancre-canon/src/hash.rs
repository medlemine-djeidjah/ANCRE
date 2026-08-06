//! Hash abstraction, resolved at compile time.
//!
//! PRD §10: BLAKE3 by default, SHA-256 behind a feature for customers whose
//! crypto policy demands it. Build the abstraction now — retrofitting one
//! through a chain crate later is miserable (mvp-plan §8.2).

pub type Hash32 = [u8; 32];

/// The all-zero hash. Used as `prev_hash` for the genesis event of a chain,
/// and never as a legitimate digest.
pub const GENESIS: Hash32 = [0u8; 32];

#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> Hash32 {
    let mut h = Hasher::new();
    h.update(bytes);
    h.finalize()
}

/// Incremental hasher. Wraps whichever primitive the build selected.
#[derive(Debug, Default)]
pub struct Hasher {
    // Read once `update`/`finalize` are implemented; the allow goes then.
    #[allow(dead_code)]
    #[cfg(feature = "hash-blake3")]
    inner: blake3::Hasher,
    #[cfg(all(feature = "hash-sha256", not(feature = "hash-blake3")))]
    inner: sha2::Sha256,
}

impl Hasher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, _bytes: &[u8]) -> &mut Self {
        todo!("M1: delegate to inner")
    }

    #[must_use]
    pub fn finalize(&self) -> Hash32 {
        todo!("M1: delegate to inner")
    }
}

/// Tree hash over an ordered range of leaf hashes.
///
/// Used for checkpoint `root_hash` (mvp-plan §4). Deliberately a tree and not
/// a running fold: an auditor verifies a *subset* of a range far more often
/// than they replay a whole chain, and a tree gives them an inclusion proof
/// for that subset. A fold does not.
///
/// TODO(M4): the inclusion-proof API is what the verifier CLI actually needs.
/// Design it alongside `ancre-verify`, not before.
#[must_use]
pub fn tree_root(_leaves: &[Hash32]) -> Hash32 {
    todo!("M1: BLAKE3 tree hash, or an explicit binary Merkle tree under SHA-256")
}
