//! Anthropic adapter. Translates from the OpenAI-wire ingress.

use crate::{Provider, ProviderError, ProviderKind, ServedBy, sse};

#[derive(Debug, Default)]
pub struct Anthropic;

impl Provider for Anthropic {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn translate_request(&self, _body: &[u8]) -> Result<Vec<u8>, ProviderError> {
        // The lossy parts, which need to be decided once and documented:
        // - `system` is a top-level field here, a message role there
        // - `max_tokens` is required
        // - tool-call shapes differ
        //
        // Anything that cannot be translated faithfully must be an error, not
        // a silent drop. A request that ran differently from what the caller
        // asked for makes the audit event wrong, and a wrong event is worse
        // than a rejected request.
        todo!("M3")
    }

    fn served_by(&self, _body: &[u8]) -> Result<ServedBy, ProviderError> {
        todo!("M3: response `model`; usage.input_tokens / output_tokens")
    }

    fn served_by_streaming(&self, _frame: &sse::Frame<'_>) -> Option<ServedBy> {
        todo!("M3: message_start carries model; message_delta carries final usage")
    }
}
