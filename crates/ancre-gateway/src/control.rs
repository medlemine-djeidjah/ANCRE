//! How a gateway node learns its configuration.
//!
//! Two paths, and the asymmetry between them is the design:
//!
//! - **The bus is the fast path.** A JetStream consumer on `ANCRE_CONFIG`
//!   starting at the last message, so a node that connects an hour after the
//!   last publish installs the current generation immediately and every later
//!   one within milliseconds.
//! - **The poll is the backstop.** `GET /v1/snapshot` every 10 seconds,
//!   because a missed message must never mean indefinite staleness (PRD §8).
//!
//! Neither is trusted. Both hand a `SnapshotEnvelope` to `ConfigFeed::apply`,
//! which recomputes the content hash before anything is installed — the bus is
//! not part of the trust boundary, and neither is an HTTP response.
//!
//! A gateway with a dead bus and a live control plane converges in one poll
//! interval. A gateway with both dead keeps serving what it has until the
//! staleness budget runs out and then fails closed for High risk, which is the
//! behaviour a customer's technical file describes.

use std::sync::Arc;

use ancre_types::SnapshotEnvelope;
use async_nats::jetstream;
use bytes::Bytes;
use http_body_util::BodyExt as _;

use crate::config_feed::{ConfigFeed, FeedError, SnapshotSource};

/// The control plane's snapshot stream. Must match `ancre-control`.
pub const CONFIG_STREAM: &str = "ANCRE_CONFIG";

/// `GET /v1/snapshot` against the control plane.
#[derive(Clone)]
pub struct HttpSnapshotSource {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<Bytes>,
    >,
    url: String,
    /// The control plane's credential.
    ///
    /// `/v1/snapshot` is protected, and it is the most sensitive read in the
    /// system — every system, every route, every pinned model version, and the
    /// hash of every key. The gateway is a first-class client of it, so it
    /// carries a credential like any other client rather than the endpoint
    /// being left open for its convenience.
    token: Option<String>,
}

impl std::fmt::Debug for HttpSnapshotSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSnapshotSource")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl HttpSnapshotSource {
    /// `base` is the control plane's root, e.g. `http://control:8081`.
    ///
    /// Plain HTTP is allowed and expected: the control plane usually sits on a
    /// private network, and the envelope carries its own content hash, so the
    /// transport is not what makes an installed snapshot trustworthy. TLS is
    /// available by pointing this at an `https://` URL.
    ///
    /// # Errors
    /// If no process-wide rustls crypto provider can be installed.
    pub fn new(base: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_all_versions()
            .build();

        Ok(Self {
            client: hyper_util::client::legacy::Client::builder(
                hyper_util::rt::TokioExecutor::new(),
            )
            .build(connector),
            url: format!("{}/v1/snapshot", base.trim_end_matches('/')),
            token: None,
        })
    }

    /// The bearer token presented to the control plane.
    ///
    /// `None` is legitimate only against a control plane with no credential
    /// configured. Anywhere else it produces a 401 at cold start, and the
    /// gateway refuses to bind rather than serving traffic it cannot pin —
    /// which is the correct failure, and the reason the error names the
    /// variable to set.
    #[must_use]
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|t| !t.is_empty());
        self
    }
}

