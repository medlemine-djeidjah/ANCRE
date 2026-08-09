//! Provider adapters.
//!
//! MVP: OpenAI and Anthropic only. Azure OpenAI and vLLM speak the OpenAI wire
//! format, so they are additive rather than architectural and are deferred
//! (mvp-plan §0).

pub mod anthropic;
pub mod openai;
pub mod sse;

use std::sync::Arc;

use ancre_types::{RiskFlag, UNKNOWN, UNRESOLVED_PREFIX};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
}

impl ProviderKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

/// What a provider says about what actually served the request.
///
/// This is where people get burned. If a customer routes to a floating alias,
/// the weights behind it can change with no signal and every pin recorded
/// against it is a lie. So `model_version` is read from the **provider's
/// response metadata**, never from the request (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedBy {
    /// The provider's own pinned identifier, or `unresolved:<alias>`, or
    /// `unknown`. Never empty, never NULL.
    pub model_version: Arc<str>,
    /// `Some(UnpinnedModel)` when the provider would not name a pinned id.
    /// That is a real finding in a readiness report, not a warning to swallow.
    pub flag: Option<RiskFlag>,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

impl ServedBy {
    /// The provider named a pinned identifier we can stand behind.
    #[must_use]
    pub fn pinned(model_version: &str, tokens_in: u32, tokens_out: u32) -> Self {
        Self {
            model_version: Arc::from(model_version),
            flag: None,
            tokens_in,
            tokens_out,
        }
    }

    /// The provider answered with something that cannot identify the weights.
    ///
    /// Recorded as `unresolved:<alias>` so the alias is still visible in the
    /// record — an auditor asking "what actually ran?" gets "we asked for
    /// gpt-4o and the provider would not say", which is a true and useful
    /// answer. `unknown` alone would throw away the one fact we do have.
    #[must_use]
    pub fn unresolved(alias: &str, tokens_in: u32, tokens_out: u32) -> Self {
        Self {
            model_version: Arc::from(format!("{UNRESOLVED_PREFIX}{alias}")),
            flag: Some(RiskFlag::UnpinnedModel),
            tokens_in,
            tokens_out,
        }
    }

    /// No model field at all in the response.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            model_version: Arc::from(UNKNOWN),
            flag: Some(RiskFlag::UnpinnedModel),
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    #[must_use]
    pub fn is_pinned(&self) -> bool {
        self.flag.is_none()
    }
}

/// Accumulates pins across a streamed response.
///
/// Providers split the answer: the model id arrives in the first frame, the
/// token counts in the last. Neither alone is a complete record, so the
/// gateway folds frames through this and reads it once the stream ends.
#[derive(Debug, Default)]
pub struct StreamPins {
    pub model_version: Option<Arc<str>>,
    pub flag: Option<RiskFlag>,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

impl StreamPins {
    /// Fold in whatever a frame revealed. Later token counts win; the model id
    /// is taken from the first frame that names one and never overwritten.
    pub fn absorb(&mut self, s: ServedBy) {
        if self.model_version.is_none() {
            self.model_version = Some(s.model_version);
            self.flag = s.flag;
        }
        if s.tokens_in > 0 {
            self.tokens_in = s.tokens_in;
        }
        if s.tokens_out > 0 {
            self.tokens_out = s.tokens_out;
        }
    }

    /// True once the model id is known, so the gateway can stop scanning
    /// frames it no longer needs. Token counts still arrive at the end, so
    /// this is not a signal to stop entirely — only that the expensive
    /// question is answered.
    #[must_use]
    pub fn has_model(&self) -> bool {
        self.model_version.is_some()
    }

    /// What the audit event records.
    ///
    /// A stream that ended without ever naming a model is `unknown` plus a
    /// flag — never an empty string, and never a guess from the request.
    #[must_use]
    pub fn finish(self) -> ServedBy {
        match self.model_version {
            Some(model_version) => ServedBy {
                model_version,
                flag: self.flag,
                tokens_in: self.tokens_in,
                tokens_out: self.tokens_out,
            },
            None => ServedBy::unknown(),
        }
    }
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Translate an OpenAI-wire ingress request to this provider's wire format.
    /// Identity for OpenAI; a real translation for Anthropic.
    fn translate_request(&self, body: &[u8]) -> Result<Vec<u8>, ProviderError>;

    /// Pull the pinned model id out of a non-streaming response body.
    fn served_by(&self, body: &[u8]) -> Result<ServedBy, ProviderError>;

    /// Pull it out of an SSE frame. Providers differ on which frame carries
    /// what, and on whether the model id arrives before or after the first
    /// content token — the pin must be captured either way without holding a
    /// byte back from the client.
    fn served_by_streaming(&self, frame: &sse::Frame<'_>) -> Option<ServedBy>;

    /// Where this provider serves the ingress path.
    ///
    /// Ingress is always the OpenAI wire, because that is the whole adoption
    /// story — a customer changes a base URL and nothing else. Providers that
    /// serve the same operation somewhere else have to say so here, or the
    /// gateway posts a translated body to a path that does not exist and the
    /// customer sees a 404 they cannot explain.
    fn upstream_path<'a>(&self, ingress: &'a str) -> std::borrow::Cow<'a, str> {
        std::borrow::Cow::Borrowed(ingress)
    }

