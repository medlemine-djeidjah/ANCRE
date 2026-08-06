//! Hash primitives, resolved at compile time.
//!
//! PRD §10: BLAKE3 by default, SHA-256 behind a feature for customers whose
//! crypto policy demands it. The abstraction exists now because retrofitting
//! one through a chain crate later is miserable (mvp-plan §8.2).

use serde::{Deserialize, Serialize};

/// A 32-byte digest.
///
/// A newtype rather than a bare `[u8; 32]` for three reasons, all of which
/// matter more than the wrapper costs:
///
/// - it cannot be confused with any other 32-byte array (an API key hash and
///   an event hash are both `[u8; 32]` and must never be interchangeable)
/// - it encodes as a CBOR **byte string**, not as an array of 32 integers,
///   which is what serde's blanket array impl would give
/// - `Debug` prints hex, so a digest in a log line is one an auditor can
///   compare against a report by eye
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash32([u8; 32]);

/// `prev_hash` for the first event of a chain. Never a legitimate digest.
pub const GENESIS: Hash32 = Hash32([0u8; 32]);

impl Hash32 {
    #[must_use]
    pub const fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn to_hex(self) -> String {
        crate::hex::encode(&self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, HexError> {
        let mut out = [0u8; 32];
        crate::hex::decode_into(s, &mut out).map_err(|()| HexError)?;
        Ok(Self(out))
    }

    #[must_use]
    pub fn is_genesis(self) -> bool {
        self == GENESIS
    }
}

#[derive(Debug)]
pub struct HexError;

impl std::fmt::Display for HexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not 64 hex characters")
    }
}

impl std::error::Error for HexError {}

impl std::fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::hex::encode(&self.0))
    }
}

impl std::fmt::Display for Hash32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::hex::encode(&self.0))
    }
}

// Two representations, chosen by the format, not by the caller:
//
// - CBOR (the canonical, hashed form): a byte string. Compact, and one
//   encoding per value.
// - JSON (fixtures, evidence packs, anything a human reads): lowercase hex.
//   An auditor comparing a hash in a report against a hash in a file should be
//   able to do it by eye, not by decoding an array of 32 integers.
//
// `is_human_readable()` is how serde exposes that distinction, and both sides
// must agree on it or a JSON round trip silently fails.
impl Serialize for Hash32 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&crate::hex::encode(&self.0))
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Hash32 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Hash32;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a 32-byte digest, as bytes or 64 hex characters")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Hash32, E> {
                <[u8; 32]>::try_from(v)
                    .map(Hash32)
                    .map_err(|_| E::invalid_length(v.len(), &self))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Hash32, E> {
                Hash32::from_hex(v)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Str(v), &self))
            }
        }
        if d.is_human_readable() {
            d.deserialize_str(V)
        } else {
            d.deserialize_bytes(V)
        }
    }
}

#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> Hash32 {
    let mut h = Hasher::new();
    h.update(bytes);
    h.finalize()
}

/// Incremental hasher over whichever primitive the build selected.
#[derive(Debug, Default, Clone)]
pub struct Hasher {
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

    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        #[cfg(feature = "hash-blake3")]
        {
            self.inner.update(bytes);
        }
        #[cfg(all(feature = "hash-sha256", not(feature = "hash-blake3")))]
        {
            use sha2::Digest;
            self.inner.update(bytes);
        }
        self
    }

    #[must_use]
    pub fn finalize(&self) -> Hash32 {
        #[cfg(feature = "hash-blake3")]
        {
            Hash32(*self.inner.finalize().as_bytes())
        }
        #[cfg(all(feature = "hash-sha256", not(feature = "hash-blake3")))]
        {
            use sha2::Digest;
            Hash32(self.inner.clone().finalize().into())
        }
    }
}

// Domain separation tags. A leaf and an interior node must never hash the same
// way, or an attacker can present an interior node as a leaf and prove
// membership of data that was never in the tree. One byte, and it closes a
// whole class of attack.
const LEAF_TAG: u8 = 0x00;
const NODE_TAG: u8 = 0x01;
const EMPTY_TAG: &[u8] = b"ancre-tree/empty";

