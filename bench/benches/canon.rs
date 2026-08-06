//! Canonical encoding and chain benches.
//!
//! Not on the request path — the gateway emits, the ingester chains — so these
//! size the ingester's throughput ceiling rather than defending a latency
//! claim. The number that matters: events chained per second per core, against
//! the 500 RPS/node target in PRD §8.

use std::hint::black_box;

use ancre_canon::GENESIS;
use ancre_chain::{event_hash, fixtures, verify_range};
use criterion::{Criterion, criterion_group, criterion_main};

fn bench_encode_event(c: &mut Criterion) {
    let event = &fixtures::chain(1)[0];
    c.bench_function("canon/encode_event", |b| {
        b.iter(|| black_box(black_box(event).hashed_body().unwrap()));
    });
}

fn bench_event_hash(c: &mut Criterion) {
    let event = &fixtures::chain(1)[0];
    c.bench_function("canon/event_hash", |b| {
        b.iter(|| black_box(event_hash(black_box(event)).unwrap()));
    });
}

fn bench_verify(c: &mut Criterion) {
    // Full-chain verification throughput. An auditor waiting fifteen minutes
    // for a verdict will not run it twice.
    let mut group = c.benchmark_group("chain_verify");
    group.sample_size(10);

    let events = fixtures::chain(10_000);
    group.bench_function("10k_events", |b| {
        b.iter(|| black_box(verify_range(black_box(events.clone()), GENESIS)));
    });

    group.finish();
}

criterion_group!(benches, bench_encode_event, bench_event_hash, bench_verify);
criterion_main!(benches);