impl SnapshotSource for HttpSnapshotSource {
    /// A non-200 is `Unreachable`, not `Refused`.
    ///
    /// The distinction is load-bearing. The control plane answers **503** when
    /// it cannot build a snapshot — nothing published yet, registry
    /// unreachable — and that is a condition to wait through, not a snapshot to
    /// reject. `Refused` is reserved for an envelope that arrived and did not
    /// describe its own contents, which is the only case where the sender is
    /// the problem.
    async fn fetch(&self) -> Result<SnapshotEnvelope, FeedError> {
        let mut req = hyper::Request::builder()
            .uri(&self.url)
            .header(hyper::header::ACCEPT, "application/json");
        if let Some(token) = &self.token {
            req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let req = req
            .body(http_body_util::Full::new(Bytes::new()))
            .map_err(|e| FeedError::Unreachable(e.to_string()))?;

        let response = self
            .client
            .request(req)
            .await
            .map_err(|e| FeedError::Unreachable(e.to_string()))?;

        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| FeedError::Unreachable(e.to_string()))?
            .to_bytes();

        if status == hyper::StatusCode::UNAUTHORIZED {
            // Named explicitly, because the generic form of this message sends
            // an operator to look at the control plane's health when the
            // problem is one unset variable on this node.
            return Err(FeedError::Unreachable(
                "control plane refused this node's credential. Set \
                 ANCRE_CONTROL_TOKEN to the control plane's ANCRE_ADMIN_TOKEN"
                    .into(),
            ));
        }
        if !status.is_success() {
            return Err(FeedError::Unreachable(format!(
                "control plane returned {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )));
        }

        serde_json::from_slice(&body).map_err(|e| FeedError::Unreachable(format!("decoding: {e}")))
    }
}

/// Watch the bus and apply every generation that lands on it.
///
/// `DeliverPolicy::Last` is what makes a late start work: the consumer is
/// handed the newest retained message on connect, so a node that missed the
/// publish does not wait for the next one.
///
/// The consumer is **ephemeral and unfiltered by node**. A durable one would
/// track per-node delivery state the gateway has no use for — configuration is
/// state to converge on, not work to complete, and a node that has the current
/// generation has nothing to catch up on.
///
/// Failures here are warnings, never fatal: this is the fast path, and the
/// poll backstop is what guarantees convergence. A subscription that dies is
/// re-established on the next loop.
pub async fn watch<S: SnapshotSource>(
    url: String,
    feed: Arc<ConfigFeed<S>>,
    shutdown: impl Future<Output = ()> + Send,
) {
    use futures_util::StreamExt as _;

    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let messages = match subscribe(&url).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "snapshot subscription failed; the poll backstop still converges"
                );
                tokio::select! {
                    () = &mut shutdown => return,
                    () = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                }
            }
        };
        tokio::pin!(messages);

        loop {
            let message = tokio::select! {
                () = &mut shutdown => return,
                next = messages.next() => match next {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "snapshot stream error; resubscribing");
                        break;
                    }
                    None => break,
                },
            };

            // Acked before it is applied, and deliberately. The message is
            // configuration, not work: if applying it fails the right answer is
            // the next publish or the next poll, never a redelivery of the same
            // bytes that just failed to verify.
            let _ = message.ack().await;

            match serde_json::from_slice::<SnapshotEnvelope>(&message.payload) {
                Ok(envelope) => apply(&feed, envelope),
                Err(e) => tracing::warn!(error = %e, "undecodable snapshot on the bus"),
            }
        }
    }
}

fn apply<S: SnapshotSource>(feed: &ConfigFeed<S>, envelope: SnapshotEnvelope) {
    let generation = envelope.generation;
    match feed.apply(envelope) {
        Ok(applied) => tracing::info!(
            generation = applied.generation,
            propagation_ms = applied.outcome.propagation_ms,
            substantial_candidates = applied.outcome.substantial_candidates().len(),
            source = "bus",
            "configuration generation applied",
        ),
        // Refused, and the previous snapshot stays installed. Anyone who can
        // reach the bus can put bytes on it; a snapshot that does not describe
        // its own contents is exactly what must not reach the request path.
        Err(e) => tracing::error!(
            error = %e,
            generation,
            "snapshot refused; keeping the installed generation"
        ),
    }
}

/// The concrete stream type is the client's own; naming it here rather than
/// boxing keeps the per-message path allocation-free, which costs nothing to
/// preserve and would be annoying to recover.
type Messages = jetstream::consumer::pull::Stream;

async fn subscribe(url: &str) -> Result<Messages, async_nats::Error> {
    let client = async_nats::connect(url).await?;
    let context = jetstream::new(client);
    let stream = context.get_stream(CONFIG_STREAM).await?;
    let consumer = stream
        .create_consumer(jetstream::consumer::pull::Config {
            deliver_policy: jetstream::consumer::DeliverPolicy::Last,
            ..Default::default()
        })
        .await?;
    Ok(consumer.messages().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL is built once, at construction. A trailing slash on the
    /// configured base must not produce `//v1/snapshot` — some proxies treat
    /// that as a different path and answer 404, which would read as "the
    /// control plane has nothing" rather than "the URL is wrong".
    #[test]
    fn the_snapshot_url_is_built_without_a_double_slash() {
        for base in ["http://control:8081", "http://control:8081/"] {
            let source = HttpSnapshotSource::new(base).unwrap();
            assert_eq!(source.url, "http://control:8081/v1/snapshot");
        }
    }
}