/// Merkle tree root over an ordered range of leaf hashes.
///
/// This is the RFC 6962 tree — the one Certificate Transparency uses. Chosen
/// over a running fold because an auditor verifies a *subset* of a range far
/// more often than they replay a whole chain, and a tree gives them an
/// inclusion proof for that subset (mvp-plan §4). Chosen over the naive
/// "duplicate the last node when the count is odd" tree because that
/// construction admits two distinct leaf sets with the same root.
///
/// It is also a better answer in a security questionnaire than anything
/// invented here would be.
#[must_use]
pub fn tree_root(leaves: &[Hash32]) -> Hash32 {
    match leaves {
        [] => hash_bytes(EMPTY_TAG),
        [single] => hash_leaf(*single),
        _ => {
            // Split at the largest power of two strictly less than len.
            let k = largest_pow2_below(leaves.len());
            let (left, right) = leaves.split_at(k);
            hash_node(tree_root(left), tree_root(right))
        }
    }
}

#[must_use]
pub fn hash_leaf(leaf: Hash32) -> Hash32 {
    let mut h = Hasher::new();
    h.update(&[LEAF_TAG]);
    h.update(leaf.as_bytes());
    h.finalize()
}

#[must_use]
pub fn hash_node(left: Hash32, right: Hash32) -> Hash32 {
    let mut h = Hasher::new();
    h.update(&[NODE_TAG]);
    h.update(left.as_bytes());
    h.update(right.as_bytes());
    h.finalize()
}

/// Largest power of two strictly less than `n`. `n` must be >= 2.
fn largest_pow2_below(n: usize) -> usize {
    debug_assert!(n >= 2);
    1usize << (usize::BITS - 1 - (n - 1).leading_zeros())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_point_is_largest_power_of_two_below() {
        assert_eq!(largest_pow2_below(2), 1);
        assert_eq!(largest_pow2_below(3), 2);
        assert_eq!(largest_pow2_below(4), 2);
        assert_eq!(largest_pow2_below(5), 4);
        assert_eq!(largest_pow2_below(8), 4);
        assert_eq!(largest_pow2_below(9), 8);
        assert_eq!(largest_pow2_below(1000), 512);
    }

    fn leaf(n: u8) -> Hash32 {
        Hash32([n; 32])
    }

    #[test]
    fn tree_root_is_stable_for_a_given_leaf_sequence() {
        let leaves: Vec<_> = (0..37).map(leaf).collect();
        assert_eq!(tree_root(&leaves), tree_root(&leaves));
    }

    #[test]
    fn tree_root_changes_when_a_leaf_changes() {
        let a: Vec<_> = (0..37).map(leaf).collect();
        let mut b = a.clone();
        b[19] = leaf(200);
        assert_ne!(tree_root(&a), tree_root(&b));
    }

    #[test]
    fn tree_root_changes_when_leaves_are_reordered() {
        let a: Vec<_> = (0..37).map(leaf).collect();
        let mut b = a.clone();
        b.swap(3, 30);
        assert_ne!(tree_root(&a), tree_root(&b));
    }

    /// The failure mode the RFC 6962 split exists to prevent: under a
    /// "duplicate the last leaf when odd" tree, `[a, b, c]` and `[a, b, c, c]`
    /// produce the same root, so an auditor cannot tell those two ranges apart.
    #[test]
    fn duplicated_tail_leaf_is_a_distinct_tree() {
        let three = vec![leaf(1), leaf(2), leaf(3)];
        let four = vec![leaf(1), leaf(2), leaf(3), leaf(3)];
        assert_ne!(tree_root(&three), tree_root(&four));
    }

    #[test]
    fn leaf_and_node_hashes_are_domain_separated() {
        // Without the tags, a single-leaf tree over H and the raw H itself
        // would be indistinguishable.
        assert_ne!(tree_root(&[leaf(7)]), leaf(7));
    }

    #[test]
    fn empty_range_has_a_defined_root() {
        assert_eq!(tree_root(&[]), hash_bytes(EMPTY_TAG));
        assert_ne!(tree_root(&[]), GENESIS);
    }

    #[test]
    fn hash32_hex_round_trips() {
        let h = hash_bytes(b"ancre");
        assert_eq!(Hash32::from_hex(&h.to_hex()).unwrap(), h);
        assert!(Hash32::from_hex("nope").is_err());
    }
}
