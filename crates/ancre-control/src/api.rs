//! HTTP API. axum — ergonomics win here, this is not the hot path.
//!
//! The eval worker (Python, V1) talks to this over the same API a customer
//! would use. Dogfooding the public API is how it stays good (PRD §10).

use std::sync::Arc;

use ancre_chain::{ChainId, Checkpoint};
use ancre_types::SnapshotEnvelope;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::checkpointer::{ChainSource, CheckpointStore, PublicKeyRecord};
use crate::export::{ChainExport, PAGE, to_jsonl};
use crate::registry::ControlError;

/// What the API can read. A trait per concern rather than one god-object, so
/// the snapshot endpoints can be served by a control plane whose checkpointer
/// has not started yet — and so the tests can supply one without the other.
pub trait SnapshotApi: Send + Sync + 'static {
    /// The published snapshot, sealed. `current()` on `SnapshotBuilder`.
    fn current(
        &self,
    ) -> impl std::future::Future<Output = Result<SnapshotEnvelope, ControlError>> + Send;

    /// A prompt body by content hash, for `PromptRef::Lazy` resolution.
    fn prompt(
        &self,
        hash: &str,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, ControlError>> + Send;
}

/// Every public key that was ever valid, with its window.
pub trait KeyDirectory: Send + Sync + 'static {
    fn public_keys(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<PublicKeyRecord>, ControlError>> + Send;
}

pub struct ControlState<S, C, K, X> {
    pub snapshots: S,
    pub checkpoints: C,
    pub keys: K,
    /// The event store, for export. Held as `Arc` because the streaming body
    /// outlives the handler that created it — the response starts before the
    /// last page has been read.
    pub chains: std::sync::Arc<X>,
}

/// Everything the router needs that is not a datastore.
///
/// Separate from `ControlState` so the protected and open route groups can
/// share the datastores while only the protected group carries the credential
/// — an accidental `.layer()` on the wrong group is then a compile error
/// rather than a quietly public endpoint.
#[derive(Clone, Debug)]
pub struct Guard {
    pub admin: crate::auth::Admin,
}

impl<S, C, K, X> std::fmt::Debug for ControlState<S, C, K, X> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlState").finish_non_exhaustive()
    }
}

/// MVP surface, deliberately thin:
///   GET  /healthz
///   GET  /v1/snapshot                    — current spec + generation, for the poll backstop
///   GET  /v1/prompts/{hash}              — lazy prompt body fetch
///   GET  /v1/checkpoints/{tenant}/{system} — signed checkpoints for the verifier
///   GET  /v1/chains/{tenant}/{system}/events — the chain itself, as JSONL
///   GET  /v1/pubkeys                     — every public key that was ever valid, with windows
///
/// Registry CRUD, oversight, and evidence-pack export are V1.
///
/// Everything here is a read. The registry is edited out of band in the MVP —
/// a write API that can change what a customer's pins mean needs authz and an
/// audit trail of its own, and half of one is worse than none.
pub fn router<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    state: Arc<ControlState<S, C, K, X>>,
    guard: Guard,
) -> Router {
    // Content. Everything here describes how a customer runs their AI, and
    // none of it is needed to *verify* evidence somebody was already handed.
    let protected = Router::new()
        .route("/v1/snapshot", get(snapshot::<S, C, K, X>))
        .route("/v1/prompts/{hash}", get(prompt::<S, C, K, X>))
        .route("/v1/chains", get(chain_list::<S, C, K, X>))
        .route(
            "/v1/chains/{tenant}/{system}/summary",
            get(chain_summary::<S, C, K, X>),
        )
        .route(
            "/v1/chains/{tenant}/{system}/events",
            get(events::<S, C, K, X>),
        )
        .layer(axum::middleware::from_fn_with_state(
            guard.admin.clone(),
            crate::auth::require_admin,
        ))
        .with_state(Arc::clone(&state));

    // Attestation, and liveness. A checkpoint is a signature over a root hash
    // and a public key is a public key: neither reveals a customer's traffic,
    // and an auditor who must obtain a credential before checking a signature
    // is an auditor who checks fewer signatures.
    let open = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/v1/checkpoints/{tenant}/{system}",
            get(checkpoints::<S, C, K, X>),
        )
        .route("/v1/pubkeys", get(pubkeys::<S, C, K, X>))
        .with_state(state);

    // Session establishment has to be reachable without a session.
    let session = Router::new()
        .route(
            "/api/session",
            get(session_status)
                .post(session_create)
                .delete(session_delete),
        )
        .with_state(guard);

    open.merge(protected)
        .merge(session)
        .merge(crate::ui::router())
}

