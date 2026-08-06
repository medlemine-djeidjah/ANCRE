//! `ancre-verify` — standalone, offline chain verifier.
//!
//! This binary is the product's credibility. It is what an auditor runs, on
//! their own laptop, on a pack we handed them, to satisfy themselves that we
//! did not fabricate the record. It therefore:
//!
//! - opens no sockets
//! - reads only the files named on the command line
//! - prints a verdict a non-engineer can act on
//! - exits 0 for clean, 1 for violations, 2 for "cannot check"
//!
//! That third exit code matters. "I cannot evaluate this rule set" is a
//! different answer from "this chain is invalid", and collapsing the two into
//! one failure would be dishonest in the direction that hurts us most.

// Scaffold-only; remove as M1 lands. See the note in ancre-gateway's main.rs.
#![allow(dead_code, unreachable_pub)]

use std::process::ExitCode;

const USAGE: &str = "\
ancre-verify — offline verification of an Ancre evidence chain

USAGE:
    ancre-verify --pack <DIR>            verify a full evidence pack
    ancre-verify --chain <FILE.jsonl>    verify a raw event stream
    ancre-verify --pubkey <FILE>         trusted signing key (repeatable)

EXIT CODES:
    0  chain verified, all checkpoints signed by a trusted key
    1  violations found — see the report
    2  cannot verify (unknown canon_version, missing key, malformed pack)
";

fn main() -> ExitCode {
    // M1: JSONL fixture in, verdict out. The pack format follows in V1.
    todo!("M1: arg parse, stream events, verify_range, print report")
}

/// The report is read by a compliance officer, not an engineer.
///
/// Name the `seq`, say what is wrong in one sentence, and never print a stack
/// trace. "Chain verified: 2 431 087 events, seq 1–2431087, 244 checkpoints,
/// no violations" is the sentence the whole product exists to produce.
fn print_report() {
    todo!("M1")
}
