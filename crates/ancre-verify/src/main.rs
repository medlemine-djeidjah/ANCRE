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
//! different answer from "this chain is invalid", and collapsing the two would
//! be dishonest in the direction that costs the most credibility.

use std::io::{BufRead, BufWriter, Write};
use std::process::ExitCode;

use ancre_canon::{GENESIS, Hash32};
use ancre_chain::{ChainReport, verify_range};
use ancre_types::AuditEvent;

const USAGE: &str = "\
ancre-verify — offline verification of an Ancre evidence chain

USAGE:
    ancre-verify --chain <FILE.jsonl>   verify a chain, one JSON event per line
    ancre-verify --chain -              read the chain from stdin
    ancre-verify --from <HASH>          hash of the event before this range
                                        (default: the start of the chain)
    ancre-verify --quiet                verdict only, no violation list

EXIT CODES:
    0  chain verified, no violations
    1  violations found — the chain does not attest what it claims
    2  cannot verify (unreadable input, or a rule set this build predates)

This tool makes no network connections and reads no files other than the one
you name.
";

const EXIT_CLEAN: u8 = 0;
const EXIT_VIOLATIONS: u8 = 1;
const EXIT_CANNOT_VERIFY: u8 = 2;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("ancre-verify: {e}");
            ExitCode::from(EXIT_CANNOT_VERIFY)
        }
    }
}

struct Args {
    chain: String,
    from: Hash32,
    quiet: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut chain = None;
    let mut from = GENESIS;
    let mut quiet = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--quiet" | "-q" => quiet = true,
            "--chain" => {
                chain = Some(
                    it.next()
                        .ok_or("--chain needs a file path (or - for stdin)")?,
                );
            }
            "--from" => {
                let h = it.next().ok_or("--from needs a 64-character hex hash")?;
                from = Hash32::from_hex(&h)
                    .map_err(|_| format!("--from: {h} is not 64 hex characters"))?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Some(Args {
        chain: chain.ok_or("nothing to verify: pass --chain <FILE.jsonl>")?,
        from,
        quiet,
    }))
}

fn run() -> Result<u8, String> {
    let Some(args) = parse_args()? else {
        print!("{USAGE}");
        return Ok(EXIT_CLEAN);
    };

    let reader: Box<dyn BufRead> = if args.chain == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        let f = std::fs::File::open(&args.chain)
            .map_err(|e| format!("cannot read {}: {e}", args.chain))?;
        Box::new(std::io::BufReader::new(f))
    };

    // Parsed lazily so a chain larger than memory still verifies. A line that
    // does not parse is fatal rather than skipped: an unreadable event is a
    // hole in the evidence, and quietly stepping over it would turn a
    // malformed record into a clean verdict.
    let mut parse_error: Option<String> = None;
    let events = reader
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            if parse_error.is_some() {
                return None;
            }
            match line {
                Err(e) => {
                    parse_error = Some(format!("line {}: {e}", i + 1));
                    None
                }
                Ok(l) if l.trim().is_empty() => None,
                Ok(l) => match serde_json::from_str::<AuditEvent>(&l) {
                    Ok(ev) => Some(ev),
                    Err(e) => {
                        parse_error = Some(format!("line {}: {e}", i + 1));
                        None
                    }
                },
            }
        })
        .collect::<Vec<_>>();

    if let Some(e) = parse_error {
        return Err(format!(
            "{} is not a readable event stream — {e}",
            args.chain
        ));
    }

    let report = verify_range(events, args.from);

    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    print_report(&mut out, &report, args.quiet).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;

    Ok(if report.is_clean() {
        EXIT_CLEAN
    } else if report.is_inconclusive() {
        EXIT_CANNOT_VERIFY
    } else {
        EXIT_VIOLATIONS
    })
}

/// The report is read by a compliance officer, not an engineer.
///
/// Name the `seq`, say what is wrong in one sentence, and never print a stack
/// trace. "Chain verified: 2 431 087 events, seq 1–2431087, no violations" is
/// the sentence the whole product exists to produce.
fn print_report<W: Write>(w: &mut W, report: &ChainReport, quiet: bool) -> std::io::Result<()> {
    if report.events_checked == 0 {
        writeln!(w, "No events to check.")?;
        return Ok(());
    }

    let range = format!(
        "{} event{}, seq {}–{}",
        thousands(report.events_checked),
        if report.events_checked == 1 { "" } else { "s" },
        report.seq_from,
        report.seq_to
    );

    if report.is_clean() {
        writeln!(w, "Chain verified: {range}, no violations.")?;
        writeln!(w, "  range root: {}", report.root_hash)?;
        writeln!(w, "  chain head: {}", report.head_hash)?;
        return Ok(());
    }

    if report.is_inconclusive() {
        writeln!(
            w,
            "Cannot verify: {range}. Nothing here indicates tampering — this \
             build does not implement the rule set these events were sealed under."
        )?;
    } else {
        writeln!(
            w,
            "VERIFICATION FAILED: {range}, {} violation{}.",
            report.violations.len(),
            if report.violations.len() == 1 {
                ""
            } else {
                "s"
            }
        )?;
    }

    if !quiet {
        // An auditor needs the first few and the count, not 40 000 lines.
        const SHOWN: usize = 50;
        for v in report.violations.iter().take(SHOWN) {
            writeln!(w, "  - {v}")?;
        }
        if report.violations.len() > SHOWN {
            writeln!(
                w,
                "  … and {} more",
                thousands((report.violations.len() - SHOWN) as u64)
            )?;
        }
    }

    Ok(())
}

/// Thin-space grouping. A seven-digit event count is read wrong often enough
/// to be worth twelve lines.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1 000");
        assert_eq!(thousands(2_431_087), "2 431 087");
    }
}
