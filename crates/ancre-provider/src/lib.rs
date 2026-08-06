//! Provider adapters.
//!
//! MVP: OpenAI and Anthropic only. Azure OpenAI and vLLM speak the OpenAI wire
//! format, so they are additive rather than architectural and are deferred
//! (mvp-plan §0).

pub mod anthropic;
pub mod openai;
pub mod sse;

use std::sync::Arc;

use ancre_types::RiskFlag;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
}

/// What a provider tells us about what actually served the request.
///
/// This is where people get burned. If a customer routes to a floating alias,
/// the weights behind it can change with no signal and every pin recorded
/// against it is a lie. So `model_version` is read from the **provider's
/// response metadata**, never from the request (spec §7).
#[derive(Debug, Clone)]
pub struct ServedBy {
    /// The provider's own pinned identifier, or `unresolved:<alias>`.
    pub model_version: Arc<str>,
    /// `Some(UnpinnedModel)` when the provider would not return a pinned id.
    /// That is a real finding in a readiness report, not a warning to swallow.
    pub flag: Option<RiskFlag>,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Translate an OpenAI-wire ingress request to this provider's wire format.
    /// Identity for OpenAI; a real translation for Anthropic.
    fn translate_request(&self, body: &[u8]) -> Result<Vec<u8>, ProviderError>;

    /// Pull the pinned model id out of a non-streaming response body.
    fn served_by(&self, body: &[u8]) -> Result<ServedBy, ProviderError>;

    /// Pull it out of the SSE frame that carries it. Providers differ on which
    /// frame that is, and on whether it arrives before or after the first
    /// content token — the pin must be captured either way without holding a
    /// byte back from the client.
    fn served_by_streaming(&self, frame: &sse::Frame<'_>) -> Option<ServedBy>;
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("malformed provider response: {0}")]
    Malformed(String),
    #[error("upstream returned {status}")]
    Upstream { status: u16 },
    #[error("transport: {0}")]
    Transport(String),
}