#[derive(Debug, serde::Deserialize)]
pub struct SessionRequest {
    pub token: String,
}

/// Whether this request carries a usable session.
///
/// The dashboard calls it on load to decide between the login screen and the
/// application. It is deliberately not protected — an unauthenticated caller
/// gets `{"authenticated": false}` rather than a 401, because a 401 here would
/// make the login page itself look like an error.
async fn session_status(
    State(guard): State<Guard>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    let ok = crate::auth::presented_token(&headers).is_some_and(|t| guard.admin.admits(&t));
    Json(serde_json::json!({ "authenticated": ok }))
}

/// Exchange the operator token for a session cookie.
async fn session_create(State(guard): State<Guard>, Json(body): Json<SessionRequest>) -> Response {
    if guard.admin.admits(body.token.trim()) {
        return (
            StatusCode::NO_CONTENT,
            [(axum::http::header::SET_COOKIE, guard.admin.session_cookie())],
        )
            .into_response();
    }

    // No detail about *why*. "Wrong token" and "no such user" are the same
    // answer here, and a login endpoint that distinguishes them is an oracle.
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        r#"{"error":"that token was not accepted"}"#,
    )
        .into_response()
}

async fn session_delete() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(
            axum::http::header::SET_COOKIE,
            crate::auth::Admin::cleared_cookie(),
        )],
    )
        .into_response()
}

/// Every chain the store has seen, with its head.
async fn chain_list<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    State(state): State<Arc<ControlState<S, C, K, X>>>,
) -> Result<Json<Vec<crate::overview::ChainListing>>, ApiError> {
    Ok(Json(state.chains.listings().await?))
}

/// The counted shape of one chain.
///
/// **Not a verification.** Nothing here re-hashes anything; it is the same
/// server that serves the events telling you what it thinks is in them. The
/// dashboard says so in as many words, and points at the evidence pack.
async fn chain_summary<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    State(state): State<Arc<ControlState<S, C, K, X>>>,
    Path((tenant_id, system_id)): Path<(String, String)>,
) -> Result<Json<crate::overview::ChainSummary>, ApiError> {
    let chain = ChainId {
        tenant_id,
        system_id,
    };
    Ok(Json(state.chains.summary(&chain).await?))
}

/// What the export endpoint needs: the head, so a caller can omit the range,
/// and the events themselves.
pub trait Chains: ChainSource + ChainExport + crate::overview::ChainOverview + 'static {}
impl<T: ChainSource + ChainExport + crate::overview::ChainOverview + 'static> Chains for T {}

/// `?from=` and `?to=`, both optional and both inclusive.
#[derive(Debug, serde::Deserialize)]
pub struct Range {
    pub from: Option<u64>,
    pub to: Option<u64>,
}

async fn snapshot<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    State(state): State<Arc<ControlState<S, C, K, X>>>,
) -> Result<Json<SnapshotEnvelope>, ApiError> {
    Ok(Json(state.snapshots.current().await?))
}