    /// Headers the provider requires, given the deployment's credential.
    ///
    /// Here rather than in the gateway because how a provider is authenticated
    /// is part of its wire contract — OpenAI takes a bearer token, Anthropic
    /// takes `x-api-key` plus a version header — and a `match` on provider kind
    /// inside the request path is where that knowledge goes to rot.
    ///
    /// `None` is a deployment with no credential configured for this provider,
    /// which is legitimate: a self-hosted vLLM or a local mock needs none. The
    /// request goes out unauthenticated and the provider's own 401 is recorded
    /// as the outcome, which is a truer answer than a gateway-invented one.
    fn upstream_headers(&self, credential: Option<&str>) -> Vec<(&'static str, String)> {
        credential
            .map(|c| vec![("authorization", format!("Bearer {c}"))])
            .unwrap_or_default()
    }
}

/// Does this identifier name specific weights, or a moving target?
///
/// The asymmetry here is deliberate and worth stating: a false
/// `UnpinnedModel` is a finding someone reviews and dismisses in a minute; a
/// false "pinned" is evidence that lies to an auditor. So anything not
/// recognised as pinned is flagged.
pub(crate) fn looks_pinned(id: &str, kind: ProviderKind) -> bool {
    // Explicitly floating, whatever else the shape suggests.
    if id.ends_with("-latest") || id.is_empty() {
        return false;
    }
    match kind {
        // A fine-tune id names exact weights.
        ProviderKind::OpenAi if id.starts_with("ft:") => true,
        // `gpt-4o-2024-08-06`: a trailing ISO date.
        ProviderKind::OpenAi => ends_with_iso_date(id),
        // `claude-sonnet-4-5-20250929`: a trailing compact date.
        ProviderKind::Anthropic => ends_with_compact_date(id),
    }
}

/// `…-YYYY-MM-DD`
fn ends_with_iso_date(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() >= 11 && b[b.len() - 11] == b'-' && matches_shape(&b[b.len() - 10..], b"dddd-dd-dd")
}

/// `…-YYYYMMDD`
fn ends_with_compact_date(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() >= 9 && b[b.len() - 9] == b'-' && matches_shape(&b[b.len() - 8..], b"dddddddd")
}

fn matches_shape(bytes: &[u8], shape: &[u8]) -> bool {
    bytes.len() == shape.len()
        && bytes.iter().zip(shape).all(|(b, s)| {
            if *s == b'd' {
                b.is_ascii_digit()
            } else {
                b == s
            }
        })
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("malformed provider response: {0}")]
    Malformed(String),
    #[error("cannot translate request faithfully: {0}")]
    Untranslatable(String),
    #[error("upstream returned {status}")]
    Upstream { status: u16 },
    #[error("transport: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_openai_ids_are_pinned() {
        assert!(looks_pinned("gpt-4o-2024-08-06", ProviderKind::OpenAi));
        assert!(looks_pinned("gpt-4o-mini-2024-07-18", ProviderKind::OpenAi));
        assert!(looks_pinned(
            "ft:gpt-4o-2024-08-06:acme::9xYz",
            ProviderKind::OpenAi
        ));
    }

    #[test]
    fn bare_openai_aliases_are_not_pinned() {
        for alias in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4o-latest",
            "chatgpt-4o-latest",
        ] {
            assert!(
                !looks_pinned(alias, ProviderKind::OpenAi),
                "{alias} names a moving target"
            );
        }
    }

    #[test]
    fn dated_anthropic_ids_are_pinned() {
        assert!(looks_pinned(
            "claude-sonnet-4-5-20250929",
            ProviderKind::Anthropic
        ));
        assert!(looks_pinned(
            "claude-opus-4-1-20250805",
            ProviderKind::Anthropic
        ));
    }

    #[test]
    fn bare_anthropic_aliases_are_not_pinned() {
        for alias in ["claude-sonnet-4-5", "claude-3-5-sonnet-latest"] {
            assert!(
                !looks_pinned(alias, ProviderKind::Anthropic),
                "{alias} names a moving target"
            );
        }
    }

    #[test]
    fn a_date_shaped_suffix_that_is_not_digits_is_not_pinned() {
        assert!(!looks_pinned("gpt-4o-abcd-ef-gh", ProviderKind::OpenAi));
        assert!(!looks_pinned("claude-x-abcdefgh", ProviderKind::Anthropic));
    }

    #[test]
    fn empty_is_never_pinned() {
        assert!(!looks_pinned("", ProviderKind::OpenAi));
        assert!(!looks_pinned("", ProviderKind::Anthropic));
    }

    #[test]
    fn an_unresolved_alias_keeps_the_alias_visible() {
        let s = ServedBy::unresolved("gpt-4o", 10, 20);
        assert_eq!(&*s.model_version, "unresolved:gpt-4o");
        assert_eq!(s.flag, Some(RiskFlag::UnpinnedModel));
        assert!(!s.is_pinned());
    }

    #[test]
    fn stream_pins_take_the_model_from_the_first_frame_and_tokens_from_the_last() {
        let mut p = StreamPins::default();
        p.absorb(ServedBy::pinned("gpt-4o-2024-08-06", 100, 0));
        assert!(p.has_model());
        // A later frame naming a different model must not overwrite the first.
        p.absorb(ServedBy::pinned("gpt-4o-mini-2024-07-18", 0, 42));

        let done = p.finish();
        assert_eq!(&*done.model_version, "gpt-4o-2024-08-06");
        assert_eq!(done.tokens_in, 100);
        assert_eq!(done.tokens_out, 42);
    }

    #[test]
    fn a_stream_that_never_names_a_model_is_unknown_and_flagged() {
        let done = StreamPins::default().finish();
        assert_eq!(&*done.model_version, "unknown");
        assert_eq!(done.flag, Some(RiskFlag::UnpinnedModel));
    }
}
