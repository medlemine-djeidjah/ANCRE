//! Signed checkpoints.
//!
//! Every N = 10 000 events or T = 5 minutes, the control plane signs a range
//! and writes it to Postgres (mvp-plan §4).

use ancre_canon::Hash32;
use serde::{Deserialize, Serialize};

use crate::ChainError;

pub type VerifyingKeyBytes = [u8; 32];

/// An ed25519 signature.
///
/// A newtype rather than a bare `[u8; 64]` because serde only implements its
/// traits for arrays up to length 32. Hand-rolled here instead of pulling in
/// `serde-big-array`: this type sits in the trust path, and the verifier's
/// dependency list is something an auditor is invited to read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SignatureBytes(pub [u8; 64]);

impl std::fmt::Debug for SignatureBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignatureBytes({})", hex::encode(self.0))
    }
}

// Same split as `Hash32`: bytes in CBOR, hex in JSON. A signature in an
// evidence pack is something an auditor may need to copy.
impl Serialize for SignatureBytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&hex::encode(self.0))
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for SignatureBytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = SignatureBytes;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("64 bytes of ed25519 signature")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                <[u8; 64]>::try_from(v)
                    .map(SignatureBytes)
                    .map_err(|_| E::invalid_length(v.len(), &self))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let mut out = [0u8; 64];
                hex::decode_to_slice(v, &mut out)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Str(v), &self))?;
                Ok(SignatureBytes(out))
            }
        }
        if d.is_human_readable() {
            d.deserialize_str(V)
        } else {
            d.deserialize_bytes(V)
        }
    }
}

/// The signed body. Canonically encoded before signing, so the signature is
/// over bytes a verifier can reproduce, not over a struct layout.
///
/// `built_at` is a `Timestamp` (integer microseconds) for the same reason the
/// event timestamps are: this value is signed, and a `time` crate upgrade that
/// changed its serde representation would invalidate every checkpoint ever
/// issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointBody {
    pub tenant_id: String,
    pub system_id: String,
    pub seq_from: u64,
    pub seq_to: u64,
    /// Tree hash over the range, **not** the last `event_hash`. A tree lets a
    /// verifier check a subset without replaying the whole chain, which is
    /// what an auditor actually does (mvp-plan §4).
    pub root_hash: Hash32,
    pub built_at: ancre_types::Timestamp,
    pub canon_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub body: CheckpointBody,
    pub signature: SignatureBytes,
    /// Which key signed. Key rotation must not invalidate old checkpoints, so
    /// the pack ships every public key that was ever valid, with its window.
    pub key_id: String,
}

/// Holds the private key. Lives only in the control plane.
///
/// MVP custody: a control-plane-local file under age/SOPS. Document that
/// Enterprise moves it to Vault, because the first security questionnaire will
/// ask and "it's a file on disk" is a worse answer if it arrives unprompted
/// (mvp-plan §8.3).
#[derive(Debug)]
pub struct CheckpointSigner {
    _private: (),
}

impl CheckpointSigner {
    pub fn from_pkcs8_pem(_pem: &str, _key_id: String) -> Result<Self, ChainError> {
        todo!("M4: ed25519_dalek::SigningKey from PKCS#8")
    }

    pub fn sign(&self, _body: CheckpointBody) -> Result<Checkpoint, ChainError> {
        todo!("M4: canonical encode body, sign, wrap")
    }

    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKeyBytes {
        todo!("M4")
    }
}

/// Offline. No network, no key server — the pack carries the public keys and
/// the auditor is told, out of band, which fingerprint to trust.
pub fn verify_checkpoint(_cp: &Checkpoint, _key: &VerifyingKeyBytes) -> Result<(), ChainError> {
    todo!("M4: re-encode body canonically, ed25519 verify")
}

/// Verify that a subset of events is included in a checkpoint's `root_hash`
/// without replaying the full range.
///
/// TODO(M4): the proof format is a deliverable of the evidence pack, so design
/// it with `ancre-verify` and write it down in `docs/mapping-table.md`. An
/// inclusion proof nobody but us can parse is not evidence.
pub fn verify_inclusion(
    _cp: &Checkpoint,
    _leaves: &[(u64, Hash32)],
    _proof: &[Hash32],
) -> Result<(), ChainError> {
    todo!("M4")
}
