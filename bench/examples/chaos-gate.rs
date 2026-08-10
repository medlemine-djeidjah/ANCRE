//! The M4 acceptance gate. mvp-plan §5, M4.
//!
//! > Kill ClickHouse for ten minutes under load; the gateway is unaffected,
//! > the ingester catches up, and the chain verifies with no gap.
//!
//! Run at scale — 100 000 events across the outage — because the failure this
//! guards against is a *gap*, and a gap is easiest to miss in a small run.
//!
//! The store is an in-memory fake that can be knocked over on command, so the
//! outage is instantaneous rather than ten real minutes. That is a real
//! fidelity gap and it is stated rather than hidden: this proves the chaining
//! and rollback logic survives an outage, **not** that the ClickHouse client
//! does. That check needs a real ClickHouse and belongs in M5 packaging.
//!
//! ```sh
//! cargo run -p ancre-bench --release --example chaos-gate
//! ```

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use ancre_canon::{GENESIS, Hash32};
use ancre_chain::{ChainId, CheckpointSigner, prove_inclusion, verify_inclusion, verify_range};
use ancre_ingester::chain_writer::IngestError;
use ancre_ingester::pipeline::Ingester;
use ancre_ingester::sink::{EventStore, RetryPolicy};
use ancre_types::{AuditEvent, EmittedEvent};

const BEFORE: u64 = 40_000;
const DURING: u64 = 40_000;
const AFTER: u64 = 20_000;
const BATCH: u64 = 500;

#[derive(Default)]
struct FlakyStore {
    rows: Mutex<Vec<AuditEvent>>,
    /// `event_id` → `seq`, maintained on insert. The real store answers the
    /// dedupe lookup from an index; a fake that answered it with a linear scan
    /// would make this gate measure the fake.
    by_id: Mutex<std::collections::HashMap<uuid::Uuid, u64>>,
    down: AtomicBool,
}

impl EventStore for &FlakyStore {
    async fn insert(&self, batch: &[AuditEvent]) -> Result<(), IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        let mut by_id = self.by_id.lock().unwrap();
        for e in batch {
            by_id.insert(e.emitted.event_id, e.seq);
        }
        self.rows.lock().unwrap().extend_from_slice(batch);
        Ok(())
    }

    async fn head(&self, _chain: &ChainId) -> Result<Option<(u64, Hash32)>, IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        let rows = self.rows.lock().unwrap();
        Ok(rows.last().map(|r| (r.seq, r.event_hash)))
    }

    async fn stored(
        &self,
        _chain: &ChainId,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, u64)>, IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        let by_id = self.by_id.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| by_id.get(id).map(|seq| (*id, *seq)))
            .collect())
    }

    async fn chains(&self) -> Result<Vec<(ChainId, u64, Hash32)>, IngestError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(IngestError::Store("connection refused".into()));
        }
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .last()
            .map(|r| {
                (
                    ChainId {
                        tenant_id: r.emitted.tenant_id.to_string(),
                        system_id: r.emitted.system_id.to_string(),
                    },
                    r.seq,
                    r.event_hash,
                )
            })
            .into_iter()
            .collect())
    }
}

fn batch(from: u64, n: u64) -> Vec<EmittedEvent> {
    (from..from + n)
        .map(|i| ancre_types::fixtures::event(i, GENESIS).emitted)
        .collect()
}

