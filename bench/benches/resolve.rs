//! Criterion benches, for tracking regressions over time.
//!
//! The absolute targets live in `examples/resolve-gate.rs`, which measures raw
//! percentiles — criterion reports a mean and a confidence interval, which is
//! the right tool for "did this change make it slower" and the wrong one for
//! "is the p99 we sell still true".
//!
//! Run both. This one catches drift; the gate catches a broken claim.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ancre_resolver::{PinResolver, StalenessPolicy, testing};
use ancre_types::ConfigSnapshot;
use criterion::{Criterion, criterion_group, criterion_main};

fn ready() -> PinResolver {
    PinResolver::with_snapshot(testing::snapshot(41), StalenessPolicy::default())
}

fn bench_resolve(c: &mut Criterion) {
    let resolver = ready();
    let req = testing::request("key-high");

    c.bench_function("resolve", |b| {
        b.iter(|| black_box(resolver.resolve(black_box(&req)).unwrap()));
    });
}

fn bench_resolve_with_override(c: &mut Criterion) {
    // Overrides allocate. Rare by construction, but measured so that "rare"
    // stays a claim someone checked rather than one someone assumed.
    let resolver = ready();
    let headers: &[(&str, &str)] = &[];
    let req = ancre_types::IngressMeta {
        path: "/v1/chat/completions",
        model_alias: None,
        api_key_hash: testing::key_hash("key-high"),
        headers,
        overrides: ancre_types::PinOverrides {
            model_version: Some("gpt-4o-2099-01-01"),
            prompt_version: None,
        },
    };

    c.bench_function("resolve/with_override", |b| {
        b.iter(|| black_box(resolver.resolve(black_box(&req)).unwrap()));
    });
}

fn bench_resolve_under_reload_storm(c: &mut Criterion) {
    // A background task reloading 10x/second against the same resolver while
    // the measured thread resolves. The contention this exposes is invisible
    // in a single-threaded bench — see `examples/swap-control.rs`, which shows
    // this shape separating ArcSwap from RwLock by ~2x at the p99.
    let resolver = Arc::new(ready());
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let resolver = Arc::clone(&resolver);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut g = 42u64;
            while !stop.load(Ordering::Relaxed) {
                resolver.reload(testing::snapshot(g));
                g += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let req = testing::request("key-high");
    c.bench_function("resolve/under_reload_storm", |b| {
        b.iter(|| black_box(resolver.resolve(black_box(&req)).unwrap()));
    });

    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}

fn bench_resolve_large_snapshot(c: &mut Criterion) {
    let snap = ConfigSnapshot::build(testing::large_spec(10_000, 5)).unwrap();
    let resolver = PinResolver::with_snapshot(snap, StalenessPolicy::default());
    let req = testing::request_for_large(5_000);

    c.bench_function("resolve/10k_systems", |b| {
        b.iter(|| black_box(resolver.resolve(black_box(&req)).unwrap()));
    });
}

fn bench_snapshot_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_build");
    // Long enough that criterion's default 100 samples would take minutes.
    group.sample_size(10);

    let spec = testing::large_spec(10_000, 5);
    group.bench_function("10k_systems_50k_routes", |b| {
        b.iter(|| black_box(ConfigSnapshot::build(black_box(spec.clone())).unwrap()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_resolve,
    bench_resolve_with_override,
    bench_resolve_under_reload_storm,
    bench_resolve_large_snapshot,
    bench_snapshot_build
);
criterion_main!(benches);
