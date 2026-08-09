//! Evidence packs: everything needed to check a chain, in one directory.
//!
//! A pack is four files and no cleverness:
//!
//! ```text
//! <pack>/
//!   manifest.json      what this is, which chain, which range, who produced it
//!   events.jsonl       the events, one JSON object per line
//!   checkpoints.json   ed25519 signatures over range roots
//!   pubkeys.json       every key that was ever active, with its window
//! ```
//!
//! ## What binds the files together
//!
//! Not the manifest. It is descriptive metadata and it is not signed, so
//! hashing the other three files into it would buy nothing — anyone able to
//! edit `events.jsonl` can edit `manifest.json` in the same breath, and a
//! checksum that only detects accidents while looking like it detects attacks
//! is worse than no checksum.
//!
//! What binds them is the thing that was already there: **a checkpoint is an
//! ed25519 signature over a tree root**. Recompute the root over the events in
//! its range, check the signature with a key whose fingerprint the auditor
//! obtained from somewhere other than this pack, and the events in that range
//! are attested. Everything else in here is commentary.
//!
//! ## The circularity, stated rather than hidden
//!
//! `pubkeys.json` arrives inside the pack, from the same party that produced
//! the pack. Verifying the pack's signatures against the pack's own keys
//! proves internal consistency and **nothing about provenance** — someone who
//! forged the whole thing would forge a matching key. That is why the key
//! fingerprints are printed prominently, and why `--key` exists: an auditor
//! who was given a fingerprint out of band can pin it, and then a forged pack
//! fails.
//!
//! Saying so in the output is the difference between evidence and theatre.

use std::collections::BTreeMap;
use std::path::Path;

use ancre_canon::{Hash32, tree_root};
use ancre_chain::{Checkpoint, verify_checkpoint};
use ancre_types::{AuditEvent, Timestamp};
use serde::Deserialize;

/// The only pack layout this build understands.
///
/// A pack naming anything else is refused with "cannot verify" rather than
/// read on a best-effort basis. A future layout could move a field this build
/// silently ignores, and a verifier that shrugged at that would produce a
/// confident verdict about a document it did not fully read.
pub(crate) const PACK_VERSION: &str = "ancre-pack/1";

#[derive(Debug, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) pack_version: String,
    pub(crate) tenant_id: String,
    pub(crate) system_id: String,
    /// Everything below is descriptive. Missing fields are not an error: they
    /// cost a line of context in the report, never a verdict.
    #[serde(default)]
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) produced_by: String,
    #[serde(default)]
    pub(crate) source: String,
    #[serde(default)]
    pub(crate) seq_from: Option<u64>,
    #[serde(default)]
    pub(crate) seq_to: Option<u64>,
    /// The hash of the event immediately before this range, for a pack that is
    /// not a whole chain.
    ///
    /// A partial export cannot verify without an anchor — its first event
    /// points back at something the pack does not contain — and requiring
    /// every auditor to be told a 64-character hex string out of band would
    /// make quarterly ranges unusable in practice.
    ///
    /// This value is **not signed**, so it is used only as a default and the
    /// report says it was. It is not a hole: an attacker who re-anchors a
    /// forged prefix still has to produce checkpoint signatures over it, and
    /// those are the thing that cannot be forged. `--from` overrides it for an
    /// auditor who has the real anchor.
    #[serde(default)]
    pub(crate) prev_hash: Option<String>,
}

/// A public key as the control plane exports it. Redeclared here rather than
/// imported: `ancre-verify` may not depend on the control plane, and this is a
/// four-field JSON shape, not a shared abstraction.
#[derive(Debug, Deserialize)]
pub(crate) struct PublicKeyRecord {
    pub(crate) key_id: String,
    /// Hex, because it is copied by hand out of a pack and compared by eye.
    pub(crate) public_key: String,
    pub(crate) valid_from: Timestamp,
    /// `None` while this is the active key.
    #[serde(default)]
    pub(crate) valid_to: Option<Timestamp>,
}

/// What a pack's files were found to contain.
#[derive(Debug)]
pub(crate) struct Pack {
    pub(crate) manifest: Manifest,
    pub(crate) events: Vec<AuditEvent>,
    pub(crate) checkpoints: Vec<Checkpoint>,
    pub(crate) keys: Vec<PublicKeyRecord>,
}

/// The verdict on one checkpoint.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Attestation {
    /// Signature checks out against a trusted key, and the root matches the
    /// events in this pack.
    Verified { key_id: String },
    /// The range is not fully present in this pack, so the root cannot be
    /// recomputed. Legitimate for a partial export, and not a violation —
    /// but it does mean these events are not attested *by this pack*.
    NotCoveredByPack,
    /// Checkable, and wrong.
    Violation(String),
}

