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

use crate::checkpointer::{CheckpointStore, PublicKeyRecord};
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

pub struct ControlState<S, C, K> {
    pub snapshots: S,
    pub checkpoints: C,
    pub keys: K,
}

impl<S, C, K> std::fmt::Debug for ControlState<S, C, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlState").finish_non_exhaustive()
    }
}

/// MVP surface, deliberately thin:
///   GET  /healthz
///   GET  /v1/snapshot                    — current spec + generation, for the poll backstop
///   GET  /v1/prompts/{hash}              — lazy prompt body fetch
///   GET  /v1/checkpoints/{tenant}/{system} — signed checkpoints for the verifier
///   GET  /v1/pubkeys                     — every public key that was ever valid, with windows
///
/// Registry CRUD, oversight, and evidence-pack export are V1.
///
/// Everything here is a read. The registry is edited out of band in the MVP —
/// a write API that can change what a customer's pins mean needs authz and an
/// audit trail of its own, and half of one is worse than none.
pub fn router<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory>(
    state: Arc<ControlState<S, C, K>>,
) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/snapshot", get(snapshot::<S, C, K>))
        .route("/v1/prompts/{hash}", get(prompt::<S, C, K>))
        .route(
            "/v1/checkpoints/{tenant}/{system}",
            get(checkpoints::<S, C, K>),
        )
        .route("/v1/pubkeys", get(pubkeys::<S, C, K>))
        .with_state(state)
}

async fn snapshot<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory>(
    State(state): State<Arc<ControlState<S, C, K>>>,
) -> Result<Json<SnapshotEnvelope>, ApiError> {
    Ok(Json(state.snapshots.current().await?))
}

async fn prompt<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory>(
    State(state): State<Arc<ControlState<S, C, K>>>,
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
async fn checkpoints<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory>(
    State(state): State<Arc<ControlState<S, C, K>>>,
    Path((tenant_id, system_id)): Path<(String, String)>,
) -> Result<Json<Vec<Checkpoint>>, ApiError> {
    let chain = ChainId {
        tenant_id,
        system_id,
    };
    Ok(Json(state.checkpoints.list(&chain).await?))
}

/// **Every** key, not the current one. Rotation must not invalidate old
/// checkpoints, so a verifier handed a two-year-old range needs the key that
/// signed it and the window it was valid in.
async fn pubkeys<S: SnapshotApi, C: CheckpointStore + 'static, K: KeyDirectory>(
    State(state): State<Arc<ControlState<S, C, K>>>,
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
            source,
            MemoryCheckpoints::default(),
            CheckpointSigner::from_bytes([3u8; 32], "cp-2".into()),
        );
        cp.tick(Timestamp::from_micros(1_754_400_000_000_000))
            .await
            .unwrap();
        let (_, store, _) = cp.into_parts();

        router(Arc::new(ControlState {
            snapshots: Snapshots(std::sync::Mutex::new(published)),
            checkpoints: store,
            keys: Keys,
        }))
    }

    async fn get_body(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, body)
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
}
