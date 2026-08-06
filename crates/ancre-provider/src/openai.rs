//! OpenAI adapter. Also the ingress wire format.
//!
//! Adoption is a base-URL change and nothing else. The acceptance test is the
//! unmodified official SDK pointed at the gateway (mvp-plan §5, M3).

use serde::Deserialize;

use crate::{Provider, ProviderError, ProviderKind, ServedBy, looks_pinned, sse};

#[derive(Debug, Default)]
pub struct OpenAi;

/// Only the fields the pins need. `serde` ignores the rest, which is the
/// point: a new OpenAI response field must never break parsing.
#[derive(Debug, Deserialize)]
struct Response {
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl Provider for OpenAi {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn translate_request(&self, body: &[u8]) -> Result<Vec<u8>, ProviderError> {
        // Identity — ingress already speaks this format. Deliberately not a
        // parse-and-reserialise: unknown fields must pass through untouched,
        // or the gateway silently drops provider features it has not heard of
        // and the customer's request runs differently from what they wrote.
        Ok(body.to_vec())
    }

    fn served_by(&self, body: &[u8]) -> Result<ServedBy, ProviderError> {
        let r: Response =
            serde_json::from_slice(body).map_err(|e| ProviderError::Malformed(e.to_string()))?;
        let (tin, tout) = r
            .usage
            .map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens));

        Ok(match r.model {
            Some(m) if looks_pinned(&m, ProviderKind::OpenAi) => ServedBy::pinned(&m, tin, tout),
            Some(m) => ServedBy::unresolved(&m, tin, tout),
            None => ServedBy::unknown(),
        })
    }

    fn served_by_streaming(&self, frame: &sse::Frame<'_>) -> Option<ServedBy> {
        if frame.is_done() {
            return None;
        }
        // A frame that does not parse is skipped rather than fatal: one
        // malformed chunk must not cost the whole stream's pins, and the
        // model id usually arrives again in a later frame anyway.
        let r: Response = serde_json::from_slice(frame.data).ok()?;
        let (tin, tout) = r
            .usage
            .map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens));

        match r.model {
            Some(m) if looks_pinned(&m, ProviderKind::OpenAi) => {
                Some(ServedBy::pinned(&m, tin, tout))
            }
            Some(m) => Some(ServedBy::unresolved(&m, tin, tout)),
            // Usage-only final chunk: no model, but the counts still matter.
            None if tin > 0 || tout > 0 => Some(ServedBy {
                model_version: std::sync::Arc::from(""),
                flag: None,
                tokens_in: tin,
                tokens_out: tout,
            }),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamPins;
    use ancre_types::RiskFlag;

    fn frame(data: &str) -> sse::Frame<'_> {
        sse::Frame {
            event: None,
            data: data.as_bytes(),
        }
    }

    #[test]
    fn a_dated_model_in_the_response_is_the_pin() {
        let body = br#"{"id":"chatcmpl-1","model":"gpt-4o-2024-08-06",
            "usage":{"prompt_tokens":1200,"completion_tokens":300}}"#;
        let s = OpenAi.served_by(body).unwrap();

        assert_eq!(&*s.model_version, "gpt-4o-2024-08-06");
        assert!(s.is_pinned());
        assert_eq!((s.tokens_in, s.tokens_out), (1200, 300));
    }

    /// **Resolver spec §10, test 5.** The provider answered with a floating
    /// alias, so we cannot say which weights ran. Flag it and serve anyway.
    #[test]
    fn case_5_a_floating_alias_is_flagged_and_the_request_still_succeeds() {
        let body = br#"{"model":"gpt-4o","usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let s = OpenAi.served_by(body).unwrap();

        assert_eq!(&*s.model_version, "unresolved:gpt-4o");
        assert_eq!(s.flag, Some(RiskFlag::UnpinnedModel));
        // The token counts are still real and still recorded.
        assert_eq!((s.tokens_in, s.tokens_out), (10, 5));
    }

    #[test]
    fn a_response_with_no_model_field_is_unknown_not_empty() {
        let s = OpenAi.served_by(br#"{"id":"chatcmpl-1"}"#).unwrap();
        assert_eq!(&*s.model_version, "unknown");
        assert_eq!(s.flag, Some(RiskFlag::UnpinnedModel));
    }

    #[test]
    fn unknown_response_fields_are_ignored_not_fatal() {
        let body = br#"{"model":"gpt-4o-2024-08-06","some_field_from_2027":{"a":1}}"#;
        assert!(OpenAi.served_by(body).unwrap().is_pinned());
    }

    #[test]
    fn a_malformed_body_is_an_error_not_a_silent_unknown() {
        // Distinguishable on purpose: "the provider sent us nonsense" and
        // "the provider would not name a model" are different findings.
        assert!(OpenAi.served_by(b"not json").is_err());
    }

    #[test]
    fn a_streamed_response_yields_the_model_first_and_usage_last() {
        let mut pins = StreamPins::default();

        for f in [
            r#"{"id":"1","model":"gpt-4o-2024-08-06","choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"id":"1","model":"gpt-4o-2024-08-06","choices":[{"delta":{"content":"He"}}]}"#,
            r#"{"id":"1","model":"gpt-4o-2024-08-06","choices":[{"delta":{"content":"llo"}}]}"#,
            r#"{"id":"1","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
        ] {
            if let Some(s) = OpenAi.served_by_streaming(&frame(f)) {
                pins.absorb(s);
            }
        }

        let done = pins.finish();
        assert_eq!(&*done.model_version, "gpt-4o-2024-08-06");
        assert!(done.is_pinned());
        assert_eq!((done.tokens_in, done.tokens_out), (9, 2));
    }

    #[test]
    fn the_done_terminator_carries_no_pins() {
        assert!(OpenAi.served_by_streaming(&frame("[DONE]")).is_none());
    }

    #[test]
    fn a_malformed_frame_is_skipped_rather_than_failing_the_stream() {
        assert!(OpenAi.served_by_streaming(&frame("{oops")).is_none());
    }

    #[test]
    fn translate_passes_the_body_through_byte_for_byte() {
        let body = br#"{"model":"gpt-4o","messages":[],"some_new_param":true}"#;
        assert_eq!(OpenAi.translate_request(body).unwrap(), body);
    }
}
