//! Signing keys and their windows.
//!
//! One key is active; every key that was ever active stays exported. Rotation
//! is a new key from a point in time, never a replacement — a checkpoint
//! signed in March must still verify in December, and the verifier is offline,
//! so the only way it can is if the pack carries the key that signed it.

use ancre_chain::CheckpointSigner;
use ancre_types::Timestamp;

use crate::api::KeyDirectory;
use crate::checkpointer::PublicKeyRecord;
use crate::registry::ControlError;

/// The active signing key plus every retired public key.
///
/// ```sql
/// CREATE TABLE signing_keys (
///   key_id     text        PRIMARY KEY,
///   public_key bytea       NOT NULL,
///   -- The private half is NOT here. MVP custody is a control-plane-local
///   -- file under age/SOPS; Enterprise moves it to Vault (mvp-plan §8.3).
///   valid_from timestamptz NOT NULL,
///   valid_to   timestamptz          -- NULL for the active key
/// );
/// ```
///
/// The private key is deliberately absent from that table. A control plane
/// whose database holds both the events' attestation key and the events is a
/// control plane where one compromised credential rewrites history and
/// re-signs it.
pub struct KeyRing {
    active: CheckpointSigner,
    active_from: Timestamp,
    retired: Vec<PublicKeyRecord>,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyRing")
            .field("active", &self.active.key_id())
            .field("retired", &self.retired.len())
            .finish_non_exhaustive()
    }
}

impl KeyRing {
    #[must_use]
    pub fn new(active: CheckpointSigner, active_from: Timestamp) -> Self {
        Self {
            active,
            active_from,
            retired: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_retired(mut self, retired: Vec<PublicKeyRecord>) -> Self {
        self.retired = retired;
        self
    }

    #[must_use]
    pub fn signer(&self) -> &CheckpointSigner {
        &self.active
    }

    /// Retire the active key and take a new one from `at`.
    ///
    /// The windows abut exactly: the old key's `valid_to` is the new key's
    /// `valid_from`. A gap between them would leave checkpoints sealed in the
    /// gap unattributable to any key an auditor was given.
    pub fn rotate(&mut self, next: CheckpointSigner, at: Timestamp) {
        let outgoing = PublicKeyRecord {
            key_id: self.active.key_id().to_string(),
            public_key: hex::encode(self.active.verifying_key()),
            valid_from: self.active_from,
            valid_to: Some(at),
        };
        self.retired.push(outgoing);
        self.active = next;
        self.active_from = at;
    }

    /// Every key, oldest first, the active one last with an open window.
    #[must_use]
    pub fn records(&self) -> Vec<PublicKeyRecord> {
        let mut out = self.retired.clone();
        out.push(PublicKeyRecord {
            key_id: self.active.key_id().to_string(),
            public_key: hex::encode(self.active.verifying_key()),
            valid_from: self.active_from,
            valid_to: None,
        });
        out
    }
}

impl KeyDirectory for KeyRing {
    async fn public_keys(&self) -> Result<Vec<PublicKeyRecord>, ControlError> {
        Ok(self.records())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Timestamp = Timestamp::from_micros(1_754_400_000_000_000);

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_micros(T0.as_micros() + secs * 1_000_000)
    }

    #[test]
    fn a_fresh_ring_exports_one_open_window() {
        let ring = KeyRing::new(CheckpointSigner::from_bytes([1u8; 32], "cp-1".into()), T0);
        let records = ring.records();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key_id, "cp-1");
        assert!(records[0].valid_to.is_none());
    }

    /// The whole reason retired keys are exported: a checkpoint signed before
    /// a rotation must still verify after it.
    #[test]
    fn a_checkpoint_signed_before_rotation_still_verifies_from_the_export() {
        let mut ring = KeyRing::new(CheckpointSigner::from_bytes([1u8; 32], "cp-1".into()), T0);
        let leaves = [ancre_canon::hash_bytes(b"one")];
        let cp = ring
            .signer()
            .seal_range("acme", "hr-screening", 1, &leaves)
            .unwrap();

        ring.rotate(
            CheckpointSigner::from_bytes([2u8; 32], "cp-2".into()),
            at(60),
        );

        let record = ring
            .records()
            .into_iter()
            .find(|r| r.key_id == cp.key_id)
            .expect("the key that signed it must still be exported");
        let mut key = [0u8; 32];
        hex::decode_to_slice(&record.public_key, &mut key).unwrap();

        assert!(ancre_chain::verify_checkpoint(&cp, &key).is_ok());
    }

    #[test]
    fn rotation_leaves_no_gap_between_windows() {
        let mut ring = KeyRing::new(CheckpointSigner::from_bytes([1u8; 32], "cp-1".into()), T0);
        ring.rotate(
            CheckpointSigner::from_bytes([2u8; 32], "cp-2".into()),
            at(60),
        );
        ring.rotate(
            CheckpointSigner::from_bytes([3u8; 32], "cp-3".into()),
            at(120),
        );

        let records = ring.records();
        assert_eq!(records.len(), 3);
        for pair in records.windows(2) {
            assert_eq!(
                pair[0].valid_to,
                Some(pair[1].valid_from),
                "a gap between key windows leaves checkpoints unattributable"
            );
        }
        assert!(records.last().unwrap().valid_to.is_none());
    }

    #[test]
    fn the_private_key_is_never_in_the_export() {
        let ring = KeyRing::new(CheckpointSigner::from_bytes([9u8; 32], "cp-1".into()), T0);
        let json = serde_json::to_string(&ring.records()).unwrap();
        assert!(!json.contains(&hex::encode([9u8; 32])));
        assert!(
            format!("{ring:?}").len() < 200,
            "Debug must not dump key material"
        );
    }
}
