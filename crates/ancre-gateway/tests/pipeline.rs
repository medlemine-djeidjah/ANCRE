//! The request pipeline, end to end, against a fake upstream.
//!
//! No network: what matters is what ends up in the audit event, and that
//! should not depend on reaching OpenAI. Resolver spec §10 cases 2 and 5 live
//! here — the two that belong to the gateway rather than the resolver.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ancre_gateway::proxy::{GatewayState, handle};
use ancre_gateway::telemetry::{BatchConfig, Batcher, EventSink, SinkError, TelemetryFork};
use ancre_gateway::upstream::Upstream;
use ancre_provider::ProviderKind;
use ancre_resolver::{PinResolver, StalenessPolicy, testing};
use ancre_types::{EmittedEvent, Outcome, RiskFlag};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

type Chunks = Vec<Bytes>;

/// Replays a canned response and records what it was sent.
#[derive(Clone)]
struct FakeUpstream {
    status: StatusCode,
    chunks: Chunks,
    /// Signal end-of-stream on the last frame rather than with a following
    /// `None`. What hyper does; see `FakeBody`.
    end_on_last: bool,
    seen: Arc<Mutex<Vec<(ProviderKind, Bytes)>>>,
}

impl FakeUpstream {
    fn json(body: &str) -> Self {
        Self {
            status: StatusCode::OK,
            chunks: vec![Bytes::from(body.to_string())],
            end_on_last: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A streamed response, one SSE frame per chunk, so the tap sees the same
    /// read boundaries a real provider would produce.
    fn sse(frames: &[&str]) -> Self {
        Self {
            status: StatusCode::OK,
            chunks: frames
                .iter()
                .map(|f| Bytes::from(format!("data: {f}\n\n")))
                .collect(),
            end_on_last: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    /// Behave the way a real Content-Length response does.
    fn ending_on_the_last_frame(mut self) -> Self {
        self.end_on_last = true;
        self
    }

    fn sent_body(&self) -> Bytes {
        self.seen.lock().unwrap()[0].1.clone()
    }

    fn sent_to(&self) -> ProviderKind {
        self.seen.lock().unwrap()[0].0
    }
}

/// The response body a fake upstream hands back.
///
/// Hand-written rather than a `StreamBody`, because the thing worth varying is
/// exactly what `StreamBody` cannot express: whether end-of-stream arrives as
/// a following `None` or as a flag on the last frame. Hyper's `Incoming` does
/// the latter for a Content-Length response and then stops polling, and a body
/// that only ever sees the first shape hides a whole class of bug.
struct FakeBody {
    chunks: std::vec::IntoIter<Bytes>,
    end_on_last: bool,
}

impl http_body::Body for FakeBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        std::task::Poll::Ready(self.chunks.next().map(|c| Ok(http_body::Frame::data(c))))
    }

    fn is_end_stream(&self) -> bool {
        self.end_on_last && self.chunks.len() == 0
    }
}

type UpstreamBody = FakeBody;

impl Upstream for FakeUpstream {
    type Body = UpstreamBody;

    async fn send(
        &self,
        provider: ProviderKind,
        req: Request<Bytes>,
    ) -> Result<Response<Self::Body>, BoxError> {
        self.seen.lock().unwrap().push((provider, req.into_body()));
        Ok(Response::builder().status(self.status).body(FakeBody {
            chunks: self.chunks.clone().into_iter(),
            end_on_last: self.end_on_last,
        })?)
    }
}

#[derive(Clone, Default)]
struct Collector {
    events: Arc<Mutex<Vec<EmittedEvent>>>,
}

impl EventSink for Collector {
    async fn publish(&self, batch: Vec<EmittedEvent>) -> Result<(), SinkError> {
        self.events.lock().unwrap().extend(batch);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    state: GatewayState<FakeUpstream>,
    collector: Collector,
    fork: TelemetryFork,
    batcher: Batcher<Collector>,
}

fn harness(upstream: FakeUpstream) -> Harness {
    let (fork, rx) = TelemetryFork::new(4096);
    let collector = Collector::default();
    let batcher = Batcher::new(
        rx,
        collector.clone(),
        BatchConfig {
            max_events: 512,
            max_wait: Duration::from_millis(10),
        },
    );

    Harness {
        state: GatewayState {
            resolver: Arc::new(PinResolver::with_snapshot(
                testing::snapshot(41),
                StalenessPolicy::default(),
            )),
            telemetry: fork.clone(),
            upstream,
            node_id: Arc::from("gw-test-1"),
            pinning_enabled: true,
        },
        collector,
        fork,
        batcher,
    }
}

impl Harness {
    /// Drive one request through, drain telemetry, return what it produced.
    async fn run(self, req: Request<Full<Bytes>>) -> (StatusCode, Vec<EmittedEvent>) {
        let response = handle(&self.state, req)
            .await
            .expect("the pipeline must not fail");
        let status = response.status();

        // Consuming the body is what a real client does, and it is what makes
        // the tap fire: the event is written when the response finishes, not
        // when it starts.
        let _ = response.into_body().collect().await;

        drop(self.state);
        drop(self.fork);
        self.batcher.run().await;

        let events = self.collector.events.lock().unwrap().clone();
        (status, events)
    }

    /// Drive a request the way hyper drives a complete response: take exactly
    /// the frames the body has and then stop — **without** the extra poll that
    /// would return `None`.
    ///
    /// That last poll is the one a real server does not make once it knows the
    /// body is done, and every fake that loops on `.collect()` makes it. The
    /// difference is the whole bug this reproduces.
    async fn run_taking_exactly(
        self,
        frames: usize,
        req: Request<Full<Bytes>>,
    ) -> Vec<EmittedEvent> {
        let response = handle(&self.state, req)
            .await
            .expect("the pipeline must not fail");
        let mut body = response.into_body();

        for _ in 0..frames {
            assert!(
                std::pin::Pin::new(&mut body).frame().await.is_some(),
                "the body owed another frame"
            );
        }
        drop(body);

        drop(self.state);
        drop(self.fork);
        self.batcher.run().await;

        self.collector.events.lock().unwrap().clone()
    }

    /// Drive a request and abandon the response part-way, the way a client
    /// that hangs up mid-stream does.
    async fn run_abandoned(self, req: Request<Full<Bytes>>) -> Vec<EmittedEvent> {
        let response = handle(&self.state, req)
            .await
            .expect("pipeline must not fail");
        let mut body = response.into_body();

        // Take one frame, then drop the rest on the floor.
        let _ = std::pin::Pin::new(&mut body).frame().await;
        drop(body);

        drop(self.state);
        drop(self.fork);
        self.batcher.run().await;

        let events = self.collector.events.lock().unwrap();
        events.clone()
    }
}

fn request(key: &str, body: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

const CHAT: &str = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
const CHAT_STREAM: &str =
    r#"{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

/// The bug this test exists for, found by running the real binary and not by
/// any fake: hyper stops polling a Content-Length body once its last byte is
/// written, so `poll_frame` never returns `None` and the tap would only ever
/// `finish` from `Drop` — the client-hung-up path. Every ordinary completion
/// was recorded as `interrupted`, on a 200.
///
/// An event whose outcome contradicts its own status code is worse than a
/// missing event: it is evidence that reads as an incident that never
/// happened.
#[tokio::test]
async fn a_response_that_ends_on_its_last_frame_is_recorded_as_ok() {
    let up = FakeUpstream::json(
        r#"{"id":"chatcmpl-1","model":"gpt-4o-2024-08-06",
            "usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
    )
    .ending_on_the_last_frame();

    let events = harness(up)
        .run_taking_exactly(1, request("key-high", CHAT))
        .await;

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].outcome, Outcome::Ok);
    assert_eq!(events[0].metrics.http_status, 200);
    // The pins still come off the body that was observed on the way past.
    assert_eq!(&*events[0].pins.model_version, "gpt-4o-2024-08-06");
    assert_eq!(events[0].metrics.tokens_in, 9);
}

#[tokio::test]
async fn a_completion_produces_exactly_one_pinned_audit_event() {
    let up = FakeUpstream::json(
        r#"{"id":"chatcmpl-1","model":"gpt-4o-2024-08-06",
            "usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
    );
    let (status, events) = harness(up).run(request("key-high", CHAT)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(events.len(), 1, "one request, one event");

    let e = &events[0];
    assert_eq!(e.outcome, Outcome::Ok);
    assert_eq!(&*e.tenant_id, "acme");
    assert_eq!(&*e.system_id, testing::SYSTEM_A);
    assert_eq!(&*e.node_id, "gw-test-1");
    assert_eq!(e.metrics.http_status, 200);
    assert_eq!((e.metrics.tokens_in, e.metrics.tokens_out), (9, 2));
    assert!(!e.pins.has_gap(), "every pin must be resolved");
}

/// The pin comes from the response, not the request. This is the whole thesis
/// in one assertion: the caller asked for `gpt-4o`, and the record says which
/// weights actually answered.
#[tokio::test]
async fn model_version_is_read_from_the_response_not_the_request() {
    let up = FakeUpstream::json(r#"{"model":"gpt-4o-2024-08-06"}"#);
    let (_, events) = harness(up).run(request("key-high", CHAT)).await;

    assert_eq!(&*events[0].pins.model_version, "gpt-4o-2024-08-06");
    assert!(!events[0].pins.risk_flags.contains(&RiskFlag::UnpinnedModel));
}

/// **Resolver spec §10, test 5.** The provider would not name pinned weights.
/// Flag it — and serve the request anyway.
#[tokio::test]
async fn case_5_a_floating_alias_is_flagged_and_the_request_still_succeeds() {
    let up = FakeUpstream::json(r#"{"model":"gpt-4o","usage":{"prompt_tokens":9}}"#);
    let (status, events) = harness(up).run(request("key-high", CHAT)).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "a governance finding is not an outage"
    );
    assert_eq!(&*events[0].pins.model_version, "unresolved:gpt-4o");
    assert!(events[0].pins.risk_flags.contains(&RiskFlag::UnpinnedModel));
    assert!(
        events[0].pins.has_gap(),
        "an unresolved alias is a countable gap"
    );
}

#[tokio::test]
async fn a_streamed_response_is_pinned_from_its_frames() {
    let up = FakeUpstream::sse(&[
        r#"{"id":"1","model":"gpt-4o-2024-08-06","choices":[{"delta":{"role":"assistant"}}]}"#,
        r#"{"id":"1","model":"gpt-4o-2024-08-06","choices":[{"delta":{"content":"hi"}}]}"#,
        r#"{"id":"1","usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
        "[DONE]",
    ]);
    let (_, events) = harness(up).run(request("key-high", CHAT_STREAM)).await;

    assert_eq!(events.len(), 1);
    assert_eq!(&*events[0].pins.model_version, "gpt-4o-2024-08-06");
    assert_eq!(
        (events[0].metrics.tokens_in, events[0].metrics.tokens_out),
        (9, 2)
    );
}

/// A client that hangs up on a 90-second completion still leaves a record.
/// "The user cancelled" is exactly the kind of thing an oversight review asks
/// about, and no event at all would be indistinguishable from no request.
#[tokio::test]
async fn a_client_that_disconnects_mid_stream_still_produces_an_event() {
    let up = FakeUpstream::sse(&[
        r#"{"id":"1","model":"gpt-4o-2024-08-06","choices":[{"delta":{"content":"a"}}]}"#,
        r#"{"id":"1","choices":[{"delta":{"content":"b"}}]}"#,
        r#"{"id":"1","choices":[{"delta":{"content":"c"}}]}"#,
    ]);
    let events = harness(up)
        .run_abandoned(request("key-high", CHAT_STREAM))
        .await;

    assert_eq!(events.len(), 1, "an abandoned stream is still evidence");
    assert_eq!(events[0].outcome, Outcome::Interrupted);
}

// ---------------------------------------------------------------------------
// Routing and translation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_openai_request_passes_through_byte_for_byte() {
    let up = FakeUpstream::json(r#"{"model":"gpt-4o-2024-08-06"}"#);
    let h = harness(up.clone());
    let _ = h.run(request("key-high", CHAT)).await;

    assert_eq!(up.sent_to(), ProviderKind::OpenAi);
    assert_eq!(up.sent_body(), Bytes::from(CHAT));
}

#[tokio::test]
async fn a_claude_route_is_translated_to_the_anthropic_wire_format() {
    // The `careful` alias routes to claude-opus-5 in the fixture.
    let body = r#"{"model":"careful","messages":[{"role":"system","content":"Be brief."},
        {"role":"user","content":"hi"}]}"#;
    let up = FakeUpstream::json(r#"{"model":"claude-opus-5-20250805"}"#);
    let h = harness(up.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer key-high")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let (_, events) = h.run(req).await;

    assert_eq!(up.sent_to(), ProviderKind::Anthropic);
    let sent: serde_json::Value = serde_json::from_slice(&up.sent_body()).unwrap();
    assert_eq!(
        sent["system"], "Be brief.",
        "system becomes a top-level field"
    );
    assert!(
        sent["max_tokens"].is_number(),
        "Anthropic requires max_tokens"
    );
    assert_eq!(&*events[0].pins.model_version, "claude-opus-5-20250805");
}

/// A request that cannot be translated faithfully is refused rather than
/// quietly altered. A wrong event is worse than a rejected request.
#[tokio::test]
async fn an_untranslatable_request_is_refused_with_no_event() {
    let body = r#"{"model":"careful","messages":[],"tools":[{"type":"function"}]}"#;
    let up = FakeUpstream::json("{}");
    let h = harness(up);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer key-high")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let (status, events) = h.run(req).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        events.is_empty(),
        "nothing reached a model, so nothing to record"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_request_without_a_bearer_token_is_401_with_no_event() {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .body(Full::new(Bytes::from(CHAT)))
        .unwrap();
    let (status, events) = harness(FakeUpstream::json("{}")).run(req).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(events.is_empty());
}

#[tokio::test]
async fn an_unknown_key_is_401_with_no_event() {
    let (status, events) = harness(FakeUpstream::json("{}"))
        .run(request("key-that-was-revoked", CHAT))
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(events.is_empty());
}

/// A refused request never reached a model, so there is no decision to record.
/// Writing an event with `unknown` pins would be worse than none — it would
/// look like evidence.
#[tokio::test]
async fn a_stale_config_refuses_a_high_risk_request_with_503_and_no_event() {
    let (fork, rx) = TelemetryFork::new(1024);
    let collector = Collector::default();
    let batcher = Batcher::new(rx, collector.clone(), BatchConfig::default());

    let state = GatewayState {
        resolver: Arc::new(PinResolver::with_snapshot(
            testing::snapshot(41),
            StalenessPolicy {
                budget: Duration::ZERO,
                fail_closed_on_stale: true,
            },
        )),
        telemetry: fork.clone(),
        upstream: FakeUpstream::json("{}"),
        node_id: Arc::from("gw-test-1"),
        pinning_enabled: true,
    };

    let response = handle(&state, request("key-high", CHAT)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    drop(state);
    drop(fork);
    batcher.run().await;
    assert!(collector.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_upstream_error_status_is_recorded_rather_than_hidden() {
    let up = FakeUpstream::json(r#"{"error":{"message":"rate limited"}}"#)
        .with_status(StatusCode::TOO_MANY_REQUESTS);
    let (status, events) = harness(up).run(request("key-high", CHAT)).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        events.len(),
        1,
        "a refused upstream call is still a request"
    );
    assert_eq!(events[0].metrics.http_status, 429);
    assert_eq!(&*events[0].metrics.error_code, "http_429");
    // The provider named no model, so the pin is unknown and flagged.
    assert_eq!(&*events[0].pins.model_version, "unknown");
    assert!(events[0].pins.risk_flags.contains(&RiskFlag::UnpinnedModel));
}

// ---------------------------------------------------------------------------
// Overrides — spec §10 case 8, at the HTTP level
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_header_override_reaches_the_pins_and_is_flagged() {
    let up = FakeUpstream::json(r#"{"model":"gpt-4o-2024-08-06"}"#);
    let h = harness(up);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer key-high")
        .header("x-ancre-pin-prompt-version", "b3:caller-supplied")
        .body(Full::new(Bytes::from(CHAT)))
        .unwrap();
    let (_, events) = h.run(req).await;

    assert_eq!(&*events[0].pins.prompt_version, "b3:caller-supplied");
    assert!(events[0].pins.risk_flags.contains(&RiskFlag::PinOverridden));
}

// ---------------------------------------------------------------------------
// Telemetry never blocks the request
// ---------------------------------------------------------------------------

/// The property the whole fork exists for: with telemetry wedged, requests
/// still succeed. The evidence has a hole and the hole is counted — the
/// traffic does not stop.
#[tokio::test]
async fn requests_still_succeed_when_telemetry_is_full() {
    // Capacity 1 and no batcher draining: every event after the first drops.
    let (fork, _rx) = TelemetryFork::new(1);
    let state = GatewayState {
        resolver: Arc::new(PinResolver::with_snapshot(
            testing::snapshot(41),
            StalenessPolicy::default(),
        )),
        telemetry: fork.clone(),
        upstream: FakeUpstream::json(r#"{"model":"gpt-4o-2024-08-06"}"#),
        node_id: Arc::from("gw-test-1"),
        pinning_enabled: true,
    };

    for _ in 0..50 {
        let response = handle(&state, request("key-high", CHAT)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.into_body().collect().await;
    }

    assert!(fork.dropped() >= 49, "drops must be counted, not silent");
}