async fn prompt<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    State(state): State<Arc<ControlState<S, C, K, X>>>,
    Path(hash): Path<String>,
) -> Result<Vec<u8>, ApiError> {
    state
        .snapshots
        .prompt(&hash)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Signed checkpoints for one chain, oldest first.
///
/// Unauthenticated on purpose: a checkpoint is a signature over a root hash.
/// It reveals no prompt, no completion and no subject, and an auditor who has
/// to obtain a credential before verifying is an auditor who verifies less.
async fn checkpoints<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    State(state): State<Arc<ControlState<S, C, K, X>>>,
    Path((tenant_id, system_id)): Path<(String, String)>,
) -> Result<Json<Vec<Checkpoint>>, ApiError> {
    let chain = ChainId {
        tenant_id,
        system_id,
    };
    Ok(Json(state.checkpoints.list(&chain).await?))
}

/// The chain itself, newline-delimited, streamed.
///
/// Unauthenticated for the same reason the checkpoints are — with one more
/// consideration that cuts the other way, and is worth being explicit about:
/// unlike a checkpoint, **this is the content**. Digests rather than prompts
/// and completions, so no payload leaks, but `system_id`, timings, token
/// counts and model versions are a competitive picture of how a customer runs
/// their AI. Deployments that care will put this behind their own gateway.
/// Authz for the read API is V1, and it is listed in `docs/deferred.md` rather
/// than half-built here.
///
/// Streamed page by page: the response starts before the chain has been read,
/// and resident memory is bounded by `PAGE` rather than by chain length. A
/// failure part-way through **truncates the body** — there is no way to change
/// a status code that has already been sent. The verifier's answer to a
/// truncated chain is the right one anyway: it reports the range it actually
/// saw, so a short export reads as a short export and not as a clean chain.
async fn events<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    State(state): State<Arc<ControlState<S, C, K, X>>>,
    Path((tenant_id, system_id)): Path<(String, String)>,
    axum::extract::Query(range): axum::extract::Query<Range>,
) -> Result<Response, ApiError> {
    let chain = ChainId {
        tenant_id,
        system_id,
    };

    // The head is read once, up front. Paging to a moving target would let the
    // export chase a chain that is still being written and never finish; a
    // fixed upper bound means the export is a snapshot of a prefix, which is
    // exactly what a checkpoint attests anyway.
    let head = state.chains.head_seq(&chain).await?.unwrap_or(0);
    let from = range.from.unwrap_or(1).max(1);
    let to = range.to.unwrap_or(head).min(head);

    // The filename is built before the chain moves into the stream, which owns
    // it for as long as the response body is being written.
    let filename = format!(
        "{}-{}-{from}-{to}.jsonl",
        chain_token(&chain.tenant_id),
        chain_token(&chain.system_id)
    );

    let chains = Arc::clone(&state.chains);
    let stream = futures_util::stream::try_unfold(from, move |next| {
        let chains = Arc::clone(&chains);
        let chain = chain.clone();
        async move {
            if next > to {
                return Ok::<_, ControlError>(None);
            }
            let last = next.saturating_add(PAGE - 1).min(to);
            let events = chains.events(&chain, next, last).await?;
            // A page that comes back empty inside a range that should hold
            // events means the store lost rows under us. Stopping here is what
            // makes that a visibly short export rather than an infinite loop.
            if events.is_empty() {
                return Ok(None);
            }
            let bytes = to_jsonl(&events)?;
            Ok(Some((axum::body::Bytes::from(bytes), last + 1)))
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        // The registered type for newline-delimited JSON. Browsers download it
        // rather than trying to render a million-line document.
        .header("content-type", "application/x-ndjson")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}\""),
        )
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| ApiError::Control(ControlError::Db(e.to_string())))
}

/// A filename is not a security boundary, but it is a place a `/` or a quote
/// ends up in a header. Keep it to characters that cannot restructure one.
fn chain_token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// **Every** key, not the current one. Rotation must not invalidate old
/// checkpoints, so a verifier handed a two-year-old range needs the key that
/// signed it and the window it was valid in.
async fn pubkeys<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory, X: Chains>(
    State(state): State<Arc<ControlState<S, C, K, X>>>,
) -> Result<Json<Vec<PublicKeyRecord>>, ApiError> {
    Ok(Json(state.keys.public_keys().await?))
}

#[derive(Debug)]
enum ApiError {
    NotFound,
    Control(ControlError),
}

