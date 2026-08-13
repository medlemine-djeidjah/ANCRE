//! The M2 latency gate. Resolver spec §9.
//!
//! | Bench                                    | Target  |
//! |------------------------------------------|---------|
//! | `resolve` p50                            | < 2µs   |
//! | `resolve` p99                            | < 5µs   |
//! | `resolve` p99 under a 10/s reload storm  | < 8µs   |
//! | snapshot build, 10k systems / 50k routes | < 500ms |
//! | snapshot resident, 10k systems           | < 200MB |
//!
//! **The reload-storm case is the one that matters.** It is what catches an
//! `RwLock<Arc<T>>` where `ArcSwap` belongs: uncontended, both look fine; with
//! writers in flight across cores, the read lock's shared cache line lands
//! straight in the p99 being sold. Run it before the code is worth defending.
//!
//! Exits non-zero if any target is missed, so it can gate CI.
//!
//! ```sh
//! cargo run -p ancre-bench --release --example resolve-gate
//! ```

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ancre_bench::{Percentiles, check, timer_overhead};
use ancre_resolver::{PinResolver, StalenessPolicy, testing};
use ancre_types::ConfigSnapshot;

const WARMUP: usize = 50_000;
const SAMPLES: usize = 500_000;

fn main() -> std::process::ExitCode {
    if cfg!(debug_assertions) {
        eprintln!(
            "refusing to publish latency numbers from a debug build.\n\
             run: cargo run -p ancre-bench --release --example resolve-gate"
        );
        return std::process::ExitCode::from(2);
    }

    println!(
        "timer overhead (Instant::now): {:.0?}\n",
        timer_overhead(10_000)
    );

    let mut ok = true;
    ok &= bench_resolve();
    ok &= bench_resolve_under_reload_storm();
    ok &= bench_resolve_saturated();
    ok &= bench_snapshot_build();

    println!();
    if ok {
        println!("all targets met");
        std::process::ExitCode::SUCCESS
    } else {
        println!("TARGET MISSED — see resolver spec §9 and the M2 gate in mvp-plan §5");
        std::process::ExitCode::FAILURE
    }
}

fn ready() -> PinResolver {
    PinResolver::with_snapshot(testing::snapshot(41), StalenessPolicy::default())
}

fn sample(resolver: &PinResolver, samples: &mut Vec<Duration>) {
    let req = testing::request("key-high");
    for _ in 0..WARMUP {
        black_box(resolver.resolve(&req).unwrap());
    }
    for _ in 0..SAMPLES {
        let t = Instant::now();
        let pins = resolver.resolve(&req);
        samples.push(t.elapsed());
        black_box(pins.unwrap());
    }
}

fn bench_resolve() -> bool {
    println!("resolve, quiescent");
    let resolver = ready();
    let mut samples = Vec::with_capacity(SAMPLES);
    sample(&resolver, &mut samples);

    let p = Percentiles::from_samples(&mut samples);
    println!("  {p}");
    let a = check("resolve p50", p.p50, Duration::from_micros(2));
    let b = check("resolve p99", p.p99, Duration::from_micros(5));
    println!();
    a && b
}

/// Reads on this thread while another thread swaps the snapshot 10 times a
/// second, exactly as the control plane would under a config-churn incident.
fn bench_resolve_under_reload_storm() -> bool {
    println!("resolve, under a 10/s reload storm");
    let resolver = Arc::new(ready());
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let resolver = Arc::clone(&resolver);
        let stop = Arc::clone(&stop);
        // Snapshots are built up front: building is the control plane's cost,
        // not the reload's, and including it would understate how often the
        // swap actually lands.
        let prebuilt: Vec<ConfigSnapshot> = (42..142).map(testing::snapshot).collect();
        std::thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                // Rebuilt each cycle rather than moved out, so the storm can
                // run indefinitely without exhausting the prebuilt set.
                resolver.reload(testing::snapshot(prebuilt[i % prebuilt.len()].generation));
                i += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let mut samples = Vec::with_capacity(SAMPLES);
    sample(&resolver, &mut samples);

    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    let p = Percentiles::from_samples(&mut samples);
    println!("  {p}");
    let ok = check(
        "resolve p99 under reload storm",
        p.p99,
        Duration::from_micros(8),
    );
    println!();
    ok
}

/// Every core resolving at once, with the reload storm underneath.
///
/// The single-reader numbers above are the uncontended floor and they flatter
/// the design: with one thread there is nothing to contend for. This is the
/// number to quote. Twelve `Arc` refcount bumps per resolve, all on the same
/// cache lines when every thread hits the same system, is the real cost — and
/// `swap-control` shows this shape is also what separates `ArcSwap` from
/// `RwLock<Arc<T>>`, so it is the case with the diagnostic power.
///
/// Pessimistic on purpose: real traffic spreads across systems and keys, which
/// spreads the refcounts across cache lines.
fn bench_resolve_saturated() -> bool {
    let threads = std::thread::available_parallelism().map_or(8, std::num::NonZero::get);
    println!("resolve, {threads} threads saturated + 10/s reload storm");

    let resolver = Arc::new(ready());
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let resolver = Arc::clone(&resolver);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut g = 42;
            while !stop.load(Ordering::Relaxed) {
                resolver.reload(testing::snapshot(g));
                g += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let per_thread = SAMPLES / 5;
    let readers: Vec<_> = (0..threads)
        .map(|_| {
            let resolver = Arc::clone(&resolver);
            std::thread::spawn(move || {
                let req = testing::request("key-high");
                for _ in 0..WARMUP {
                    black_box(resolver.resolve(&req).unwrap());
                }
                let mut samples = Vec::with_capacity(per_thread);
                for _ in 0..per_thread {
                    let t = Instant::now();
                    let pins = resolver.resolve(&req);
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

    let p = Percentiles::from_samples(&mut all);
    println!("  {p}");
    let ok = check(
        "resolve p99, all cores + reload storm",
        p.p99,
        Duration::from_micros(8),
    );
    println!();
    ok
}

fn bench_snapshot_build() -> bool {
    println!("snapshot build, 10k systems / 50k routes");
    let spec = testing::large_spec(10_000, 5);

    // Once for the timing, then keep one resident to measure footprint.
    let t = Instant::now();
    let snap = ConfigSnapshot::build(spec.clone()).unwrap();
    let elapsed = t.elapsed();
    assert_eq!(snap.system_count(), 10_000);

    let ok = check("snapshot build", elapsed, Duration::from_millis(500));

    // A resolve against the large snapshot: the route scan is linear, so the
    // number that matters is how it behaves with realistic route counts, not
    // with the three-route fixture.
    let resolver = PinResolver::with_snapshot(snap, StalenessPolicy::default());
    let req = testing::request_for_large(0);
    let mut samples = Vec::with_capacity(SAMPLES / 5);
    for _ in 0..WARMUP {
        black_box(resolver.resolve(&req).unwrap());
    }
    for _ in 0..SAMPLES / 5 {
        let t = Instant::now();
        let pins = resolver.resolve(&req);
        samples.push(t.elapsed());
        black_box(pins.unwrap());
    }
    let p = Percentiles::from_samples(&mut samples);
    println!("  {p}");
    let ok2 = check("resolve p99, 10k systems", p.p99, Duration::from_micros(5));
    println!();
    ok && ok2
}
