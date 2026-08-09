//! A provider that answers like OpenAI, so the quickstart needs nobody's key.
//!
//! This exists for exactly one reason: **the demo must not depend on a
//! credential the person running it does not have.** A quickstart whose first
//! step is "get an OpenAI key and put $5 on it" is a quickstart most people
//! stop reading, and the thing being demonstrated — a verifiable chain — has
//! nothing to do with which model actually answered.
//!
//! It is an *example*, never a binary, and it is only ever built into the
//! `demo` compose profile. Nothing on the request path imports it.
//!
//! What it deliberately does *not* do is fake a pin. The model id it returns
//! is the one the gateway will record:
//!
//! - a request for `gpt-4o` is answered as `gpt-4o-2024-08-06`, a dated id, so
//!   the chain shows a clean pin;
//! - a request for anything else is answered with that id unchanged, so a
//!   floating alias stays floating and the event carries
//!   `RiskFlag::UnpinnedModel`.
//!
//! The second case is the more interesting half of the demo. An evidence
//! system that only ever shows clean rows has not been shown to work.
//!
//! ```sh
//! cargo run -p ancre-gateway --example mock-provider   # listens on :9090
//! ```

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;

/// The one alias this mock knows how to resolve to specific weights. Real
/// providers publish a table like this; a mock with an empty one would make
/// every demo request look unpinned and hide the distinction that matters.
const PINNED: &[(&str, &str)] = &[
    ("gpt-4o", "gpt-4o-2024-08-06"),
    ("gpt-4o-mini", "gpt-4o-mini-2024-07-18"),
    ("claude-sonnet-4-5", "claude-sonnet-4-5-20250929"),
];

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = std::env::var("MOCK_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:9090".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("mock provider listening on {addr}");

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service_fn(handle))
                .with_upgrades()
                .await;
        });
    }
}

async fn handle(req: Request<hyper::body::Incoming>) -> Result<Response<BoxBody>, Infallible> {
    let path = req.uri().path().to_string();
    let body = req
        .into_body()
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();

    // Anthropic's path, which the gateway must have rewritten on the way here.
    // Serving it with the Anthropic response shape is what makes the demo
    // prove the rewrite: if the gateway posted an Anthropic body to
    // `/v1/chat/completions` — the bug that shipped through all of M5 — this
    // arm would never be reached and the OpenAI arm would fail to parse it.
    if path.ends_with("/v1/messages") {
        return Ok(anthropic(&body));
    }

    if !path.ends_with("/chat/completions") {
        return Ok(json(
            404,
            r#"{"error":{"message":"mock provider serves /v1/chat/completions and /v1/messages","type":"mock"}}"#
                .to_string(),
        ));
    }

    let requested = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("model")?.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "gpt-4o".to_string());
    let served = PINNED
        .iter()
        .find(|(alias, _)| *alias == requested)
        .map_or(requested, |(_, pinned)| (*pinned).to_string());

    let streaming = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false);

    Ok(if streaming {
        sse(&served)
    } else {
        json(200, completion(&served))
    })
}

/// Anthropic's response shape, which is a different document from OpenAI's —
/// different usage field names, content as a list of blocks. The adapter reads
/// `model` and `usage.{input,output}_tokens` from it, so a demo that answered
/// with the OpenAI shape here would record `unknown` pins and look like a bug
/// in the provider rather than in the mock.
///
/// Streaming is not implemented: Anthropic's SSE is its own event vocabulary,
/// and a half-right imitation would teach the demo's reader something false.
/// A streamed request gets an honest 400.
fn anthropic(body: &[u8]) -> Response<BoxBody> {
    let requested = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model")?.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "claude-sonnet-4-5".to_string());

    if serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
    {
        return json(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"the mock provider does not imitate Anthropic's streaming vocabulary"}}"#
                .to_string(),
        );
    }

    let served = PINNED
        .iter()
        .find(|(alias, _)| *alias == requested)
        .map_or(requested, |(_, pinned)| (*pinned).to_string());

    json(
        200,
        format!(
            r#"{{"id":"msg_mock","type":"message","role":"assistant","model":"{served}","content":[{{"type":"text","text":"This candidate's CV lists the two certifications the role requires."}}],"stop_reason":"end_turn","usage":{{"input_tokens":214,"output_tokens":18}}}}"#
        ),
    )
}

fn completion(model: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-mock","object":"chat.completion","created":1730000000,"model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":"This candidate's CV lists the two certifications the role requires."}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":214,"completion_tokens":18,"total_tokens":232}}}}"#
    )
}

/// A real SSE response: separate frames, with a gap between them.
///
/// The gap is the point. The gateway's claim is that the first token is
/// forwarded before the second is read, and a mock that writes the whole
/// stream in one syscall cannot tell a passthrough from a buffer — both look
/// identical to the client. Ten milliseconds is enough to make the difference
/// visible to `curl -N`.
fn sse(model: &str) -> Response<BoxBody> {
    let chunks: Vec<String> = ["This candidate's CV lists ", "the two certifications ", "the role requires."]
        .iter()
        .map(|text| {
            format!(
                r#"data: {{"id":"chatcmpl-mock","object":"chat.completion.chunk","model":"{model}","choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}

"#
            )
        })
        .chain(std::iter::once(format!(
            r#"data: {{"id":"chatcmpl-mock","object":"chat.completion.chunk","model":"{model}","choices":[],"usage":{{"prompt_tokens":214,"completion_tokens":18}}}}

"#
        )))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();

    let stream = futures_util::stream::unfold(chunks.into_iter(), |mut it| async move {
        let next = it.next()?;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        Some((Ok::<_, Infallible>(Frame::data(Bytes::from(next))), it))
    });

    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(BoxBody::new(StreamBody::new(stream)))
        .expect("a static SSE response is always well formed")
}

fn json(status: u16, body: String) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(BoxBody::new(Full::new(Bytes::from(body))))
        .expect("a static JSON response is always well formed")
}