#[derive(Debug)]
pub(crate) struct CheckpointVerdict {
    pub(crate) seq_from: u64,
    pub(crate) seq_to: u64,
    pub(crate) root_hash: Hash32,
    pub(crate) attestation: Attestation,
}

impl CheckpointVerdict {
    #[must_use]
    pub(crate) fn is_violation(&self) -> bool {
        matches!(self.attestation, Attestation::Violation(_))
    }
}

/// Read a pack from a directory.
///
/// # Errors
/// If a required file is missing or unreadable, or the pack version is one
/// this build does not implement. All of these are "cannot verify", never
/// "invalid" — see the exit-code note in `main`.
pub(crate) fn read(dir: &Path) -> Result<Pack, String> {
    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    if manifest.pack_version != PACK_VERSION {
        return Err(format!(
            "this pack is {} and this build implements {PACK_VERSION}. \
             Nothing here indicates tampering — it means you need a verifier \
             that speaks the newer format",
            manifest.pack_version
        ));
    }

    let events = read_events(&dir.join("events.jsonl"))?;

    // Absent is not empty, and the distinction is reported rather than
    // flattened: a pack with no `checkpoints.json` is malformed, a pack with an
    // empty one is a chain nothing has sealed yet.
    let checkpoints: Vec<Checkpoint> = read_json(&dir.join("checkpoints.json"))?;
    let keys: Vec<PublicKeyRecord> = read_json(&dir.join("pubkeys.json"))?;

    Ok(Pack {
        manifest,
        events,
        checkpoints,
        keys,
    })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not readable as JSON: {e}", path.display()))
}

fn read_events(path: &Path) -> Result<Vec<AuditEvent>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str::<AuditEvent>(l)
                .map_err(|e| format!("{}, line {}: {e}", path.display(), i + 1))
        })
        .collect()
}

/// Check every checkpoint against the events the pack actually contains.
///
/// `trusted` is the key set to check signatures against. When an auditor
/// supplied fingerprints out of band it holds only those; otherwise it is the
/// pack's own `pubkeys.json`, and the report says so.
#[must_use]
pub(crate) fn attest(pack: &Pack, trusted: &BTreeMap<String, [u8; 32]>) -> Vec<CheckpointVerdict> {
    // One pass over the events, indexed by seq, so a checkpoint's range is a
    // lookup rather than a scan per checkpoint.
    //
    // The leaf is the **recomputed** hash, never the one the event carries.
    // That distinction is the whole value of this check: a checkpoint signs a
    // tree over event hashes, so feeding it the hashes the file claims would
    // make it attest an attacker's arithmetic. Recomputing means the signature
    // covers the events' *contents*, and an edited event fails here as well as
    // in the chain check — two independent detections of one tamper, which is
    // what lets the report say which property broke.
    //
    // An event this build cannot hash (a rule set it predates) simply has no
    // leaf, so its checkpoint reports as uncheckable rather than as wrong.
    let by_seq: BTreeMap<u64, Hash32> = pack
        .events
        .iter()
        .filter_map(|e| ancre_chain::event_hash(e).ok().map(|h| (e.seq, h)))
        .collect();

    pack.checkpoints
        .iter()
        .map(|cp| {
            let attestation = attest_one(cp, &by_seq, trusted);
            CheckpointVerdict {
                seq_from: cp.body.seq_from,
                seq_to: cp.body.seq_to,
                root_hash: cp.body.root_hash,
                attestation,
            }
        })
        .collect()
}

fn attest_one(
    cp: &Checkpoint,
    by_seq: &BTreeMap<u64, Hash32>,
    trusted: &BTreeMap<String, [u8; 32]>,
) -> Attestation {
    // Signature first. A checkpoint whose signature does not check is a
    // violation whether or not its range is in the pack — the range being
    // absent cannot excuse a bad signature.
    //
    // A key pinned with no id (`--key <hex>`, the common case when an auditor
    // was handed one fingerprint) is stored under the empty string and tried
    // for any checkpoint. Naming the key is an option, not a requirement: what
    // an auditor is given out of band is usually the bytes.
    let Some(key) = trusted.get(&cp.key_id).or_else(|| trusted.get("")) else {
        return Attestation::Violation(format!(
            "checkpoint over seq {}–{} is signed by key {}, which is not in the \
             key set being trusted. It cannot be attributed to anyone",
            cp.body.seq_from, cp.body.seq_to, cp.key_id
        ));
    };

    if let Err(e) = verify_checkpoint(cp, key) {
        return Attestation::Violation(format!(
            "the signature on the checkpoint over seq {}–{} does not verify \
             against key {}: {e}",
            cp.body.seq_from, cp.body.seq_to, cp.key_id
        ));
    }

    // Then the root, against the events actually present.
    let leaves: Vec<Hash32> = (cp.body.seq_from..=cp.body.seq_to)
        .map(|seq| by_seq.get(&seq).copied())
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();

    if leaves.len() != cp.body.tree_size() {
        return Attestation::NotCoveredByPack;
    }

    if tree_root(&leaves) != cp.body.root_hash {
        return Attestation::Violation(format!(
            "the events for seq {}–{} do not produce the root this checkpoint \
             signed: it attests {} but these events hash to {}",
            cp.body.seq_from,
            cp.body.seq_to,
            short(cp.body.root_hash),
            short(tree_root(&leaves))
        ));
    }

    Attestation::Verified {
        key_id: cp.key_id.clone(),
    }
}

