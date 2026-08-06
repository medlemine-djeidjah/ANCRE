//! Signed checkpoints.
//!
//! Every N = 10 000 events or T = 5 minutes, the control plane signs a range
//! and writes it to Postgres (mvp-plan §4).
//!
//! A chain proves internal consistency: nobody edited event 41 207 without
//! breaking the link to 41 208. It does **not** prove when the chain was
//! written, and a party who controls the whole store could rewrite every event
//! and every link together. The signature is what closes that — it commits to
//! a range at a point in time, under a key the store operator does not hold.
//!
//! Still tamper-*evident*, never tamper-proof (PRD §5).

use ancre_canon::{Hash32, root_from_proof, tree_proof, tree_root};
use ancre_types::Timestamp;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointBody {
    pub tenant_id: String,
    pub system_id: String,
    pub seq_from: u64,
    pub seq_to: u64,
    /// Tree hash over the range, **not** the last `event_hash`. A tree lets a
    /// verifier check a subset without replaying the whole chain, which is
    /// what an auditor actually does (mvp-plan §4).
    pub root_hash: Hash32,
    pub built_at: Timestamp,
    pub canon_version: String,
}

impl CheckpointBody {
    /// Number of leaves the root covers. Needed to verify an inclusion proof,
    /// and derived rather than stored so it cannot disagree with the range.
    #[must_use]
    pub fn tree_size(&self) -> usize {
        usize::try_from(self.seq_to.saturating_sub(self.seq_from) + 1).unwrap_or(usize::MAX)
    }

    /// Position of `seq` within the range, if it falls inside.
    #[must_use]
    pub fn index_of(&self, seq: u64) -> Option<usize> {
        (seq >= self.seq_from && seq <= self.seq_to)
            .then(|| usize::try_from(seq - self.seq_from).unwrap_or(usize::MAX))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub body: CheckpointBody,
    pub signature: SignatureBytes,
    /// Which key signed. Key rotation must not invalidate old checkpoints, so
    /// the pack ships every public key that was ever valid, with its window.
    pub key_id: String,
}

/// Domain tag, so a checkpoint signature can never be replayed as a signature
/// over anything else this system signs later.
const CHECKPOINT_TAG: &[u8] = b"ancre-checkpoint/1";

fn signing_payload(body: &CheckpointBody) -> Result<Vec<u8>, ChainError> {
    let encoded = ancre_canon::encode(body).map_err(|e| ChainError::Canon(e.to_string()))?;
    let mut payload = Vec::with_capacity(CHECKPOINT_TAG.len() + encoded.len() + 8);
    let mut h = ancre_canon::Hasher::new();
    crate::absorb(&mut h, CHECKPOINT_TAG);
    crate::absorb(&mut h, &encoded);
    payload.extend_from_slice(h.finalize().as_bytes());
    Ok(payload)
}

/// Holds the private key. Lives only in the control plane.
///
/// MVP custody: a control-plane-local file under age/SOPS. Document that
/// Enterprise moves it to Vault, because the first security questionnaire will
/// ask and "it's a file on disk" is a worse answer if it arrives unprompted
/// (mvp-plan §8.3).
pub struct CheckpointSigner {
    key: SigningKey,
    key_id: String,
}

// The missing field is the whole point: `key` must never be printed, not even
// truncated. `finish_non_exhaustive` says so in the output.
impl std::fmt::Debug for CheckpointSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointSigner")
            .field("key_id", &self.key_id)
            .field("public_key", &hex::encode(self.verifying_key()))
            .finish_non_exhaustive()
    }
}

impl CheckpointSigner {
    #[must_use]
    pub fn from_bytes(secret: [u8; 32], key_id: String) -> Self {
        Self {
            key: SigningKey::from_bytes(&secret),
            key_id,
        }
    }

    /// Generate a fresh key. The control plane does this once, at install.
    #[must_use]
    pub fn generate(key_id: String) -> Self {
        use rand::RngCore;
        let mut secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        Self::from_bytes(secret, key_id)
    }

