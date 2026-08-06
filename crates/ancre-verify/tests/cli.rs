//! End-to-end tests against the real binary.
//!
//! M1's done-when (mvp-plan §5): a deliberately corrupted event in a fixture
//! is caught by the verifier, and it names the seq. These run the compiled
//! CLI, not a library function, because the exit code and the wording of the
//! verdict are the deliverable — an auditor never calls `verify_range`.

use std::io::Write;
use std::process::Command;

const EXIT_CLEAN: i32 = 0;
const EXIT_VIOLATIONS: i32 = 1;
const EXIT_CANNOT_VERIFY: i32 = 2;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn write_chain(events: &[ancre_types::AuditEvent]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    for e in events {
        writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
    }
    f.flush().unwrap();
    f
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

#[test]
fn a_clean_chain_exits_zero_and_says_so() {
    let f = write_chain(&ancre_chain::fixtures::chain(5_000));
    let run = verify(&["--chain", f.path().to_str().unwrap()]);

    assert_eq!(run.code, EXIT_CLEAN, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("Chain verified"), "{}", run.stdout);
    assert!(run.stdout.contains("5 000 events"), "{}", run.stdout);
    assert!(run.stdout.contains("seq 1–5000"), "{}", run.stdout);
}

/// The test the whole milestone is measured by.
#[test]
fn a_corrupted_event_is_caught_and_the_report_names_its_seq() {
    let mut events = ancre_chain::fixtures::chain(10_000);
    // Edit one row, the way someone with ClickHouse DDL rights would.
    events[4_206].emitted.metrics.http_status = 500;
    let f = write_chain(&events);

    let run = verify(&["--chain", f.path().to_str().unwrap()]);

    assert_eq!(run.code, EXIT_VIOLATIONS, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("VERIFICATION FAILED"), "{}", run.stdout);
    assert!(
        run.stdout.contains("event 4207 was altered"),
        "the report must name the seq: {}",
        run.stdout
    );
}

#[test]
fn a_removed_event_is_reported_as_missing() {
    let mut events = ancre_chain::fixtures::chain(200);
    events.remove(99); // seq 100
    let f = write_chain(&events);

    let run = verify(&["--chain", f.path().to_str().unwrap()]);

    assert_eq!(run.code, EXIT_VIOLATIONS);
    assert!(
        run.stdout.contains("events 100 to 100 are missing"),
        "{}",
        run.stdout
    );
}

/// "I cannot check this" must never be reported as "this is invalid".
#[test]
fn an_unknown_rule_set_exits_two_and_does_not_claim_tampering() {
    let mut events = ancre_chain::fixtures::chain(10);
    events[4].canon_version = "ancre-canon/99".into();
    let f = write_chain(&events);

    let run = verify(&["--chain", f.path().to_str().unwrap()]);

    assert_eq!(run.code, EXIT_CANNOT_VERIFY, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("Cannot verify"), "{}", run.stdout);
    assert!(
        run.stdout.contains("Nothing here indicates tampering"),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("VERIFICATION FAILED"),
        "an unimplemented rule set must not read as a failed chain: {}",
        run.stdout
    );
}

#[test]
fn a_chunk_verifies_against_the_previous_chunks_head() {
    let all = ancre_chain::fixtures::chain(300);
    let anchor = all[149].event_hash;
    let f = write_chain(&all[150..]);

    let run = verify(&[
        "--chain",
        f.path().to_str().unwrap(),
        "--from",
        &anchor.to_hex(),
    ]);

    assert_eq!(run.code, EXIT_CLEAN, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("seq 151–300"), "{}", run.stdout);
}

#[test]
fn the_wrong_anchor_is_a_broken_link_not_a_silent_pass() {
    let all = ancre_chain::fixtures::chain(300);
    let f = write_chain(&all[150..]);

    // Anchored at the start of the chain instead of at event 150.
    let run = verify(&["--chain", f.path().to_str().unwrap()]);

    assert_eq!(run.code, EXIT_VIOLATIONS);
    assert!(
        run.stdout.contains("does not follow the one before it"),
        "{}",
        run.stdout
    );
}

#[test]
fn a_malformed_line_is_fatal_rather_than_skipped() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    for e in ancre_chain::fixtures::chain(3) {
        writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
    }
    writeln!(f, "{{not json}}").unwrap();
    f.flush().unwrap();

    let run = verify(&["--chain", f.path().to_str().unwrap()]);

    // Skipping it would turn a hole in the evidence into a clean verdict.
    assert_eq!(run.code, EXIT_CANNOT_VERIFY, "{}{}", run.stdout, run.stderr);
    assert!(run.stderr.contains("line 4"), "{}", run.stderr);
}

#[test]
fn a_missing_file_reports_cannot_verify() {
    let run = verify(&["--chain", "/nonexistent/chain.jsonl"]);
    assert_eq!(run.code, EXIT_CANNOT_VERIFY);
    assert!(run.stderr.contains("cannot read"), "{}", run.stderr);
}

#[test]
fn no_arguments_is_an_error_not_a_pass() {
    let run = verify(&[]);
    assert_eq!(run.code, EXIT_CANNOT_VERIFY);
    assert!(run.stderr.contains("nothing to verify"), "{}", run.stderr);
}

#[test]
fn help_exits_zero() {
    let run = verify(&["--help"]);
    assert_eq!(run.code, EXIT_CLEAN);
    assert!(run.stdout.contains("EXIT CODES"), "{}", run.stdout);
}

/// The JSON an auditor opens must be readable — hashes as hex, not as arrays
/// of 32 integers.
#[test]
fn the_fixture_format_is_human_readable() {
    let e = &ancre_chain::fixtures::chain(1)[0];
    let json = serde_json::to_string(e).unwrap();
    assert!(
        json.contains(&format!("\"event_hash\":\"{}\"", e.event_hash.to_hex())),
        "{json}"
    );
    assert!(!json.contains("\"event_hash\":["), "{json}");
}
