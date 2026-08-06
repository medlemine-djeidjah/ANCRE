//! The hot path.
//!
//! Raw hyper rather than axum here: axum's extractors cost more than they are
//! worth on a proxy that would rather not parse the body at all. axum is used
//! in the control plane, where ergonomics win (PRD §10).
//!
//! Order matters and is not negotiable:
//!
//! 1. hash the key
//! 2. **resolve pins once** into `RequestCtx`
//! 3. call the provider, streaming straight through
//! 4. fork telemetry — after the response is on its way, never before
//!
//! Step 2 happens exactly once. Re-resolving before writing the audit event is
//! the bug that eats this whole design: a streaming completion can run for 90
//! seconds, and a reload in that window would make the event report a
//! configuration the request never used (spec §5).

use std::sync::Arc;
use std::time::Instant;

use ancre_provider::{Provider, ProviderKind, anthropic::Anthropic, openai::OpenAi};
use ancre_resolver::{PinResolver, ResolveError};
use ancre_types::{IngressMeta, RequestCtx};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{Request, Response, StatusCode};

use crate::tap::{Tap, TappedBody};
use crate::telemetry::TelemetryFork;
use crate::{auth, upstream::Upstream};

static OPENAI: OpenAi = OpenAi;
static ANTHROPIC: Anthropic = Anthropic;

/// Shared, immutable, cloned into every connection task.
pub struct GatewayState<U: Upstream> {
    pub resolver: Arc<PinResolver>,
    pub telemetry: TelemetryFork,
    pub upstream: U,
    pub node_id: Arc<str>,
    /// Compiled out by the null-baseline build, so "overhead" can be measured
    /// as a delta against an otherwise identical binary rather than against
    /// direct-to-provider, which would conflate our cost with network variance
    /// (mvp-plan §5, M3).
    pub pinning_enabled: bool,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// Hand-written rather than derived: deriving would put a `Debug` bound on the
// upstream type, and a connection pool has no useful `Debug` to give. The
// bound would then propagate through every signature that touches the state.
impl<U: Upstream> std::fmt::Debug for GatewayState<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayState")
            .field("generation", &self.resolver.generation())
            .field("node_id", &self.node_id)
            .field("pinning_enabled", &self.pinning_enabled)
            .finish_non_exhaustive()
    }
}

impl<B> std::fmt::Debug for ProxyBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tapped(_) => f.write_str("ProxyBody::Tapped"),
            Self::Fixed(_) => f.write_str("ProxyBody::Fixed"),
        }
    }
}

/// Serve one request.
///
/// Generic over the request body rather than tied to `hyper::body::Incoming`:
/// `Incoming` has no public constructor, so pinning the signature to it would
/// make the pipeline testable only against a live socket. What matters here is
/// what lands in the audit event, and that should not need a network to check.
pub async fn handle<U, B>(
    state: &GatewayState<U>,
    req: Request<B>,
) -> Result<Response<ProxyBody<U::Body>>, BoxError>
where
    U: Upstream,
    B: http_body::Body<Data = Bytes> + Send,
    B::Error: Into<BoxError>,
{
    let started = Instant::now();

    // 1. Admission.
    let path = req.uri().path().to_string();
    let (parts, body) = req.into_parts();

    // Header views borrowed from `parts`, which outlives resolution. One
    // allocation for the vector; the strings themselves are not copied.
    let headers: Vec<(&str, &str)> = parts
        .headers
        .iter()
        .filter_map(|(n, v)| v.to_str().ok().map(|v| (n.as_str(), v)))
        .collect();

    let Some(bearer) = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, v)| auth::bearer_token(v))
    else {
        return Ok(refuse(StatusCode::UNAUTHORIZED, "missing bearer token"));
    };

    // The request body is read in full before the upstream call: the provider
    // needs a complete JSON document, and every LLM request body is small
    // relative to its response. This is the one place buffering is correct.
    let body = body.collect().await.map_err(Into::into)?.to_bytes();
    let request_digest = ancre_canon::hash_bytes(&body);

    let model_alias = extract_model(&body);

    // 2. Resolve, exactly once.
    let overrides = auth::overrides_from_headers(&headers);
    let meta = IngressMeta {
        path: &path,
        model_alias: model_alias.as_deref(),
        api_key_hash: auth::key_hash(bearer),
        headers: &headers,
        overrides,
    };

    let pins = if state.pinning_enabled {
        match state.resolver.resolve(&meta) {
            Ok(p) => p,
            Err(e) => return Ok(refuse_resolve(&e)),
        }
    } else {
        // Null baseline: same code path, same allocations, no resolution.
        ancre_types::Pins::null_baseline()
    };

    let tenant_id: Arc<str> = state
        .resolver
        .snapshot()
        .key_binding(&meta.api_key_hash)
        .map_or_else(|| Arc::from("unknown"), |b| Arc::clone(&b.tenant_id));

    let provider: &'static dyn Provider = match provider_for(&pins.model_id) {
        ProviderKind::Anthropic => &ANTHROPIC,
        ProviderKind::OpenAi => &OPENAI,
    };

    let ctx = RequestCtx {
        trace_id: Arc::from(uuid::Uuid::new_v4().to_string()),
        attempt_seq: 0,
        pins,
        started,
    };

    // 3. Upstream. Streamed straight through.
    let streaming = is_streaming(&body);
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let header_map = parts.headers.clone();
    let translated = match provider.translate_request(&body) {
        Ok(b) => b,
        Err(e) => return Ok(refuse(StatusCode::BAD_REQUEST, &e.to_string())),
    };

    // `headers` borrows `parts`, so the upstream request is assembled from a
    // fresh head rather than moving it. Cheap: header values are `Bytes`.
    let mut upstream_req = Request::new(Bytes::from(translated));
    *upstream_req.method_mut() = method;
    *upstream_req.uri_mut() = uri;
    *upstream_req.headers_mut() = header_map;
    let response = state.upstream.send(provider.kind(), upstream_req).await?;
    let (parts, upstream_body) = response.into_parts();
    let http_status = parts.status.as_u16();

    // 4. Fork telemetry — attached to the body, so it fires when the response
    //    finishes rather than before it starts.
    let tap = Tap {
        ctx,
        tenant_id,
        node_id: Arc::clone(&state.node_id),
        provider,
        telemetry: state.telemetry.clone(),
        http_status,
        request_digest,
        streaming,
    };

    Ok(Response::from_parts(
        parts,
        ProxyBody::Tapped(Box::new(TappedBody::new(upstream_body, tap))),
    ))
}

