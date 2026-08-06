//! Deterministic CBOR encoding and content hashing — the Ancre trust root.
//!
//! Two nodes that hold the same logical value must produce byte-identical
//! encodings, forever, across versions and architectures. If that ever fails,
//! every chain Ancre has written is unverifiable and the product is unsound.
//! See `version-pin-resolver-spec.md` §10 test 6.
//!
//! Rules enforced here, per RFC 8949 §4.2 deterministic encoding:
//!
//! - map keys sorted by the canonical ordering (length first, then bytewise)
//! - definite-length arrays, maps, strings and byte strings only
//! - minimal-width integers
//! - duplicate map keys rejected
//! - **no floats** — IEEE-754 has multiple bit patterns for the same value and
//!   no defensible canonical form. Encode fixed-point or a string instead.
//!
//! Decoding is strict in the same direction: input that decodes but does not
//! re-encode to the identical bytes is **rejected**, not normalised. Silently
//! accepting a non-canonical encoding would let an attacker choose which of
//! two byte sequences a verifier sees for the same logical event.

use ciborium::Value;
use ciborium::value::CanonicalValue;
use serde::{Serialize, de::DeserializeOwned};

pub mod hash;
mod hex;

pub use hash::{GENESIS, Hash32, Hasher, hash_bytes, hash_leaf, hash_node, tree_root};

/// Borrowed bytes that encode as a CBOR **byte string**.
///
/// serde's blanket impl for `&[u8]` produces an array of integers, which is
/// four times the size and — more importantly — a different encoding for the
/// same logical value depending on how the caller typed it. Anything that goes
/// into a hashed body as raw bytes goes through this.
#[derive(Debug, Clone, Copy)]
pub struct Bytes<'a>(pub &'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(self.0)
    }
}

/// Identifies the encoding + hash rule set an event was sealed under.
///
/// Hashed into every event. It can never change silently: a new value means a
/// new rule set, and verifiers dispatch on it (mvp-plan §8.1).
#[cfg(feature = "hash-blake3")]
pub const CANON_VERSION: &str = "ancre-canon/1";

#[cfg(all(feature = "hash-sha256", not(feature = "hash-blake3")))]
pub const CANON_VERSION: &str = "ancre-canon/1-sha256";

#[derive(Debug, PartialEq, Eq)]
pub enum CanonError {
    /// A float appeared somewhere in the value graph. Not encodable.
    FloatNotAllowed,
    /// The same map key twice. Ambiguous, and a verifier and a writer could
    /// disagree about which one wins.
    DuplicateMapKey,
    Encode(String),
    Decode(String),
    /// Decoded, but re-encoding produced different bytes.
    NotCanonical,
    UnknownCanonVersion(String),
}

impl std::fmt::Display for CanonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FloatNotAllowed => write!(f, "floats are not canonically encodable"),
            Self::DuplicateMapKey => write!(f, "duplicate map key"),
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
/// Goes via `ciborium::Value` so the whole graph can be inspected before a
/// byte is written. That costs an extra allocation pass, which is affordable
/// here: this runs in the ingester, never on the request path. If it ever does
/// need to be fast, the right fix is a `Serializer` wrapper that rejects
/// floats at the type level — not loosening the check.
pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CanonError> {
    let v = Value::serialized(value).map_err(|e| CanonError::Encode(e.to_string()))?;
    write_canonical(&canonicalize(v)?)
}

/// Decode canonical CBOR, rejecting anything that was not canonically encoded.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CanonError> {
    let v: Value = ciborium::from_reader(bytes).map_err(|e| CanonError::Decode(e.to_string()))?;
    let v = canonicalize(v)?;
    if write_canonical(&v)? != bytes {
        return Err(CanonError::NotCanonical);
    }
    v.deserialized()
        .map_err(|e| CanonError::Decode(e.to_string()))
}

/// Encode then hash, in one pass. The only path any caller outside this crate
/// should use to derive a content hash.
pub fn content_hash<T: Serialize + ?Sized>(value: &T) -> Result<Hash32, CanonError> {
    Ok(hash_bytes(&encode(value)?))
}

