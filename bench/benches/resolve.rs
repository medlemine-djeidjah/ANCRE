//! Resolver benches. Targets from `version-pin-resolver-spec.md` §9.
//!
//! | Bench                                   | Target   |
//! |-----------------------------------------|----------|
//! | `resolve` p50                           | < 2µs    |
//! | `resolve` p99                           | < 5µs    |
//! | `resolve` p99 under 10/s reload storm   | < 8µs    |
//! | snapshot build, 10k systems / 50k routes| < 500ms  |
//!
//! **Write and run the reload-storm bench on day one of M2.** It is the one
//! that catches an `RwLock` in place of `ArcSwap`, and it is cheap to run
//! before there is code worth defending. Gate: if p99 cannot get under 5µs by
//! 23 Aug, stop and re-plan — the whole latency claim descends from it.

use criterion::{Criterion, criterion_group, criterion_main};

fn bench_resolve(_c: &mut Criterion) {
    todo!("M2")
}

fn bench_resolve_under_reload_storm(_c: &mut Criterion) {
    // A background task calling reload() 10x/second against the same resolver
    // while the measured thread resolves. The contention this exposes is
    // invisible in a single-threaded bench.
    todo!("M2")
}

fn bench_snapshot_build(_c: &mut Criterion) {
    todo!("M2")
}

criterion_group!(
    benches,
    bench_resolve,
    bench_resolve_under_reload_storm,
    bench_snapshot_build
);
criterion_main!(benches);