impl From<ControlError> for ApiError {
    fn from(e: ControlError) -> Self {
        Self::Control(e)
    }
}

impl IntoResponse for ApiError {
    /// A control plane that cannot build a snapshot returns **503**, not 500.
    /// The gateway's poll backstop retries on 503 and keeps serving its
    /// current snapshot until the staleness budget runs out; a 500 reads as
    /// "this request was wrong", which it was not.
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            Self::Control(e) => {
                tracing::warn!(error = %e, "control plane read failed");
                (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpointer::Checkpointer;
    use crate::checkpointer::testing::{MemoryChain, MemoryCheckpoints};
    use ancre_chain::CheckpointSigner;
    use ancre_types::Timestamp;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct Snapshots(std::sync::Mutex<Option<SnapshotEnvelope>>);

    impl SnapshotApi for Snapshots {
        async fn current(&self) -> Result<SnapshotEnvelope, ControlError> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| ControlError::Invalid("nothing published yet".into()))
        }

        async fn prompt(&self, hash: &str) -> Result<Option<Vec<u8>>, ControlError> {
            Ok((hash == "b3:9f2c").then(|| b"screen this CV".to_vec()))
        }
    }

    struct Keys;

    impl KeyDirectory for Keys {
        async fn public_keys(&self) -> Result<Vec<PublicKeyRecord>, ControlError> {
            Ok(vec![
                PublicKeyRecord {
                    key_id: "cp-1".into(),
                    public_key: "aa".repeat(32),
                    valid_from: Timestamp::from_micros(0),
                    valid_to: Some(Timestamp::from_micros(1_000)),
                },
                PublicKeyRecord {
                    key_id: "cp-2".into(),
                    public_key: "bb".repeat(32),
                    valid_from: Timestamp::from_micros(1_000),
                    valid_to: None,
                },
            ])
        }
    }

    fn envelope() -> SnapshotEnvelope {
        let spec = ancre_resolver::testing::spec(41);
        let hash = ancre_canon::content_hash(&spec.content_view()).unwrap();
        SnapshotEnvelope::seal(spec, hash)
    }

    async fn app(published: Option<SnapshotEnvelope>) -> Router {
        let source = MemoryChain::default();
        let chain = ChainId {
            tenant_id: "acme".into(),
            system_id: "hr-screening".into(),
        };
        source.append(&chain, 20);
        let cp = Checkpointer::new(
            Arc::new(source),
            MemoryCheckpoints::default(),
            CheckpointSigner::from_bytes([3u8; 32], "cp-2".into()),
        );
        cp.tick(Timestamp::from_micros(1_754_400_000_000_000))
            .await
            .unwrap();
        let (source, store, _) = cp.into_parts();

        router(
            Arc::new(ControlState {
                snapshots: Snapshots(std::sync::Mutex::new(published)),
                checkpoints: store,
                keys: Keys,
                chains: source,
            }),
            Guard {
                admin: crate::auth::Admin::new(TEST_TOKEN),
            },
        )
    }

    const TEST_TOKEN: &str = "test-operator-token";

    /// Authenticated by default: every existing test in this module is about
    /// what a handler returns, not about who may call it. The auth behaviour
    /// has tests of its own below.
    async fn get_body(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
        get_body_as(app, uri, Some(TEST_TOKEN)).await
    }

    async fn get_body_as(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Vec<u8>) {
        let mut req = axum::http::Request::builder().uri(uri);
        if let Some(t) = token {
            req = req.header(axum::http::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let res = app
            .clone()
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, body)
    }

    /// The split this whole module exists to enforce. A regression here is
    /// silent — the endpoint keeps working, it simply works for everyone — so
    /// it is asserted endpoint by endpoint rather than trusted to a layer.
    #[tokio::test]
    async fn the_content_endpoints_refuse_an_unauthenticated_caller() {
        let app = app(Some(envelope())).await;

        for uri in [
            "/v1/snapshot",
            "/v1/prompts/b3:9f2c",
            "/v1/chains",
            "/v1/chains/acme/hr-screening/summary",
            "/v1/chains/acme/hr-screening/events",
        ] {
            let (status, _) = get_body_as(&app, uri, None).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{uri} served a customer's data to nobody in particular"
            );
        }
    }