/// The contiguous run of sequence numbers covered by a verified signature.
///
/// Contiguous on purpose. "seq 1–20 and 24–30 are attested" is a materially
/// different statement from "26 of 30 events are attested", and only the first
/// one tells an auditor where to look.
#[must_use]
pub(crate) fn attested_runs(verdicts: &[CheckpointVerdict]) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = verdicts
        .iter()
        .filter(|v| matches!(v.attestation, Attestation::Verified { .. }))
        .map(|v| (v.seq_from, v.seq_to))
        .collect();
    ranges.sort_unstable();

    let mut runs: Vec<(u64, u64)> = Vec::new();
    for (from, to) in ranges {
        match runs.last_mut() {
            // Abutting or overlapping ranges merge; a gap starts a new run.
            Some(last) if from <= last.1.saturating_add(1) => last.1 = last.1.max(to),
            _ => runs.push((from, to)),
        }
    }
    runs
}

/// Decode the pack's own keys into a trust set.
///
/// A key that is not 32 hex-encoded bytes is skipped and named. Refusing the
/// whole pack over one unreadable key would make a single bad row hide every
/// signature that *does* check.
#[must_use]
pub(crate) fn keys_from_pack(pack: &Pack) -> (BTreeMap<String, [u8; 32]>, Vec<String>) {
    let mut trusted = BTreeMap::new();
    let mut skipped = Vec::new();

    for k in &pack.keys {
        match decode_key(&k.public_key) {
            Some(bytes) => {
                trusted.insert(k.key_id.clone(), bytes);
            }
            None => skipped.push(format!(
                "key {} is not 32 hex-encoded bytes and was ignored",
                k.key_id
            )),
        }
    }
    (trusted, skipped)
}

/// `<key-id>=<64 hex>`, or bare hex when there is only one key to pin.
///
/// # Errors
/// If the value is not a hex-encoded 32-byte key.
pub(crate) fn parse_trusted_key(arg: &str) -> Result<(String, [u8; 32]), String> {
    let (key_id, hex) = arg.split_once('=').unwrap_or(("", arg));
    let bytes = decode_key(hex).ok_or_else(|| format!("--key: {hex} is not 64 hex characters"))?;
    Ok((key_id.to_string(), bytes))
}

fn decode_key(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// First eight bytes, which is what fits on a line and is what people compare.
#[must_use]
pub(crate) fn short(h: Hash32) -> String {
    h.to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(from: u64, to: u64, attested: bool) -> CheckpointVerdict {
        CheckpointVerdict {
            seq_from: from,
            seq_to: to,
            root_hash: ancre_canon::GENESIS,
            attestation: if attested {
                Attestation::Verified {
                    key_id: "cp-test".into(),
                }
            } else {
                Attestation::NotCoveredByPack
            },
        }
    }

    #[test]
    fn abutting_checkpoints_merge_into_one_run() {
        let runs = attested_runs(&[verdict(1, 20, true), verdict(21, 30, true)]);
        assert_eq!(runs, vec![(1, 30)]);
    }

    /// The property that makes the output worth reading: a hole between two
    /// signed ranges has to stay visible as a hole.
    #[test]
    fn a_gap_between_signed_ranges_is_reported_as_two_runs() {
        let runs = attested_runs(&[
            verdict(1, 20, true),
            verdict(21, 23, false),
            verdict(24, 30, true),
        ]);
        assert_eq!(runs, vec![(1, 20), (24, 30)]);
    }

    #[test]
    fn a_key_can_be_pinned_with_or_without_an_id() {
        let hex = "a".repeat(64);
        let (id, bytes) = parse_trusted_key(&format!("cp-1={hex}")).unwrap();
        assert_eq!(id, "cp-1");
        assert_eq!(bytes, [0xaa; 32]);

        let (id, _) = parse_trusted_key(&hex).unwrap();
        assert_eq!(id, "", "a bare key pins the value, not a name");
    }

    #[test]
    fn a_malformed_key_is_refused_rather_than_padded() {
        assert!(parse_trusted_key("not-hex").is_err());
        assert!(parse_trusted_key(&"z".repeat(64)).is_err());
        assert!(parse_trusted_key(&"a".repeat(63)).is_err());
    }
}
