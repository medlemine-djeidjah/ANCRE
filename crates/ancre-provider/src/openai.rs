//! OpenAI adapter. Also the ingress wire format.
//!
//! Adoption is a base-URL change and nothing else. The acceptance test is the
//! unmodified official SDK pointed at the gateway (mvp-plan §5, M3).

use crate::{Provider, ProviderError, ProviderKind, ServedBy, sse};

#[derive(Debug, Default)]
pub struct OpenAi;

impl Provider for OpenAi {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn translate_request(&self, body: &[u8]) -> Result<Vec<u8>, ProviderError> {
        // Identity — ingress already speaks this format. Deliberately not a
        // parse-and-reserialise: unknown fields must pass through untouched or
        // the gateway silently drops provider features it hasn't heard of.
        Ok(body.to_vec())
    }

    fn served_by(&self, _body: &[u8]) -> Result<ServedBy, ProviderError> {
        todo!("M3: response `model` field; flag UnpinnedModel if it equals the requested alias")
    }

    fn served_by_streaming(&self, _frame: &sse::Frame<'_>) -> Option<ServedBy> {
        todo!("M3: first chunk carries `model`; usage arrives in the final chunk")
    }
}