    /// And the other half, which matters just as much: an auditor checking a
    /// signature they were handed must not need a credential first.
    #[tokio::test]
    async fn the_attestation_endpoints_stay_open() {
        let app = app(Some(envelope())).await;

        for uri in [
            "/healthz",
            "/v1/pubkeys",
            "/v1/checkpoints/acme/hr-screening",
        ] {
            let (status, _) = get_body_as(&app, uri, None).await;
            assert_eq!(status, StatusCode::OK, "{uri} should need no credential");
        }
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused() {
        let app = app(Some(envelope())).await;
        let (status, _) = get_body_as(&app, "/v1/chains", Some("not-the-token")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_session_cookie_is_accepted_in_place_of_a_bearer() {
        let app = app(Some(envelope())).await;

        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/session")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "token": TEST_TOKEN }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let cookie = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("a successful login must set a cookie")
            .to_string();

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/chains")
                    .header(
                        axum::http::header::COOKIE,
                        cookie.split(';').next().unwrap(),
                    )
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_bad_login_says_nothing_useful_about_why() {
        let app = app(None).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/session")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "token": "wrong" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(
            res.headers().get(axum::http::header::SET_COOKIE).is_none(),
            "a refused login must not hand out a session"
        );
    }

    #[tokio::test]
    async fn the_chain_list_and_summary_agree_with_the_chain() {
        let app = app(None).await;

        let (status, body) = get_body(&app, "/v1/chains").await;
        assert_eq!(status, StatusCode::OK);
        let listings: Vec<crate::overview::ChainListing> = serde_json::from_slice(&body).unwrap();
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].system_id, "hr-screening");
        assert_eq!(listings[0].head_seq, 20);

        let (status, body) = get_body(&app, "/v1/chains/acme/hr-screening/summary").await;
        assert_eq!(status, StatusCode::OK);
        let summary: crate::overview::ChainSummary = serde_json::from_slice(&body).unwrap();
        assert_eq!(summary.event_count, 20);
        assert_eq!(summary.head_seq, 20);
        assert!(summary.first_event_at.is_some());
    }

    /// An empty chain must report absence, not the epoch. A dashboard that
    /// renders 1970 for a system with no traffic reads as a broken chain.
    #[tokio::test]
    async fn an_unknown_chain_summarises_as_empty_rather_than_failing() {
        let app = app(None).await;
        let (status, body) = get_body(&app, "/v1/chains/acme/nothing-here/summary").await;

        assert_eq!(status, StatusCode::OK);
        let summary: crate::overview::ChainSummary = serde_json::from_slice(&body).unwrap();
        assert_eq!(summary.event_count, 0);
        assert_eq!(summary.first_event_at, None);
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let app = app(None).await;
        assert_eq!(get_body(&app, "/healthz").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_snapshot_endpoint_serves_a_verifiable_envelope() {
        let app = app(Some(envelope())).await;
        let (status, body) = get_body(&app, "/v1/snapshot").await;

        assert_eq!(status, StatusCode::OK);
        let env: SnapshotEnvelope = serde_json::from_slice(&body).unwrap();
        assert_eq!(env.generation, 41);
        assert!(
            env.verify().is_ok(),
            "what the poll backstop receives must verify without asking the sender"
        );
    }

    /// The poll backstop must be able to tell "not ready" from "you asked
    /// wrong", or a gateway will keep its stale snapshot for the wrong reason.
    #[tokio::test]
    async fn a_control_plane_with_nothing_published_returns_503() {
        let app = app(None).await;
        assert_eq!(
            get_body(&app, "/v1/snapshot").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn a_prompt_body_is_served_by_hash_and_404s_when_unknown() {
        let app = app(None).await;
        let (status, body) = get_body(&app, "/v1/prompts/b3:9f2c").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"screen this CV");

        assert_eq!(
            get_body(&app, "/v1/prompts/b3:nope").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn checkpoints_are_served_per_chain_and_verify_offline() {
        let app = app(None).await;
        let (status, body) = get_body(&app, "/v1/checkpoints/acme/hr-screening").await;
        assert_eq!(status, StatusCode::OK);

        let cps: Vec<Checkpoint> = serde_json::from_slice(&body).unwrap();
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].body.seq_to, 20);

        let key = CheckpointSigner::from_bytes([3u8; 32], "cp-2".into()).verifying_key();
        assert!(ancre_chain::verify_checkpoint(&cps[0], &key).is_ok());
    }

    #[tokio::test]
    async fn an_unknown_chain_returns_an_empty_list_rather_than_an_error() {
        let app = app(None).await;
        let (status, body) = get_body(&app, "/v1/checkpoints/acme/no-such-system").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Vec<Checkpoint>>(&body)
                .unwrap()
                .len(),
            0
        );
    }

    /// The export carries retired keys too. A verifier handed a range signed
    /// under a rotated-out key has to be able to check it.
    #[tokio::test]
    async fn the_pubkey_export_includes_retired_keys_with_their_windows() {
        let app = app(None).await;
        let (status, body) = get_body(&app, "/v1/pubkeys").await;
        assert_eq!(status, StatusCode::OK);

        let keys: Vec<PublicKeyRecord> = serde_json::from_slice(&body).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(
            keys[0].valid_to.is_some(),
            "the retired key keeps its window"
        );
        assert!(keys[1].valid_to.is_none(), "exactly one active key");
    }

    /// The endpoint the whole product ends at: a chain an auditor can pipe
    /// straight into `ancre-verify`.
    #[tokio::test]
    async fn the_chain_exports_as_jsonl_that_verifies() {
        let app = app(None).await;
        let (status, body) = get_body(&app, "/v1/chains/acme/hr-screening/events").await;
        assert_eq!(status, StatusCode::OK);

        // Parsed exactly the way the verifier parses it — one JSON event per
        // line, nothing else in the body.
        let events: Vec<ancre_types::AuditEvent> = String::from_utf8(body)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line must parse"))
            .collect();
        assert_eq!(events.len(), 20);

        let report = ancre_chain::verify_range(events, ancre_canon::GENESIS);
        assert!(
            report.is_clean(),
            "an exported chain must verify: {:?}",
            report.violations
        );
        assert_eq!(report.seq_to, 20);
    }

    #[tokio::test]
    async fn an_export_range_is_inclusive_at_both_ends() {
        let app = app(None).await;
        let (status, body) =
            get_body(&app, "/v1/chains/acme/hr-screening/events?from=5&to=9").await;
        assert_eq!(status, StatusCode::OK);

        let lines: Vec<_> = String::from_utf8(body)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 5);

        let first: ancre_types::AuditEvent = serde_json::from_str(&lines[0]).unwrap();
        let last: ancre_types::AuditEvent = serde_json::from_str(&lines[4]).unwrap();
        assert_eq!(first.seq, 5);
        assert_eq!(last.seq, 9);
    }

    /// A chain nobody has written to is an empty export, not a 404. "No events
    /// yet" and "no such system" look the same from here, and inventing a
    /// distinction the store cannot support would be a lie either way.
    #[tokio::test]
    async fn an_unknown_chain_exports_nothing_rather_than_failing() {
        let app = app(None).await;
        let (status, body) = get_body(&app, "/v1/chains/acme/no-such-system/events").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty());
    }

    /// A range past the head is clamped rather than padded or refused: the
    /// export is a prefix of the chain, and asking for more than exists is a
    /// reasonable thing for a script to do.
    #[tokio::test]
    async fn a_range_past_the_head_stops_at_the_head() {
        let app = app(None).await;
        let (status, body) =
            get_body(&app, "/v1/chains/acme/hr-screening/events?from=18&to=9999").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(String::from_utf8(body).unwrap().lines().count(), 3);
    }
}