fn write_canonical(v: &Value) -> Result<Vec<u8>, CanonError> {
    let mut buf = Vec::new();
    ciborium::into_writer(v, &mut buf).map_err(|e| CanonError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Rewrite a value graph into its single permitted form, or refuse.
///
/// Recursive, and deliberately so — depth is bounded by the event schema,
/// which is frozen and shallow. If a future schema ever nests unboundedly,
/// this needs an explicit depth limit before it ships, because a hostile
/// payload could otherwise blow the stack inside the ingester.
fn canonicalize(v: Value) -> Result<Value, CanonError> {
    match v {
        Value::Float(_) => Err(CanonError::FloatNotAllowed),

        Value::Map(entries) => {
            let mut out: Vec<(CanonicalValue, Value)> = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                out.push((canonicalize(k)?.into(), canonicalize(val)?));
            }
            // RFC 8949 §4.2.1: shorter encoded key first, then bytewise.
            // `CanonicalValue`'s `Ord` is ciborium's implementation of exactly
            // that ordering — borrowed rather than reimplemented, because a
            // subtly wrong comparator here would produce an encoder that is
            // self-consistent and disagrees with every other CBOR library.
            out.sort_by(|a, b| a.0.cmp(&b.0));
            if out.windows(2).any(|w| w[0].0 == w[1].0) {
                return Err(CanonError::DuplicateMapKey);
            }
            Ok(Value::Map(
                out.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            ))
        }

        Value::Array(items) => items
            .into_iter()
            .map(canonicalize)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),

        Value::Tag(tag, inner) => Ok(Value::Tag(tag, Box::new(canonicalize(*inner)?))),

        // Integers, text, bytes, bool and null have one encoding each once
        // ciborium's minimal-width integer rule is applied on the way out.
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::{BTreeMap, HashMap};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Pins {
        model_version: String,
        prompt_version: String,
        generation: u64,
        risk_flags: Vec<String>,
    }

    fn pins() -> Pins {
        Pins {
            model_version: "gpt-4o-2024-08-06".into(),
            prompt_version: "b3:9f2c1a".into(),
            generation: 41,
            risk_flags: vec!["stale_config".into()],
        }
    }

    #[test]
    fn round_trips() {
        let bytes = encode(&pins()).unwrap();
        assert_eq!(decode::<Pins>(&bytes).unwrap(), pins());
    }

    /// Resolver spec §10 test 6, in its smallest form: two independently
    /// built values that are logically equal encode to identical bytes.
    ///
    /// This is the test the whole product rests on. Non-deterministic
    /// canonical encoding stays invisible until an auditor cannot verify a
    /// chain, and by then every chain ever written is suspect.
    #[test]
    fn map_insertion_order_does_not_affect_the_encoding() {
        let mut a: HashMap<&str, u32> = HashMap::new();
        for (k, v) in [("zebra", 1), ("alpha", 2), ("beta", 3), ("gamma", 4)] {
            a.insert(k, v);
        }
        let mut b: HashMap<&str, u32> = HashMap::new();
        for (k, v) in [("gamma", 4), ("beta", 3), ("zebra", 1), ("alpha", 2)] {
            b.insert(k, v);
        }
        assert_eq!(encode(&a).unwrap(), encode(&b).unwrap());
        assert_eq!(content_hash(&a).unwrap(), content_hash(&b).unwrap());
    }

    #[test]
    fn encoding_is_stable_across_many_shuffled_builds() {
        // The HashMap iteration order is randomised per instance, so building
        // the same logical map 1000 times exercises 1000 different orderings.
        let expected = {
            let m: BTreeMap<&str, u64> = (0..64).map(|i| (KEYS[i], i as u64)).collect();
            encode(&m).unwrap()
        };
        for _ in 0..1000 {
            let m: HashMap<&str, u64> = (0..64).map(|i| (KEYS[i], i as u64)).collect();
            assert_eq!(encode(&m).unwrap(), expected);
        }
    }

    const KEYS: [&str; 64] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "aa", "bb",
        "cc", "dd", "ee", "ff", "gg", "hh", "ii", "jj", "kk", "ll", "mm", "nn", "oo", "pp", "aaa",
        "bbb", "ccc", "ddd", "eee", "fff", "ggg", "hhh", "iii", "jjj", "kkk", "lll", "mmm", "nnn",
        "ooo", "ppp", "aaaa", "bbbb", "cccc", "dddd", "eeee", "ffff", "gggg", "hhhh", "iiii",
        "jjjj", "kkkk", "llll", "mmmm", "nnnn", "oooo", "pppp",
    ];

    #[test]
    fn keys_sort_by_encoded_length_before_bytes() {
        // RFC 8949's ordering is *not* plain lexicographic: "z" (one byte)
        // sorts before "aa" (two bytes). Getting this backwards would produce
        // a self-consistent implementation that disagrees with every other
        // CBOR library — the worst possible failure, because our own tests
        // would pass.
        let m: BTreeMap<&str, u8> = [("aa", 1), ("z", 2)].into_iter().collect();
        let bytes = encode(&m).unwrap();
        let decoded: Value = ciborium::from_reader(&bytes[..]).unwrap();
        let Value::Map(entries) = decoded else {
            panic!("expected a map")
        };
        assert_eq!(entries[0].0.as_text(), Some("z"));
        assert_eq!(entries[1].0.as_text(), Some("aa"));
    }

    #[test]
    fn floats_are_rejected_at_every_depth() {
        assert_eq!(encode(&1.5f64).unwrap_err(), CanonError::FloatNotAllowed);
        assert_eq!(
            encode(&vec![vec![1.5f32]]).unwrap_err(),
            CanonError::FloatNotAllowed
        );
        let m: BTreeMap<&str, f64> = [("latency", 0.5)].into_iter().collect();
        assert_eq!(encode(&m).unwrap_err(), CanonError::FloatNotAllowed);
    }

    #[test]
    fn float_nan_does_not_slip_through() {
        // NaN has many bit patterns, all of which compare unequal to
        // themselves. If one ever reached a hash, two verifiers could disagree
        // about the same event.
        assert_eq!(encode(&f64::NAN).unwrap_err(), CanonError::FloatNotAllowed);
    }

    #[test]
    fn non_canonical_input_is_rejected_rather_than_normalised() {
        // A map with keys in the wrong order: valid CBOR, decodes fine, and
        // must still be refused.
        let wrong = Value::Map(vec![
            (Value::Text("aa".into()), Value::Integer(1.into())),
            (Value::Text("z".into()), Value::Integer(2.into())),
        ]);
        let mut bytes = Vec::new();
        ciborium::into_writer(&wrong, &mut bytes).unwrap();

        assert_eq!(
            decode::<BTreeMap<String, u8>>(&bytes).unwrap_err(),
            CanonError::NotCanonical
        );
    }

    #[test]
    fn indefinite_length_input_is_rejected() {
        // 0x9f = indefinite-length array, 0x01 0x02, 0xff = break.
        let bytes = [0x9f, 0x01, 0x02, 0xff];
        assert_eq!(
            decode::<Vec<u8>>(&bytes).unwrap_err(),
            CanonError::NotCanonical
        );
    }

    #[test]
    fn duplicate_map_keys_are_rejected() {
        let dupe = Value::Map(vec![
            (Value::Text("k".into()), Value::Integer(1.into())),
            (Value::Text("k".into()), Value::Integer(2.into())),
        ]);
        let mut bytes = Vec::new();
        ciborium::into_writer(&dupe, &mut bytes).unwrap();
        assert_eq!(
            decode::<BTreeMap<String, u8>>(&bytes).unwrap_err(),
            CanonError::DuplicateMapKey
        );
    }

    #[test]
    fn integers_use_minimal_width() {
        // 1 encodes as one byte, not as a padded u64. Anything else and two
        // implementations that agree on the value disagree on the bytes.
        assert_eq!(encode(&1u64).unwrap(), vec![0x01]);
        assert_eq!(encode(&1u8).unwrap(), vec![0x01]);
        assert_eq!(encode(&1i32).unwrap(), vec![0x01]);
    }

    #[test]
    fn a_changed_field_changes_the_hash() {
        let mut other = pins();
        other.generation = 42;
        assert_ne!(
            content_hash(&pins()).unwrap(),
            content_hash(&other).unwrap()
        );
    }

    #[test]
    fn hash32_encodes_as_a_byte_string_not_an_array() {
        // 0x58 0x20 = byte string, length 32. If this ever becomes 0x98 0x20
        // (array of 32), the encoding has silently changed and every stored
        // hash is unverifiable.
        let bytes = encode(&hash_bytes(b"x")).unwrap();
        assert_eq!(&bytes[..2], &[0x58, 0x20]);
        assert_eq!(bytes.len(), 34);
    }
}
