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

mod pack;

use std::collections::BTreeMap;
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
    ancre-verify --pack <DIR>           verify an evidence pack: the chain, the
                                        signed checkpoints over it, and which
                                        events those signatures actually cover
    ancre-verify --key <HEX>            trust only this public key — the
                                        fingerprint you were given out of band.
                                        Repeatable, and `<key-id>=<HEX>` names
                                        it. Without this, the pack's own keys
                                        are used and the report says so
    ancre-verify --from <HASH>          hash of the event before this range
                                        (default: the start of the chain)
    ancre-verify --quiet                verdict only, no violation list

EXIT CODES:
    0  verified, no violations
    1  violations found — the evidence does not attest what it claims
    2  cannot verify (unreadable input, or a rule set this build predates)

This tool makes no network connections and reads no files other than the ones
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
    /// Exactly one of these two. A pack contains a chain, so accepting both
    /// would mean verifying one and silently ignoring the other.
    target: Target,
    from: Hash32,
    quiet: bool,
    /// Keys the auditor supplied out of band. Empty means "use the pack's own",
    /// which is a materially weaker claim and is reported as such.
    trusted: BTreeMap<String, [u8; 32]>,
}

enum Target {
    Chain(String),
    Pack(std::path::PathBuf),
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut chain = None;
    let mut pack_dir = None;
    let mut from = GENESIS;
    let mut quiet = false;
    let mut trusted = BTreeMap::new();

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
            "--pack" => {
                pack_dir = Some(std::path::PathBuf::from(
                    it.next().ok_or("--pack needs a directory")?,
                ));
            }
            "--key" => {
                let v = it.next().ok_or("--key needs a 64-character hex key")?;
                let (id, bytes) = pack::parse_trusted_key(&v)?;
                trusted.insert(id, bytes);
            }
            "--from" => {
                let h = it.next().ok_or("--from needs a 64-character hex hash")?;
                from = Hash32::from_hex(&h)
                    .map_err(|_| format!("--from: {h} is not 64 hex characters"))?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    let target = match (chain, pack_dir) {
        (Some(_), Some(_)) => {
            return Err(
                "--chain and --pack are alternatives: a pack already contains a chain".into(),
            );
        }
        (Some(c), None) => Target::Chain(c),
        (None, Some(d)) => Target::Pack(d),
        (None, None) => {
            return Err("nothing to verify: pass --chain <FILE.jsonl> or --pack <DIR>".into());
        }
    };

    Ok(Some(Args {
        target,
        from,
        quiet,
        trusted,
    }))
}

fn run() -> Result<u8, String> {
    let Some(args) = parse_args()? else {
        print!("{USAGE}");
        return Ok(EXIT_CLEAN);
    };

    match &args.target {
        Target::Chain(path) => run_chain(&args, path),
        Target::Pack(dir) => run_pack(&args, dir),
    }
}

/// A pack: the chain, plus what has actually signed it.
///
/// The chain verdict comes first and is the same one `--chain` produces. What
/// follows is the question a chain alone cannot answer — *who says so* — and
/// the two are printed as separate verdicts on purpose. "The events are
/// internally consistent" and "a key you trust signed them" fail
/// independently, and an auditor needs to know which one did.
fn run_pack(args: &Args, dir: &std::path::Path) -> Result<u8, String> {
    let pack = pack::read(dir)?;

    let (pack_keys, skipped) = pack::keys_from_pack(&pack);
    let pinned = !args.trusted.is_empty();
    let trusted = if pinned {
        args.trusted.clone()
    } else {
        pack_keys
    };

    // An explicit `--from` always wins. Otherwise a pack that declares its own
    // anchor is anchored there, and the header says so — the alternative is
    // that every partial export reports a broken first link, which trains an
    // auditor to ignore exactly the message that matters.
    let declared_anchor = pack
        .manifest
        .prev_hash
        .as_deref()
        .map(Hash32::from_hex)
        .transpose()
        .map_err(|_| "the manifest's prev_hash is not 64 hex characters".to_string())?;
    let anchored_from_manifest = args.from == GENESIS && declared_anchor.is_some();
    let from = if anchored_from_manifest {
        declared_anchor.unwrap_or(GENESIS)
    } else {
        args.from
    };

    let report = verify_range(pack.events.clone(), from);
    let verdicts = pack::attest(&pack, &trusted);

    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let printed = (|| -> std::io::Result<()> {
        print_pack_header(&mut out, &pack.manifest, &report, anchored_from_manifest)?;
        print_report(&mut out, &report, args.quiet)?;
        writeln!(out)?;
        print_attestation(&mut out, &pack, &verdicts, &trusted, pinned, &skipped)?;
        out.flush()
    })();
    printed.map_err(|e| e.to_string())?;

    let signature_violations = verdicts.iter().any(pack::CheckpointVerdict::is_violation);
    Ok(if report.is_inconclusive() {
        EXIT_CANNOT_VERIFY
    } else if !report.is_clean() || signature_violations {
        EXIT_VIOLATIONS
    } else {
        EXIT_CLEAN
    })
}

fn run_chain(args: &Args, chain: &str) -> Result<u8, String> {
    let reader: Box<dyn BufRead> = if chain == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        let f = std::fs::File::open(chain).map_err(|e| format!("cannot read {chain}: {e}"))?;
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
        return Err(format!("{chain} is not a readable event stream — {e}"));
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

fn print_pack_header<W: Write>(
    w: &mut W,
    m: &pack::Manifest,
    report: &ChainReport,
    anchored_from_manifest: bool,
) -> std::io::Result<()> {
    writeln!(w, "Pack: {}/{}", m.tenant_id, m.system_id)?;
    if !m.created_at.is_empty() {
        let by = if m.produced_by.is_empty() {
            "an unnamed build"
        } else {
            &m.produced_by
        };
        writeln!(w, "  produced {} by {by}", m.created_at)?;
    }
    if !m.source.is_empty() {
        writeln!(w, "  source: {}", m.source)?;
    }
    if anchored_from_manifest {
        writeln!(
            w,
            "  anchored at the prev_hash this pack declares, which nothing has \
             signed. Pass --from <HASH> to anchor it independently"
        )?;
    }

    // The manifest is unsigned, so a disagreement here is not a violation —
    // but it is the cheapest possible signal that a pack was truncated in
    // transit, and it costs an auditor nothing to be told.
    if report.events_checked > 0 {
        let declared = (m.seq_from, m.seq_to);
        if let (Some(from), Some(to)) = declared {
            if from != report.seq_from || to != report.seq_to {
                writeln!(
                    w,
                    "  ! the manifest declares seq {from}–{to}, and this pack \
                     contains seq {}–{}. The manifest is not signed, so treat \
                     the events as authoritative and ask why they differ",
                    report.seq_from, report.seq_to
                )?;
            }
        }
    }
    writeln!(w)
}

/// Who says so.
///
/// The chain verdict above says the events are internally consistent. This
/// says which of them a signature actually covers — the question that decides
/// whether an auditor can rely on them, and the one this whole system exists
/// to answer.
fn print_attestation<W: Write>(
    w: &mut W,
    p: &pack::Pack,
    verdicts: &[pack::CheckpointVerdict],
    trusted: &BTreeMap<String, [u8; 32]>,
    pinned: bool,
    skipped: &[String],
) -> std::io::Result<()> {
    for s in skipped {
        writeln!(w, "  ! {s}")?;
    }

    if verdicts.is_empty() {
        writeln!(
            w,
            "NOT ATTESTED: this pack contains no signed checkpoints. The events \
             are internally consistent and nothing has vouched for them — which \
             is the expected state for a chain younger than one checkpoint \
             interval, and a finding for one that is not."
        )?;
        return Ok(());
    }

    let verified = verdicts
        .iter()
        .filter(|v| matches!(v.attestation, pack::Attestation::Verified { .. }))
        .count();
    let violations: Vec<&pack::CheckpointVerdict> =
        verdicts.iter().filter(|v| v.is_violation()).collect();

    if violations.is_empty() {
        writeln!(w, "Checkpoints: {verified} of {} verified.", verdicts.len())?;
    } else {
        writeln!(
            w,
            "CHECKPOINT VERIFICATION FAILED: {} of {} did not verify.",
            violations.len(),
            verdicts.len()
        )?;
    }

    for v in verdicts {
        match &v.attestation {
            pack::Attestation::Verified { key_id } => writeln!(
                w,
                "  seq {}–{}  root {}  signed by {key_id}",
                v.seq_from,
                v.seq_to,
                pack::short(v.root_hash)
            )?,
            // Not a violation, and not silence either. An auditor handed a
            // partial export needs to know the difference between "no
            // signature covers these events" and "the signature is here but
            // the events it covers are not".
            pack::Attestation::NotCoveredByPack => writeln!(
                w,
                "  seq {}–{}  signed, but this pack does not contain that whole \
                 range — nothing to check the root against",
                v.seq_from, v.seq_to
            )?,
            pack::Attestation::Violation(msg) => writeln!(w, "  - {msg}")?,
        }
    }

    let runs = pack::attested_runs(verdicts);
    if runs.is_empty() {
        writeln!(w, "Attested range: none.")?;
    } else {
        let spans: Vec<String> = runs.iter().map(|(a, b)| format!("{a}–{b}")).collect();
        writeln!(w, "Attested range: seq {}.", spans.join(", "))?;
    }

    writeln!(w)?;
    if pinned {
        writeln!(
            w,
            "Signatures were checked against the {} key{} you pinned on the \
             command line, not against the keys in this pack.",
            trusted.len(),
            if trusted.len() == 1 { "" } else { "s" }
        )?;
    } else {
        writeln!(
            w,
            "Signatures were checked against this pack's own keys. That proves \
             the pack is internally consistent and says nothing about who made \
             it — compare these fingerprints with the ones you were given \
             separately, or re-run with --key <HEX>:"
        )?;
        for k in &p.keys {
            let window = match k.valid_to {
                None => format!("active since {}", k.valid_from.to_rfc3339()),
                Some(t) => format!("valid {} to {}", k.valid_from.to_rfc3339(), t.to_rfc3339()),
            };
            writeln!(w, "  {}  {}  ({window})", k.key_id, k.public_key)?;
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
