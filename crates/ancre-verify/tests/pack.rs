//! End-to-end tests of `--pack`, against the real binary.
//!
//! A pack is what an auditor is actually handed, so what is tested here is the
//! auditor's experience: the exit code, and whether the words on screen would
//! lead a careful non-engineer to the right conclusion. The interesting cases
//! are the ones where the answer is *partly* good — a chain that verifies with
//! a signature that does not, a signature that is fine over events the pack
//! does not contain — because those are the ones a naive verifier reports as
//! either wholly clean or wholly broken, and both would be wrong.

use std::io::Write;
use std::process::Command;

use ancre_chain::{CheckpointSigner, checkpoint::CheckpointBody};
use ancre_types::{AuditEvent, Timestamp};

const EXIT_CLEAN: i32 = 0;
const EXIT_VIOLATIONS: i32 = 1;
const EXIT_CANNOT_VERIFY: i32 = 2;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn verify(args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_ancre-verify"))
        .args(args)
        .output()
        .expect("the verifier binary must be built");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Build a pack on disk the way the control plane's three endpoints would.
struct PackBuilder {
    dir: tempfile::TempDir,
    events: Vec<AuditEvent>,
    signer: CheckpointSigner,
    checkpoints: Vec<ancre_chain::Checkpoint>,
    keys: Vec<serde_json::Value>,
    /// Set by `only_from`, the way a partial export's manifest would carry it.
    anchor: Option<String>,
}

impl PackBuilder {
    fn new(n: u64) -> Self {
        let signer = CheckpointSigner::generate("cp-test".into());
        let keys = vec![serde_json::json!({
            "key_id": signer.key_id(),
            "public_key": hex(&signer.verifying_key()),
            "valid_from": Timestamp::now(),
            "valid_to": serde_json::Value::Null,
        })];

        Self {
            dir: tempfile::TempDir::new().unwrap(),
            events: ancre_chain::fixtures::chain(n),
            signer,
            checkpoints: Vec::new(),
            keys,
            anchor: None,
        }
    }

    /// Seal `from..=to` over the events as they currently stand.
    fn seal(mut self, from: u64, to: u64) -> Self {
        let leaves: Vec<_> = self
            .events
            .iter()
            .filter(|e| e.seq >= from && e.seq <= to)
            .map(|e| e.event_hash)
            .collect();

        let body = CheckpointBody {
            tenant_id: "acme".into(),
            system_id: "hr-screening".into(),
            seq_from: from,
            seq_to: to,
            root_hash: ancre_canon::tree_root(&leaves),
            built_at: Timestamp::now(),
            canon_version: ancre_canon::CANON_VERSION.into(),
        };
        self.checkpoints.push(self.signer.sign(body).unwrap());
        self
    }

    fn tamper(mut self, index: usize) -> Self {
        self.events[index].emitted.metrics.http_status = 500;
        self
    }

    /// Drop events before `seq`, as a partial export would.
    fn only_from(mut self, seq: u64) -> Self {
        self.anchor = self
            .events
            .iter()
            .find(|e| e.seq == seq)
            .map(|e| e.prev_hash.to_hex());
        self.events.retain(|e| e.seq >= seq);
        self
    }

    fn write(self) -> tempfile::TempDir {
        let p = self.dir.path();
        std::fs::write(
            p.join("manifest.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "pack_version": "ancre-pack/1",
                "tenant_id": "acme",
                "system_id": "hr-screening",
                "created_at": "2026-08-09T10:00:00Z",
                "produced_by": "ancre-control 0.1.0+test",
                "source": "http://localhost:8081",
                "prev_hash": self.anchor,
            }))
            .unwrap(),
        )
        .unwrap();

        let mut f = std::fs::File::create(p.join("events.jsonl")).unwrap();
        for e in &self.events {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
        f.flush().unwrap();

        std::fs::write(
            p.join("checkpoints.json"),
            serde_json::to_vec(&self.checkpoints).unwrap(),
        )
        .unwrap();
        std::fs::write(
            p.join("pubkeys.json"),
            serde_json::to_vec(&self.keys).unwrap(),
        )
        .unwrap();

        self.dir
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn path(dir: &tempfile::TempDir) -> String {
    dir.path().to_str().unwrap().to_string()
}

#[test]
fn a_signed_pack_verifies_and_names_the_attested_range() {
    let dir = PackBuilder::new(100).seal(1, 60).seal(61, 100).write();
    let run = verify(&["--pack", &path(&dir)]);

    assert_eq!(run.code, EXIT_CLEAN, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("Pack: acme/hr-screening"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("Chain verified"), "{}", run.stdout);
    assert!(
        run.stdout.contains("Checkpoints: 2 of 2 verified"),
        "{}",
        run.stdout
    );
    // Two abutting checkpoints are one covered run, not two.
    assert!(
        run.stdout.contains("Attested range: seq 1–100."),
        "{}",
        run.stdout
    );
}

/// The claim that has to be made honestly. A pack carrying its own keys proves
/// internal consistency; the report must say so rather than let the reader
/// infer provenance it does not have.
#[test]
fn a_pack_verified_against_its_own_keys_says_that_is_what_happened() {
    let dir = PackBuilder::new(20).seal(1, 20).write();
    let run = verify(&["--pack", &path(&dir)]);

    assert_eq!(run.code, EXIT_CLEAN, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("says nothing about who made"),
        "the circularity must be stated: {}",
        run.stdout
    );
    assert!(run.stdout.contains("cp-test"), "{}", run.stdout);
}

#[test]
fn a_pinned_key_that_did_not_sign_this_pack_fails_it() {
    let dir = PackBuilder::new(20).seal(1, 20).write();
    let stranger = "a".repeat(64);

    let run = verify(&["--pack", &path(&dir), "--key", &stranger]);

    assert_eq!(run.code, EXIT_VIOLATIONS, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("CHECKPOINT VERIFICATION FAILED"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("does not verify"), "{}", run.stdout);
    // And it must not quietly fall back to the pack's own key.
    assert!(
        run.stdout.contains("you pinned on the command line"),
        "{}",
        run.stdout
    );
}

/// Both halves fail independently, and the report has to show both. A tampered
/// event breaks the chain *and* the root the checkpoint signed.
#[test]
fn tampering_breaks_the_chain_and_the_signature_over_it() {
    let dir = PackBuilder::new(50)
        .seal(1, 50)
        .tamper(24) // seq 25, after the checkpoint was signed
        .write();

    let run = verify(&["--pack", &path(&dir)]);

    assert_eq!(run.code, EXIT_VIOLATIONS, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("event 25 was altered"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("do not produce the root this checkpoint signed"),
        "the signature check must fail on its own terms, not merely inherit the \
         chain's verdict: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("Attested range: none."),
        "a range containing an altered event must not be reported as attested: {}",
        run.stdout
    );
}

/// A partial export is legitimate. A signature over events that are not in the
/// pack is not a violation — and it is not attestation either, and conflating
/// the two in either direction misleads.
#[test]
fn a_signature_over_events_the_pack_lacks_is_reported_not_counted() {
    let dir = PackBuilder::new(100)
        .seal(1, 40)
        .seal(41, 100)
        .only_from(41)
        .write();

    let run = verify(&["--pack", &path(&dir)]);

    assert_eq!(run.code, EXIT_CLEAN, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout
            .contains("this pack does not contain that whole range"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("Attested range: seq 41–100."),
        "the covered range must exclude the part that could not be checked: {}",
        run.stdout
    );
}

#[test]
fn a_chain_nothing_has_sealed_is_reported_as_not_attested() {
    let dir = PackBuilder::new(10).write();
    let run = verify(&["--pack", &path(&dir)]);

    // The events are internally consistent, so this is not a violation — but
    // an auditor must not read "Chain verified" as "somebody vouched for this".
    assert_eq!(run.code, EXIT_CLEAN, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("NOT ATTESTED"), "{}", run.stdout);
}

#[test]
fn a_pack_from_a_future_format_is_refused_rather_than_guessed_at() {
    let dir = PackBuilder::new(5).write();
    std::fs::write(
        dir.path().join("manifest.json"),
        br#"{"pack_version":"ancre-pack/9","tenant_id":"acme","system_id":"hr"}"#,
    )
    .unwrap();

    let run = verify(&["--pack", &path(&dir)]);

    assert_eq!(run.code, EXIT_CANNOT_VERIFY, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stderr.contains("Nothing here indicates tampering"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_missing_pack_file_is_cannot_verify_and_names_the_file() {
    let dir = PackBuilder::new(5).write();
    std::fs::remove_file(dir.path().join("pubkeys.json")).unwrap();

    let run = verify(&["--pack", &path(&dir)]);

    assert_eq!(run.code, EXIT_CANNOT_VERIFY);
    assert!(run.stderr.contains("pubkeys.json"), "{}", run.stderr);
}

#[test]
fn chain_and_pack_together_are_refused_rather_than_one_silently_ignored() {
    let run = verify(&["--chain", "-", "--pack", "/tmp"]);
    assert_eq!(run.code, EXIT_CANNOT_VERIFY);
    assert!(run.stderr.contains("alternatives"), "{}", run.stderr);
}
