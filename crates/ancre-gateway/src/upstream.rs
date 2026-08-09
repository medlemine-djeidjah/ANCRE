//! Talking to the provider.
//!
//! A trait, so the request pipeline can be tested end-to-end against a fake
//! without a network — and so the TLS client stays out of the tests that care
//! about pins.

use ancre_provider::{Provider, ProviderKind};
use bytes::Bytes;
use hyper::{Request, Response};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub trait Upstream: Send + Sync + 'static {
    type Body: http_body::Body<Data = Bytes> + Unpin + Send + 'static;

    /// Send a translated request to `provider`.
    ///
    /// The provider is passed as a trait object rather than a kind because the
    /// upstream has to ask it two things it alone knows: where it serves the
    /// ingress path, and how it wants to be authenticated.
    fn send(
        &self,
        provider: &'static dyn Provider,
        req: Request<Bytes>,
    ) -> impl std::future::Future<Output = Result<Response<Self::Body>, BoxError>> + Send;
}

/// The deployment's credential for each provider.
///
/// Read once at startup and held for the process. **Never** taken from the
/// request: the caller presents a virtual key that names a system in the
/// registry, and a gateway that forwarded a caller-supplied provider
/// credential would be a proxy for whoever asked rather than for the customer
/// who deployed it.
#[derive(Clone, Default)]
pub struct Credentials {
    pub openai: Option<String>,
    pub anthropic: Option<String>,
}

impl Credentials {
    #[must_use]
    pub fn get(&self, provider: ProviderKind) -> Option<&str> {
        match provider {
            ProviderKind::OpenAi => self.openai.as_deref(),
            ProviderKind::Anthropic => self.anthropic.as_deref(),
        }
    }

    /// Which providers this deployment can authenticate to. Names only —
    /// nothing here ever prints a credential, including in `Debug`.
    #[must_use]
    pub fn configured(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.openai.is_some() {
            out.push("openai");
        }
        if self.anthropic.is_some() {
            out.push("anthropic");
        }
        out
    }
}

// Hand-written so that no accidental `{:?}` anywhere in this workspace can put
// a provider API key into a log line.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("configured", &self.configured())
            .finish()
    }
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
    credentials: Credentials,
}

impl std::fmt::Debug for HttpsUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsUpstream")
            .field("endpoints", &self.endpoints)
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

impl HttpsUpstream {
    /// # Errors
    /// If no process-wide rustls crypto provider can be installed.
    pub fn new_with_credentials(
        endpoints: Endpoints,
        credentials: Credentials,
    ) -> Result<Self, BoxError> {
        let mut up = Self::new(endpoints)?;
        up.credentials = credentials;
        Ok(up)
    }

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
            credentials: Credentials::default(),
        })
    }
}

impl Upstream for HttpsUpstream {
    type Body = hyper::body::Incoming;

    async fn send(
        &self,
        provider: &'static dyn Provider,
        req: Request<Bytes>,
    ) -> Result<Response<Self::Body>, BoxError> {
        let (mut parts, body) = req.into_parts();
        let kind = provider.kind();

        // Re-target at the provider, keeping the caller's query. The path is
        // whatever the provider says it serves this operation at — identity
        // for OpenAI, which is what makes adoption a base-URL change and
        // nothing else on the customer's side.
        let base = self.endpoints.base(kind);
        let ingress = parts
            .uri
            .path_and_query()
            .map_or_else(|| "/".to_string(), ToString::to_string);
        let path = provider.upstream_path(&ingress);
        parts.uri = format!("{base}{path}").parse()?;

        // The caller's Authorization header names *their* virtual key, which
        // means nothing upstream and must never be forwarded — a virtual key
        // reaching a provider would be a credential leak in the one direction
        // nobody audits.
        parts.headers.remove(hyper::header::AUTHORIZATION);
        parts.headers.remove(hyper::header::HOST);
        // Content-Length is stale after translation, and a wrong one hangs the
        // upstream connection waiting for bytes that will never arrive.
        parts.headers.remove(hyper::header::CONTENT_LENGTH);

        // The deployment's own credential, plus whatever else this provider
        // requires on the wire. Inserted rather than appended, so a header a
        // client happened to send cannot end up alongside ours.
        for (name, value) in provider.upstream_headers(self.credentials.get(kind)) {
            parts.headers.insert(
                hyper::header::HeaderName::from_static(name),
                hyper::header::HeaderValue::from_str(&value)
                    .map_err(|_| format!("{name} for {kind:?} is not a valid header value"))?,
            );
        }

        let req = Request::from_parts(parts, http_body_util::Full::new(body));
        Ok(self.client.request(req).await?)
    }
}
