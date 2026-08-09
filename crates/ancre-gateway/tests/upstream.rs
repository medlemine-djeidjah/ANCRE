//! What actually goes out on the wire, against a real socket.
//!
//! Every other test in this workspace uses a fake upstream, which is right for
//! checking what lands in an audit event and useless for checking what the
//! provider receives. Two defects hid behind that fake all the way through M5:
//! the gateway sent no provider credential at all, and it posted Anthropic's
//! translated body to OpenAI's path. Both would have passed every existing
//! test and failed on the first real request a customer made.
//!
//! So this suite binds a socket, points `Endpoints` at it, and asserts on the
//! request that arrives.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use ancre_gateway::upstream::{Credentials, Endpoints, HttpsUpstream, Upstream};
use ancre_provider::{Provider, anthropic::Anthropic, openai::OpenAi};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;

static OPENAI: OpenAi = OpenAi;
static ANTHROPIC: Anthropic = Anthropic;

/// What the far end saw.
#[derive(Debug, Clone, Default)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A server that records one request and answers with an empty completion.
async fn record_one() -> (SocketAddr, Arc<Mutex<Option<Seen>>>) {
    let seen = Arc::new(Mutex::new(None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let captured = Arc::clone(&seen);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let captured = Arc::clone(&captured);
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                service_fn(move |req: Request<hyper::body::Incoming>| {
                    let captured = Arc::clone(&captured);
                    async move {
                        *captured.lock().unwrap() = Some(Seen {
                            path: req
                                .uri()
                                .path_and_query()
                                .map_or_else(String::new, ToString::to_string),
                            headers: req
                                .headers()
                                .iter()
                                .map(|(n, v)| {
                                    (n.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                                })
                                .collect(),
                        });
                        Ok::<_, std::convert::Infallible>(Response::new(http_body_util::Full::new(
                            Bytes::from_static(b"{}"),
                        )))
                    }
                }),
            )
            .await;
    });

    (addr, seen)
}

async fn send_through(
    provider: &'static dyn Provider,
    credentials: Credentials,
    ingress_path: &str,
) -> Seen {
    let (addr, seen) = record_one().await;
    let base = format!("http://{addr}");
    let endpoints = Endpoints {
        openai: base.clone(),
        anthropic: base,
    };

    let upstream = HttpsUpstream::new_with_credentials(endpoints, credentials).unwrap();

    let mut req = Request::new(Bytes::from_static(b"{}"));
    *req.uri_mut() = ingress_path.parse().unwrap();
    *req.method_mut() = hyper::Method::POST;
    // The caller's virtual key. It must not reach the provider.
    req.headers_mut().insert(
        hyper::header::AUTHORIZATION,
        hyper::header::HeaderValue::from_static("Bearer ancre-virtual-key"),
    );

    let response = upstream.send(provider, req).await.unwrap();
    let _ = response.into_body().collect().await;

    let s = seen.lock().unwrap().clone();
    s.expect("the upstream must have received a request")
}

#[tokio::test]
async fn openai_gets_a_bearer_token_and_the_path_it_was_given() {
    let seen = send_through(
        &OPENAI,
        Credentials {
            openai: Some("sk-deployment".into()),
            anthropic: None,
        },
        "/v1/chat/completions",
    )
    .await;

    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(seen.header("authorization"), Some("Bearer sk-deployment"));
}

/// The bug that would have 404'd every Anthropic request in production.
#[tokio::test]
async fn anthropic_gets_the_messages_path_its_own_key_header_and_a_version() {
    let seen = send_through(
        &ANTHROPIC,
        Credentials {
            openai: None,
            anthropic: Some("sk-ant-deployment".into()),
        },
        "/v1/chat/completions",
    )
    .await;

    assert_eq!(
        seen.path, "/v1/messages",
        "the OpenAI ingress path does not exist on Anthropic"
    );
    assert_eq!(seen.header("x-api-key"), Some("sk-ant-deployment"));
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
}

/// The caller's virtual key names a system in *our* registry and means nothing
/// upstream. Forwarding it would send a credential the customer issued to a
/// third party that has no business holding it.
#[tokio::test]
async fn the_callers_virtual_key_never_reaches_the_provider() {
    let seen = send_through(
        &ANTHROPIC,
        Credentials {
            openai: None,
            anthropic: Some("sk-ant-deployment".into()),
        },
        "/v1/chat/completions",
    )
    .await;

    assert_eq!(
        seen.header("authorization"),
        None,
        "the virtual key must be stripped, and Anthropic takes no bearer anyway"
    );

    let seen = send_through(
        &OPENAI,
        Credentials {
            openai: Some("sk-deployment".into()),
            anthropic: None,
        },
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer sk-deployment"),
        "replaced by the deployment's own credential, never appended to"
    );
}

/// A deployment with no credential — a mock, or a self-hosted model behind an
/// overridden base URL — sends none, rather than an empty one that reads as a
/// malformed credential in the provider's logs.
#[tokio::test]
async fn no_credential_means_no_header() {
    let seen = send_through(&OPENAI, Credentials::default(), "/v1/chat/completions").await;
    assert_eq!(seen.header("authorization"), None);

    let seen = send_through(&ANTHROPIC, Credentials::default(), "/v1/chat/completions").await;
    assert_eq!(seen.header("x-api-key"), None);
    assert_eq!(
        seen.header("anthropic-version"),
        Some("2023-06-01"),
        "the version header is a wire requirement, not a credential"
    );
}

/// Nothing may print a provider key, including a stray `{:?}` in a log line.
#[test]
fn credentials_do_not_appear_in_debug_output() {
    let creds = Credentials {
        openai: Some("sk-super-secret".into()),
        anthropic: Some("sk-ant-super-secret".into()),
    };
    let rendered = format!("{creds:?}");

    assert!(!rendered.contains("super-secret"), "{rendered}");
    assert!(rendered.contains("openai"), "{rendered}");
    assert!(rendered.contains("anthropic"), "{rendered}");
}
