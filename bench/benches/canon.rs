//! Canonical encoding and chain benches.
//!
//! Not on the request path — the gateway emits, the ingester chains — so these
//! exist to size the ingester's throughput ceiling, not to defend a latency
//! claim. The number that matters: events chained per second per core, against
//! the 500 RPS/node target in PRD §8.

use criterion::{Criterion, criterion_group, criterion_main};

fn bench_encode_event(_c: &mut Criterion) {
    todo!("M1")
}

fn bench_event_hash(_c: &mut Criterion) {
    todo!("M1")
}

fn bench_verify_100k(_c: &mut Criterion) {
    // Full-chain verification throughput. An auditor waiting fifteen minutes
    // for a verdict will not run it twice.
    todo!("M1")
}

criterion_group!(
    benches,
    bench_encode_event,
    bench_event_hash,
    bench_verify_100k
);
criterion_main!(benches);
