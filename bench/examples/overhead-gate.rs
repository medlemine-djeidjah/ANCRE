//! The M3 overhead gate. mvp-plan §5, M3.
//!
//! | Measurement                          | Target |
//! |--------------------------------------|--------|
//! | end-to-end p99 overhead vs baseline  | < 2ms  |
//! | added TTFT vs baseline               | < 2ms  |
//!
//! **Measured against a null-gateway baseline**: the same binary, the same
//! request path, the same fake upstream, with pinning compiled out. Measuring
//! against direct-to-provider instead would fold network variance into the
//! number and produce something that falls apart the first time a prospect's
//! own engineer reproduces it. Overhead only means anything as a delta against
//! an otherwise identical path.
//!
//! The upstream here is a local in-process fake, deliberately. A real provider
//! call is 300ms of network and model time that would swamp a 1ms signal —
//! this isolates what Ancre itself costs, which is the number being sold.
//!
//! ```sh
//! cargo run -p ancre-bench --release --example overhead-gate
//! ```

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ancre_bench::{Percentiles, check};
use ancre_gateway::proxy::{GatewayState, handle};
use ancre_gateway::telemetry::{BatchConfig, Batcher, EventSink, SinkError, TelemetryFork};
use ancre_gateway::upstream::Upstream;
use ancre_provider::ProviderKind;
use ancre_resolver::{PinResolver, StalenessPolicy, testing};
use ancre_types::EmittedEvent;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const WARMUP: usize = 2_000;
const SAMPLES: usize = 50_000;

const CHAT: &str = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hello"}]}"#;
const RESPONSE: &str = r#"{"id":"chatcmpl-1","object":"chat.completion",
    "model":"gpt-4o-2024-08-06",
    "choices":[{"index":0,"message":{"role":"assistant","content":"Hi there."}}],
    "usage":{"prompt_tokens":9,"completion_tokens":3,"total_tokens":12}}"#;

#[derive(Clone)]
struct FakeUpstream {
    body: Bytes,
}

impl Upstream for FakeUpstream {
    type Body = Full<Bytes>;

    async fn send(
        &self,
        _provider: ProviderKind,
        _req: Request<Bytes>,
    ) -> Result<Response<Self::Body>, BoxError> {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Full::new(self.body.clone()))?)
    }
}

/// Counts rather than stores: holding 50 000 events would measure the
/// allocator, not the gateway.
#[derive(Clone, Default)]
struct CountingSink {
    n: Arc<Mutex<u64>>,
}

impl EventSink for CountingSink {
    async fn publish(&self, batch: Vec<EmittedEvent>) -> Result<(), SinkError> {
        *self.n.lock().unwrap() += batch.len() as u64;
        Ok(())
    }
}

fn state(pinning_enabled: bool, fork: TelemetryFork) -> GatewayState<FakeUpstream> {
    GatewayState {
        resolver: Arc::new(PinResolver::with_snapshot(
            testing::snapshot(41),
            StalenessPolicy::default(),
        )),
        telemetry: fork,
        upstream: FakeUpstream {
            body: Bytes::from(RESPONSE),
        },
        node_id: Arc::from("gw-bench"),
        pinning_enabled,
    }
}

fn request() -> Request<Full<Bytes>> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer key-high")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(CHAT)))
        .unwrap()
}

async fn measure(pinning_enabled: bool) -> (Percentiles, Percentiles) {
    let (fork, rx) = TelemetryFork::new(65_536);
    let sink = CountingSink::default();
    let batcher = tokio::spawn(Batcher::new(rx, sink.clone(), BatchConfig::default()).run());
    let st = state(pinning_enabled, fork.clone());

    for _ in 0..WARMUP {
        let resp = handle(&st, request()).await.unwrap();
        let _ = resp.into_body().collect().await;
    }

    let mut total = Vec::with_capacity(SAMPLES);
    let mut ttfb = Vec::with_capacity(SAMPLES);

    for _ in 0..SAMPLES {
        let t = Instant::now();
        let resp = handle(&st, request()).await.unwrap();
        // Time to the response head: everything Ancre does before a byte of
        // the answer can start flowing. This is the number a streaming client
        // feels as added TTFT.
        ttfb.push(t.elapsed());
        let _ = resp.into_body().collect().await;
        total.push(t.elapsed());
    }

    drop(st);
    drop(fork);
    let _ = batcher.await;

    (
        Percentiles::from_samples(&mut total),
        Percentiles::from_samples(&mut ttfb),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    if cfg!(debug_assertions) {
        eprintln!(
            "refusing to publish latency numbers from a debug build.\n\
             run: cargo run -p ancre-bench --release --example overhead-gate"
        );
        return std::process::ExitCode::from(2);
    }

    println!("null baseline (pinning compiled out)");
    let (base_total, base_ttfb) = measure(false).await;
    println!("  total {base_total}");
    println!("  head  {base_ttfb}");

    println!("\nfull gateway (pins resolved, event emitted)");
    let (full_total, full_ttfb) = measure(true).await;
    println!("  total {full_total}");
    println!("  head  {full_ttfb}");

    println!("\ndelta");
    let total_delta = full_total.p99.saturating_sub(base_total.p99);
    let ttfb_delta = full_ttfb.p99.saturating_sub(base_ttfb.p99);
    println!(
        "  p50 total: {:?}",
        full_total.p50.saturating_sub(base_total.p50)
    );

    let a = check(
        "p99 added overhead (pinning)",
        total_delta,
        Duration::from_millis(2),
    );
    let b = check(
        "p99 added TTFT (pinning)",
        ttfb_delta,
        Duration::from_millis(2),
    );

    // The delta above isolates what *pinning* costs, which is what the
    // null-baseline design is for. But the baseline still hashes the key,
    // parses the body, builds the event and forks telemetry — so the number a
    // customer actually feels is the full in-process cost, checked here
    // against the PRD §8 budget of 1ms.
    println!();
    let c = check(
        "p99 total gateway cost, in-process",
        full_total.p99,
        Duration::from_millis(1),
    );

    println!(
        "\n  Note: an in-process fake upstream, so this isolates what Ancre costs.\n  \
         A real provider call adds ~300ms of network and model time that would\n  \
         swamp the signal entirely."
    );

    println!();
    if a && b && c {
        println!("all targets met");
        std::process::ExitCode::SUCCESS
    } else {
        println!("TARGET MISSED — mvp-plan §14 kill criterion: the only technical");
        println!("differentiator is the latency claim. Re-plan before continuing.");
        std::process::ExitCode::FAILURE
    }
}
