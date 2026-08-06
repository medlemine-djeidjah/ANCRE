//! Deterministic CBOR encoding and content hashing — the Ancre trust root.
//!
//! Two nodes that hold the same logical value must produce byte-identical
//! encodings, forever, across versions and architectures. If that ever fails,
//! every chain Ancre has written is unverifiable and the product is unsound.
//! See `version-pin-resolver-spec.md` §10 test 6.
//!
//! Rules enforced here:
//! - map keys sorted per RFC 8949 deterministic encoding
//! - definite-length arrays and maps only
//! - minimal-width integers
//! - **no floats** — IEEE-754 has multiple bit patterns for the same value and
//!   no defensible canonical form. Encode fixed-point or a string instead.

use serde::{Serialize, de::DeserializeOwned};

pub mod hash;

pub use hash::{Hash32, Hasher, hash_bytes};

/// Identifies the encoding + hash rule set an event was sealed under.
///
/// This string is hashed into every event. It can never change silently: a new
/// value means a new rule set, and verifiers dispatch on it. See mvp-plan §8.1.
#[cfg(feature = "hash-blake3")]
pub const CANON_VERSION: &str = "ancre-canon/1";

#[cfg(all(feature = "hash-sha256", not(feature = "hash-blake3")))]
pub const CANON_VERSION: &str = "ancre-canon/1-sha256";

#[derive(Debug)]
pub enum CanonError {
    /// A float appeared anywhere in the value graph. Not encodable.
    FloatNotAllowed,
    Encode(String),
    Decode(String),
    /// Decoded successfully, but re-encoding produced different bytes. The
    /// input was non-canonical — reject it rather than silently normalising,
    /// or an attacker picks which of two encodings a verifier sees.
    NotCanonical,
    UnknownCanonVersion(String),
}

impl std::fmt::Display for CanonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FloatNotAllowed => write!(f, "floats are not canonically encodable"),
            Self::Encode(m) => write!(f, "encode failed: {m}"),
            Self::Decode(m) => write!(f, "decode failed: {m}"),
            Self::NotCanonical => write!(f, "input was not canonically encoded"),
            Self::UnknownCanonVersion(v) => write!(f, "unknown canon_version: {v}"),
        }
    }
}

impl std::error::Error for CanonError {}

/// Encode a value to canonical CBOR.
///
/// TODO(M1): reject floats before handing off to ciborium — walk the
/// `ciborium::Value` graph, or gate it at the type level so `f32`/`f64` cannot
/// reach here at all. The type-level version is better and is the one to build.
pub fn encode<T: Serialize>(_value: &T) -> Result<Vec<u8>, CanonError> {
    todo!("M1: ciborium::value::canonical_into_vec + float rejection")
}

/// Decode canonical CBOR, rejecting any non-canonical input.
pub fn decode<T: DeserializeOwned>(_bytes: &[u8]) -> Result<T, CanonError> {
    todo!("M1: ciborium::de::from_reader, then re-encode and compare bytes")
}

/// Encode then hash, in one pass. The only path any caller outside this crate
/// should use to derive a content hash.
pub fn content_hash<T: Serialize>(value: &T) -> Result<Hash32, CanonError> {
    Ok(hash_bytes(&encode(value)?))
}

#[cfg(test)]
mod tests {
    // The tests that must exist before this crate is trusted (mvp-plan §5, M1):
    //
    // - roundtrip:      decode(encode(x)) == x
    // - stability:      encode(x) is byte-identical across 10_000 runs built
    //                   from shuffled input orderings
    // - determinism:    two independently-constructed snapshots of the same
    //                   logical config produce identical content_hash
    //                   (resolver spec test 6 — write this one first)
    // - float rejection: any f64 anywhere in the graph is an error, not a NaN
    // - non-canonical:  hand-crafted CBOR with unsorted map keys is rejected
    //
    // Property tests via proptest. These are cheap and they are the only thing
    // standing between the product and a silently unsound evidence chain.
}