#[tokio::main(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "a linear four-phase scenario reads better in one place"
)]
async fn main() -> std::process::ExitCode {
    let store = FlakyStore::default();
    let mut ing = Ingester::new(&store, "ing-1").with_retry(RetryPolicy {
        initial: std::time::Duration::from_micros(10),
        max: std::time::Duration::from_micros(100),
        attempts: 3,
    });

    let started = Instant::now();

    println!("phase 1: {BEFORE} events, store healthy");
    let mut next = 1;
    for _ in 0..BEFORE / BATCH {
        ing.ingest(batch(next, BATCH)).await.unwrap();
        next += BATCH;
    }

    println!("phase 2: store down, {DURING} events keep arriving");
    store.down.store(true, Ordering::SeqCst);
    let mut unacked = Vec::new();
    for _ in 0..DURING / BATCH {
        let b = batch(next, BATCH);
        let r = ing.ingest(b.clone()).await.unwrap();
        assert_eq!(
            r.inserted, 0,
            "nothing may be stored while the store is down"
        );
        assert_eq!(r.deferred, 1, "the batch must be handed back unacked");
        unacked.push(b);
        next += BATCH;
    }
    // Total rows at the end of the outage — must still be exactly what
    // phase 1 wrote, i.e. the outage added nothing.
    let rows_after_outage = store.rows.lock().unwrap().len() as u64;

    println!(
        "phase 3: store back, bus redelivers {} batches",
        unacked.len()
    );
    store.down.store(false, Ordering::SeqCst);
    for b in unacked {
        ing.ingest(b).await.unwrap();
    }

    println!("phase 4: {AFTER} events, store healthy");
    for _ in 0..AFTER / BATCH {
        ing.ingest(batch(next, BATCH)).await.unwrap();
        next += BATCH;
    }

    // The failure mode this phase exists for does not error and does not gap.
    // A restart empties the dedupe window, so every message the bus still
    // holds unacked is one the new process cannot recognise. Chained again,
    // they take fresh seqs and the chain verifies perfectly — it simply claims
    // more traffic than happened, and the checkpointer then refuses the range
    // for good because the leaf count stops matching. Run at scale because by
    // now the window has evicted, which is the state a restart inherits.
    println!("phase 5: the process restarts, the bus redelivers the last {BATCH}");
    drop(ing);
    let mut restarted = Ingester::new(&store, "ing-2");
    let seeded = restarted.seed_from_store().await.unwrap();

    let replayed = batch(next - BATCH, BATCH);
    let r = restarted.ingest(replayed).await.unwrap();
    let redeliveries_caught = r.duplicates;
    let chained_after_restart = r.chained;

    // A silent chain still has to be countable after a restart, which is only
    // true if the restart took over its writer.
    let day = ancre_types::Timestamp::from_micros(1_754_400_000_000_000);
    let heartbeats_written = restarted.heartbeats(day).await.unwrap();
    // Same day, second process-lifetime: the store already holds it.
    let mut again = Ingester::new(&store, "ing-3");
    again.seed_from_store().await.unwrap();
    let heartbeats_repeated = again.heartbeats(day).await.unwrap();

    let elapsed = started.elapsed();
    let rows = {
        let mut r = store.rows.lock().unwrap().clone();
        r.sort_by_key(|e| e.seq);
        r
    };
    // The heartbeat is a real event on the chain and counts toward the total.
    let total = BEFORE + DURING + AFTER + 1;

    println!("\nresults");
    println!("  rows at end of outage:      {rows_after_outage} (phase 1 wrote {BEFORE})");
    println!("  rows after recovery:        {}", rows.len());
    println!("  wall time:                  {elapsed:.1?}");

    let mut ok = true;

    ok &= expect(
        "the outage stored nothing",
        rows_after_outage == BEFORE,
        &format!("{rows_after_outage} rows, expected {BEFORE}"),
    );
    ok &= expect(
        "every event recovered",
        rows.len() as u64 == total,
        &format!("{} of {total}", rows.len()),
    );

    let report = verify_range(rows.clone(), GENESIS);
    ok &= expect(
        "chain verifies with no gap",
        report.is_clean(),
        &format!("{} violations", report.violations.len()),
    );
    ok &= expect(
        "seq range is contiguous",
        report.seq_from == 1 && report.seq_to == total,
        &format!("{}..={}", report.seq_from, report.seq_to),
    );

    // Signing the recovered range is what makes the recovery attestable
    // rather than merely self-consistent.
    let leaves: Vec<Hash32> = rows.iter().map(|r| r.event_hash).collect();
    let signer = CheckpointSigner::from_bytes([5u8; 32], "cp-1".into());
    let cp = signer
        .seal_range("acme", "hr-screening", 1, &leaves)
        .unwrap();
    let target = BEFORE + DURING / 2;
    let proof = prove_inclusion(&cp, &leaves, target).unwrap();

    ok &= expect(
        "an event written across the outage proves into the signed range",
        verify_inclusion(&cp, &proof, &signer.verifying_key()).is_ok(),
        "inclusion failed",
    );
    ok &= expect(
        "the proof stays logarithmic",
        proof.path.len() <= 20,
        &format!("{} hashes for {total} events", proof.path.len()),
    );

    ok &= expect(
        "a restart recognises redeliveries its window cannot remember",
        redeliveries_caught == BATCH && chained_after_restart == 0,
        &format!("{redeliveries_caught} of {BATCH} caught, {chained_after_restart} chained again"),
    );
    ok &= expect(
        "a restart takes over the chains already in the store",
        seeded == 1,
        &format!("{seeded} chains seeded"),
    );
    ok &= expect(
        "a restarted process still heartbeats a chain that has gone quiet",
        heartbeats_written == 1,
        &format!("{heartbeats_written} written"),
    );
    ok &= expect(
        "a second restart the same day does not write a second heartbeat",
        heartbeats_repeated == 0,
        &format!("{heartbeats_repeated} written"),
    );

    println!(
        "\n  Note: an in-memory store. This proves the chaining and rollback\n  \
         logic survives an outage, not that the ClickHouse client does —\n  \
         that needs a real ClickHouse and belongs in M5 packaging."
    );

    println!();
    if ok {
        println!("all targets met");
        std::process::ExitCode::SUCCESS
    } else {
        println!("ACCEPTANCE FAILED — see mvp-plan §5, M4");
        std::process::ExitCode::FAILURE
    }
}

fn expect(label: &str, ok: bool, detail: &str) -> bool {
    println!(
        "  [{}] {label:<52} {}",
        if ok { "PASS" } else { "FAIL" },
        if ok { "" } else { detail }
    );
    ok
}
