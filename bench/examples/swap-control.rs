//! Does the reload-storm bench actually have the power to catch the mistake?
//!
//! The plan says the storm bench exists to catch an `RwLock<Arc<T>>` where
//! `ArcSwap` belongs. A bench that passes for both would be worthless, and a
//! passing bench is not evidence until you have seen it fail.
//!
//! So this runs the same read load against both, with readers on several cores
//! and a writer swapping underneath — the shape that actually exposes the
//! difference. Uncontended, on one thread, the two are indistinguishable;
//! that is exactly why the naive bench is a trap.
//!
//! ```sh
//! cargo run -p ancre-bench --release --example swap-control
//! ```

#![allow(clippy::cast_precision_loss)]

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ancre_bench::Percentiles;
use ancre_resolver::testing;
use ancre_types::{ConfigSnapshot, IngressMeta, Pins};
use arc_swap::ArcSwap;

/// Defaults to the machine's core count: the effect being measured is
/// cross-core cache-line contention, and under-subscribing hides it.
fn readers() -> usize {
    std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, std::num::NonZero::get))
}
const SAMPLES_PER_READER: usize = 200_000;
const RELOADS_PER_SEC: u64 = 10;

/// The read half of a resolve, reduced to the part the swap primitive
/// governs: take the snapshot, do the lookups, clone the pins out.
fn read(snap: &ConfigSnapshot, req: &IngressMeta<'_>) -> Option<Pins> {
    let binding = snap.key_binding(&req.api_key_hash)?;
    let system = snap.system(&binding.system_id)?;
    let route = system
        .routes
        .iter()
        .find(|r| r.matcher.matches(req))
        .unwrap_or(&system.routes[system.default_route]);
    Some(Pins {
        config_generation: snap.generation,
        config_hash: snap.content_hash,
        system_id: Arc::clone(&system.system_id),
        system_version: Arc::clone(&system.system_version),
        ifu_version: Arc::clone(&system.ifu_version),
        model_id: Arc::clone(&route.model_id),
        model_version: Arc::clone(&route.model_version),
        prompt_id: Arc::clone(&route.prompt_id),
        prompt_version: Arc::clone(&route.prompt_version),
        policy_id: Arc::clone(&system.policy_id),
        policy_version: Arc::clone(&system.policy_version),
        gateway_version: Arc::clone(&snap.gateway_version),
        risk_class: system.risk_class,
        resolved_stale: false,
        risk_flags: smallvec::SmallVec::new(),
    })
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("run with --release");
        std::process::exit(2);
    }

    println!(
        "{} reader threads, {RELOADS_PER_SEC} reloads/s, {SAMPLES_PER_READER} samples each\n",
        readers()
    );

    let arc_swap = run_arc_swap();
    println!("  ArcSwap          {arc_swap}");

    let rw_lock = run_rw_lock();
    println!("  RwLock<Arc<T>>   {rw_lock}");

    println!();
    let ratio = rw_lock.p99.as_nanos() as f64 / arc_swap.p99.as_nanos() as f64;
    println!("  p99 ratio: RwLock is {ratio:.1}x ArcSwap");
    if ratio < 1.5 {
        println!(
            "\n  NOTE: the two are close at this reader count, so the storm bench\n  \
             is not currently proving much. Pass a higher reader count as argv[1]\n  \
             before trusting it to catch a locking regression."
        );
    } else {
        println!(
            "\n  The storm bench has power: it separates the two primitives by a\n  \
             margin that would show up in the p99 being sold."
        );
    }
}

fn run_arc_swap() -> Percentiles {
    let cell = Arc::new(ArcSwap::from_pointee(testing::snapshot(41)));
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let cell = Arc::clone(&cell);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut g = 42;
            while !stop.load(Ordering::Relaxed) {
                cell.store(Arc::new(testing::snapshot(g)));
                g += 1;
                std::thread::sleep(Duration::from_millis(1000 / RELOADS_PER_SEC));
            }
        })
    };

    let readers: Vec<_> = (0..readers())
        .map(|_| {
            let cell = Arc::clone(&cell);
            std::thread::spawn(move || {
                let req = testing::request("key-high");
                let mut samples = Vec::with_capacity(SAMPLES_PER_READER);
                for _ in 0..SAMPLES_PER_READER {
                    let t = Instant::now();
                    let pins = read(&cell.load(), &req);
                    samples.push(t.elapsed());
                    black_box(pins.unwrap());
                }
                samples
            })
        })
        .collect();

    let mut all: Vec<Duration> = readers
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    Percentiles::from_samples(&mut all)
}

fn run_rw_lock() -> Percentiles {
    let cell = Arc::new(RwLock::new(Arc::new(testing::snapshot(41))));
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let cell = Arc::clone(&cell);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut g = 42;
            while !stop.load(Ordering::Relaxed) {
                *cell.write().unwrap() = Arc::new(testing::snapshot(g));
                g += 1;
                std::thread::sleep(Duration::from_millis(1000 / RELOADS_PER_SEC));
            }
        })
    };

    let readers: Vec<_> = (0..readers())
        .map(|_| {
            let cell = Arc::clone(&cell);
            std::thread::spawn(move || {
                let req = testing::request("key-high");
                let mut samples = Vec::with_capacity(SAMPLES_PER_READER);
                for _ in 0..SAMPLES_PER_READER {
                    let t = Instant::now();
                    let snap = Arc::clone(&cell.read().unwrap());
                    let pins = read(&snap, &req);
                    samples.push(t.elapsed());
                    black_box(pins.unwrap());
                }
                samples
            })
        })
        .collect();

    let mut all: Vec<Duration> = readers
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    Percentiles::from_samples(&mut all)
}
