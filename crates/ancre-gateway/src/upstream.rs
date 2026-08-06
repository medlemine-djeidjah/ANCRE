//! Talking to the provider.
//!
//! A trait, so the request pipeline can be tested end-to-end against a fake
//! without a network — and so the TLS client stays out of the tests that care
//! about pins.

use ancre_provider::ProviderKind;
use bytes::Bytes;
use hyper::{Request, Response};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub trait Upstream: Send + Sync + 'static {
    type Body: http_body::Body<Data = Bytes> + Unpin + Send + 'static;

    fn send(
        &self,
        provider: ProviderKind,
        req: Request<Bytes>,
    ) -> impl std::future::Future<Output = Result<Response<Self::Body>, BoxError>> + Send;
}

/// Where each provider lives. Overridable so a customer can point at Azure or
/// a self-hosted vLLM without a code change.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub openai: String,
    pub anthropic: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            openai: "https://api.openai.com".into(),
            anthropic: "https://api.anthropic.com".into(),
        }
    }
}

impl Endpoints {
    #[must_use]
    pub fn base(&self, provider: ProviderKind) -> &str {
        match provider {
            ProviderKind::OpenAi => &self.openai,
            ProviderKind::Anthropic => &self.anthropic,
        }
    }
}

/// The real client: hyper over rustls, connection-pooled.
///
/// rustls with webpki roots, never OpenSSL — the static-binary distribution
/// story depends on it, and "no OpenSSL CVE surface" is a sentence worth
/// having in a security questionnaire (PRD §10).
#[derive(Clone)]
pub struct HttpsUpstream {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<Bytes>,
    >,
    endpoints: Endpoints,
}

impl std::fmt::Debug for HttpsUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsUpstream")
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl HttpsUpstream {
    /// # Errors
    /// If no process-wide rustls crypto provider can be installed.
    pub fn new(endpoints: Endpoints) -> Result<Self, BoxError> {
        // Installing the provider is idempotent-ish: a second call errors, and
        // that is fine — it means something already installed one.
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
            endpoints,
        })
    }
}

impl Upstream for HttpsUpstream {
    type Body = hyper::body::Incoming;

    async fn send(
        &self,
        provider: ProviderKind,
        req: Request<Bytes>,
    ) -> Result<Response<Self::Body>, BoxError> {
        let (mut parts, body) = req.into_parts();

        // Re-target at the provider, keeping the caller's path and query. This
        // is what makes adoption a base-URL change on the customer's side.
        let base = self.endpoints.base(provider);
        let path = parts
            .uri
            .path_and_query()
            .map_or_else(|| "/".to_string(), ToString::to_string);
        parts.uri = format!("{base}{path}").parse()?;

        // The caller's Authorization header names *their* virtual key, which
        // means nothing upstream. Strip it; the connector adds the real
        // provider credential.
        parts.headers.remove(hyper::header::AUTHORIZATION);
        parts.headers.remove(hyper::header::HOST);
        // Content-Length is stale after translation, and a wrong one hangs the
        // upstream connection waiting for bytes that will never arrive.
        parts.headers.remove(hyper::header::CONTENT_LENGTH);

        let req = Request::from_parts(parts, http_body_util::Full::new(body));
        Ok(self.client.request(req).await?)
    }
}