    pub fn sign(&self, body: CheckpointBody) -> Result<Checkpoint, ChainError> {
        let payload = signing_payload(&body)?;
        Ok(Checkpoint {
            signature: SignatureBytes(self.key.sign(&payload).to_bytes()),
            key_id: self.key_id.clone(),
            body,
        })
    }

    /// Sign a range of event hashes: computes the tree root, then signs.
    pub fn seal_range(
        &self,
        tenant_id: &str,
        system_id: &str,
        seq_from: u64,
        leaves: &[Hash32],
    ) -> Result<Checkpoint, ChainError> {
        if leaves.is_empty() {
            return Err(ChainError::EmptyRange);
        }
        let body = CheckpointBody {
            tenant_id: tenant_id.to_string(),
            system_id: system_id.to_string(),
            seq_from,
            seq_to: seq_from + leaves.len() as u64 - 1,
            root_hash: tree_root(leaves),
            built_at: Timestamp::now(),
            canon_version: ancre_canon::CANON_VERSION.to_string(),
        };
        self.sign(body)
    }

    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKeyBytes {
        self.key.verifying_key().to_bytes()
    }

    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

/// Offline. No network, no key server — the pack carries the public keys and
/// the auditor is told, out of band, which fingerprint to trust.
pub fn verify_checkpoint(cp: &Checkpoint, key: &VerifyingKeyBytes) -> Result<(), ChainError> {
    let vk = VerifyingKey::from_bytes(key).map_err(|e| ChainError::BadKey(e.to_string()))?;
    let payload = signing_payload(&cp.body)?;
    vk.verify(&payload, &Signature::from_bytes(&cp.signature.0))
        .map_err(|_| ChainError::BadSignature)
}

/// Prove that one event was in a signed range, without replaying the range.
///
/// `leaves` is the full ordered set of `event_hash` values the checkpoint
/// covers — the party producing the proof has them; the auditor receiving it
/// does not need them.
pub fn prove_inclusion(
    cp: &Checkpoint,
    leaves: &[Hash32],
    seq: u64,
) -> Result<InclusionProof, ChainError> {
    let index = cp.body.index_of(seq).ok_or(ChainError::SeqOutOfRange {
        seq,
        from: cp.body.seq_from,
        to: cp.body.seq_to,
    })?;
    if leaves.len() != cp.body.tree_size() || tree_root(leaves) != cp.body.root_hash {
        return Err(ChainError::LeavesDoNotMatchCheckpoint);
    }
    let path = tree_proof(leaves, index).ok_or(ChainError::LeavesDoNotMatchCheckpoint)?;
    Ok(InclusionProof {
        seq,
        event_hash: leaves[index],
        path,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InclusionProof {
    pub seq: u64,
    pub event_hash: Hash32,
    pub path: Vec<Hash32>,
}

/// Verify that a checkpoint really covers this event.
///
/// Two independent things are checked, and both must hold: the checkpoint's
/// signature (so the range was attested by a key we trust) and the inclusion
/// path (so this event is in that range). Either alone proves nothing useful.
pub fn verify_inclusion(
    cp: &Checkpoint,
    proof: &InclusionProof,
    key: &VerifyingKeyBytes,
) -> Result<(), ChainError> {
    verify_checkpoint(cp, key)?;

    let index = cp
        .body
        .index_of(proof.seq)
        .ok_or(ChainError::SeqOutOfRange {
            seq: proof.seq,
            from: cp.body.seq_from,
            to: cp.body.seq_to,
        })?;

    let computed = root_from_proof(proof.event_hash, index, cp.body.tree_size(), &proof.path)
        .ok_or(ChainError::BadInclusionProof)?;

    (computed == cp.body.root_hash)
        .then_some(())
        .ok_or(ChainError::BadInclusionProof)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: u64) -> Vec<Hash32> {
        (0..n)
            .map(|i| ancre_canon::hash_bytes(format!("event-{i}").as_bytes()))
            .collect()
    }

    fn signer() -> CheckpointSigner {
        CheckpointSigner::from_bytes([7u8; 32], "ancre-cp-2026-08".into())
    }

    #[test]
    fn a_signed_checkpoint_verifies_under_its_own_key() {
        let s = signer();
        let cp = s
            .seal_range("acme", "hr-screening", 1, &leaves(1000))
            .unwrap();

        assert_eq!(cp.body.seq_from, 1);
        assert_eq!(cp.body.seq_to, 1000);
        assert_eq!(cp.key_id, "ancre-cp-2026-08");
        assert!(verify_checkpoint(&cp, &s.verifying_key()).is_ok());
    }

    #[test]
    fn a_checkpoint_does_not_verify_under_a_different_key() {
        let cp = signer().seal_range("acme", "hr", 1, &leaves(10)).unwrap();
        let other = CheckpointSigner::from_bytes([9u8; 32], "other".into());

        assert_eq!(
            verify_checkpoint(&cp, &other.verifying_key()),
            Err(ChainError::BadSignature)
        );
    }

    /// The point of signing: an operator who can rewrite the store cannot
    /// rewrite what was already attested.
    #[test]
    fn any_edit_to_the_signed_body_breaks_the_signature() {
        let s = signer();
        let original = s.seal_range("acme", "hr", 1, &leaves(100)).unwrap();

        let mut edits = Vec::new();

        let mut cp = original.clone();
        cp.body.seq_to = 99;
        edits.push(("shortened the range", cp));

        let mut cp = original.clone();
        cp.body.root_hash = ancre_canon::hash_bytes(b"a different range");
        edits.push(("swapped the root", cp));

        let mut cp = original.clone();
        cp.body.built_at = Timestamp::from_micros(0);
        edits.push(("backdated it", cp));

        let mut cp = original.clone();
        cp.body.tenant_id = "someone-else".into();
        edits.push(("reassigned the tenant", cp));

        let mut cp = original;
        cp.body.canon_version = "ancre-canon/99".into();
        edits.push(("claimed another rule set", cp));

        for (what, cp) in edits {
            assert_eq!(
                verify_checkpoint(&cp, &s.verifying_key()),
                Err(ChainError::BadSignature),
                "{what} must invalidate the signature"
            );
        }
    }

    #[test]
    fn signing_is_deterministic_for_a_given_body() {
        // ed25519 is deterministic, and the payload is canonically encoded, so
        // two control-plane replicas signing the same range produce identical
        // bytes. Anything else and an auditor sees two checkpoints for one
        // range and cannot tell which is authoritative.
        let s = signer();
        let body = CheckpointBody {
            tenant_id: "acme".into(),
            system_id: "hr".into(),
            seq_from: 1,
            seq_to: 100,
            root_hash: tree_root(&leaves(100)),
            built_at: Timestamp::from_micros(1_754_400_000_000_000),
            canon_version: ancre_canon::CANON_VERSION.into(),
        };
        assert_eq!(
            s.sign(body.clone()).unwrap().signature,
            s.sign(body).unwrap().signature
        );
    }

    /// What an auditor actually does: "prove this one decision was in the
    /// signed record", without being handed the whole range.
    #[test]
    fn one_event_can_be_proven_without_replaying_the_range() {
        let s = signer();
        let all = leaves(10_000);
        let cp = s.seal_range("acme", "hr", 1, &all).unwrap();

        let proof = prove_inclusion(&cp, &all, 4_207).unwrap();

        assert_eq!(proof.event_hash, all[4_206], "seq 4207 is index 4206");
        assert!(
            proof.path.len() <= 14,
            "a proof out of 10k events must stay logarithmic, got {}",
            proof.path.len()
        );
        assert!(verify_inclusion(&cp, &proof, &s.verifying_key()).is_ok());
    }

    #[test]
    fn an_event_that_was_never_in_the_range_cannot_be_proven_into_it() {
        let s = signer();
        let all = leaves(64);
        let cp = s.seal_range("acme", "hr", 1, &all).unwrap();
        let mut proof = prove_inclusion(&cp, &all, 10).unwrap();

        // Same position, fabricated event.
        proof.event_hash = ancre_canon::hash_bytes(b"an event that never happened");
        assert_eq!(
            verify_inclusion(&cp, &proof, &s.verifying_key()),
            Err(ChainError::BadInclusionProof)
        );
    }

    #[test]
    fn a_real_event_cannot_be_claimed_at_the_wrong_position() {
        let s = signer();
        let all = leaves(64);
        let cp = s.seal_range("acme", "hr", 1, &all).unwrap();
        let mut proof = prove_inclusion(&cp, &all, 10).unwrap();

        proof.seq = 11;
        assert_eq!(
            verify_inclusion(&cp, &proof, &s.verifying_key()),
            Err(ChainError::BadInclusionProof)
        );
    }

    #[test]
    fn an_inclusion_proof_under_an_untrusted_key_is_refused_before_the_path_is_checked() {
        let s = signer();
        let all = leaves(64);
        let cp = s.seal_range("acme", "hr", 1, &all).unwrap();
        let proof = prove_inclusion(&cp, &all, 10).unwrap();
        let other = CheckpointSigner::from_bytes([3u8; 32], "other".into());

        // A valid path under an unsigned checkpoint proves nothing.
        assert_eq!(
            verify_inclusion(&cp, &proof, &other.verifying_key()),
            Err(ChainError::BadSignature)
        );
    }

    #[test]
    fn a_seq_outside_the_range_has_no_proof() {
        let s = signer();
        let all = leaves(10);
        let cp = s.seal_range("acme", "hr", 100, &all).unwrap();

        assert!(matches!(
            prove_inclusion(&cp, &all, 5),
            Err(ChainError::SeqOutOfRange { .. })
        ));
        assert!(prove_inclusion(&cp, &all, 100).is_ok());
        assert!(prove_inclusion(&cp, &all, 109).is_ok());
    }

    #[test]
    fn leaves_that_do_not_match_the_checkpoint_cannot_produce_a_proof() {
        let s = signer();
        let cp = s.seal_range("acme", "hr", 1, &leaves(64)).unwrap();
        let mut tampered = leaves(64);
        tampered[3] = ancre_canon::hash_bytes(b"edited");

        assert_eq!(
            prove_inclusion(&cp, &tampered, 10),
            Err(ChainError::LeavesDoNotMatchCheckpoint)
        );
    }

    #[test]
    fn an_empty_range_cannot_be_sealed() {
        assert_eq!(
            signer().seal_range("acme", "hr", 1, &[]),
            Err(ChainError::EmptyRange)
        );
    }

    #[test]
    fn a_checkpoint_round_trips_through_json_for_the_evidence_pack() {
        let s = signer();
        let cp = s.seal_range("acme", "hr", 1, &leaves(50)).unwrap();

        let json = serde_json::to_string(&cp).unwrap();
        assert!(json.contains(&hex::encode(cp.signature.0)), "hex in JSON");

        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cp);
        assert!(verify_checkpoint(&back, &s.verifying_key()).is_ok());
    }

    #[test]
    fn a_generated_key_signs_and_verifies() {
        let s = CheckpointSigner::generate("fresh".into());
        let cp = s.seal_range("acme", "hr", 1, &leaves(8)).unwrap();
        assert!(verify_checkpoint(&cp, &s.verifying_key()).is_ok());
    }

    #[test]
    fn debug_never_prints_the_private_key() {
        let s = CheckpointSigner::from_bytes([42u8; 32], "k".into());
        let printed = format!("{s:?}");
        assert!(!printed.contains(&hex::encode([42u8; 32])));
        assert!(printed.contains(&hex::encode(s.verifying_key())));
    }
}