/// Either a tapped upstream body or a locally generated refusal.
pub enum ProxyBody<B> {
    Tapped(Box<TappedBody<B>>),
    Fixed(Option<Bytes>),
}

impl<B> http_body::Body for ProxyBody<B>
where
    B: http_body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display + Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, BoxError>>> {
        match &mut *self {
            Self::Tapped(inner) => std::pin::Pin::new(inner.as_mut())
                .poll_frame(cx)
                .map(|o| o.map(|r| r.map_err(Into::into))),
            Self::Fixed(bytes) => {
                std::task::Poll::Ready(bytes.take().map(|b| Ok(http_body::Frame::data(b))))
            }
        }
    }
}

/// A refusal the gateway generated itself.
///
/// No audit event: a request refused at admission never reached a model, so
/// there is no decision to record. The refusal is a metric and a log line, and
/// for `StaleConfigFailClosed` the customer's own 503 rate is the signal — a
/// fabricated event with `unknown` pins would be worse than none.
fn refuse<B>(status: StatusCode, message: &str) -> Response<ProxyBody<B>> {
    let body = format!(r#"{{"error":{{"message":{message:?},"type":"ancre_gateway"}}}}"#);
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(ProxyBody::Fixed(Some(Bytes::from(body))))
        .expect("a static refusal response is always well formed")
}

fn refuse_resolve<B>(e: &ResolveError) -> Response<ProxyBody<B>> {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    refuse(status, &e.to_string())
}

/// Which provider serves a model id.
///
/// Prefix matching, deliberately conservative: anything unrecognised goes to
/// the OpenAI wire format, which is also what Azure and vLLM speak.
fn provider_for(model_id: &str) -> ProviderKind {
    if model_id.starts_with("claude-") {
        ProviderKind::Anthropic
    } else {
        ProviderKind::OpenAi
    }
}

/// The requested model alias, read without a full parse where possible.
fn extract_model(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(ToString::to_string)
}

fn is_streaming(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_claude_models_to_anthropic() {
        assert_eq!(
            provider_for("claude-sonnet-4-5-20250929"),
            ProviderKind::Anthropic
        );
        assert_eq!(provider_for("gpt-4o"), ProviderKind::OpenAi);
        // Unrecognised ids take the OpenAI wire format, which is what Azure
        // and self-hosted vLLM speak too.
        assert_eq!(provider_for("llama-3.1-70b"), ProviderKind::OpenAi);
    }

    #[test]
    fn reads_the_model_alias_and_stream_flag() {
        let body = br#"{"model":"gpt-4o","stream":true,"messages":[]}"#;
        assert_eq!(extract_model(body).as_deref(), Some("gpt-4o"));
        assert!(is_streaming(body));

        let body = br#"{"model":"gpt-4o","messages":[]}"#;
        assert!(!is_streaming(body), "absent stream means non-streaming");
    }

    #[test]
    fn a_malformed_body_yields_no_alias_rather_than_panicking() {
        assert_eq!(extract_model(b"not json"), None);
        assert!(!is_streaming(b"not json"));
    }
}
