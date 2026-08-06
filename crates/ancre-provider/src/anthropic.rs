//! Anthropic adapter. Translates from the OpenAI-wire ingress.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Provider, ProviderError, ProviderKind, ServedBy, looks_pinned, sse};

#[derive(Debug, Default)]
pub struct Anthropic;

/// Anthropic requires `max_tokens`; OpenAI treats it as optional. A request
/// that omits it has to be given one, and inventing a number silently would
/// mean the completion the customer got was shaped by us rather than by them.
/// So the value is explicit, documented, and generous enough not to truncate
/// real work.
pub const DEFAULT_MAX_TOKENS: u64 = 4096;

#[derive(Debug, Deserialize)]
struct Response {
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// `message_start` nests the message; `message_delta` carries final usage.
#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(default)]
    message: Option<Response>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: Value,
    max_tokens: u64,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Value>,
}

impl Provider for Anthropic {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    /// OpenAI wire in, Anthropic wire out.
    ///
    /// Anything that cannot be translated faithfully is an **error, not a
    /// silent drop**. A request that ran differently from what the caller
    /// asked for makes the audit event wrong, and a wrong event is worse than
    /// a rejected request — the whole product is the claim that the record
    /// describes what happened.
    fn translate_request(&self, body: &[u8]) -> Result<Vec<u8>, ProviderError> {
        let req: Map<String, Value> =
            serde_json::from_slice(body).map_err(|e| ProviderError::Malformed(e.to_string()))?;

        let model = req
            .get("model")
            .cloned()
            .ok_or_else(|| ProviderError::Untranslatable("request has no model".into()))?;

        let messages = req
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::Untranslatable("request has no messages array".into()))?;

        // `system` is a top-level field here, a message role there.
        let mut system_parts = Vec::new();
        let mut out_messages = Vec::with_capacity(messages.len());
        for m in messages {
            match m.get("role").and_then(Value::as_str) {
                Some("system") => {
                    if let Some(c) = m.get("content") {
                        system_parts.push(c.clone());
                    }
                }
                Some("user" | "assistant") => out_messages.push(m.clone()),
                Some(other) => {
                    // `tool` and `function` messages have a genuinely different
                    // shape. Refusing beats guessing.
                    return Err(ProviderError::Untranslatable(format!(
                        "message role `{other}` has no faithful Anthropic equivalent"
                    )));
                }
                None => {
                    return Err(ProviderError::Untranslatable("message has no role".into()));
                }
            }
        }

        for unsupported in ["tools", "tool_choice", "functions", "response_format"] {
            if req.contains_key(unsupported) {
                return Err(ProviderError::Untranslatable(format!(
                    "`{unsupported}` is not translated in the MVP; \
                     route this system to OpenAI or wait for the tool mapping"
                )));
            }
        }

        let system = match system_parts.len() {
            0 => None,
            1 => Some(system_parts.remove(0)),
            // Multiple system messages: joining them changes the prompt, so
            // the request is refused rather than quietly rewritten.
            _ => {
                return Err(ProviderError::Untranslatable(
                    "multiple system messages cannot be merged without changing the prompt".into(),
                ));
            }
        };

        let max_tokens = req
            .get("max_tokens")
            .or_else(|| req.get("max_completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_TOKENS);

        let out = AnthropicRequest {
            model,
            max_tokens,
            messages: out_messages,
            system,
            stream: req.get("stream").cloned(),
            temperature: req.get("temperature").cloned(),
            top_p: req.get("top_p").cloned(),
            stop_sequences: req.get("stop").cloned(),
        };

        serde_json::to_vec(&out).map_err(|e| ProviderError::Malformed(e.to_string()))
    }

    fn served_by(&self, body: &[u8]) -> Result<ServedBy, ProviderError> {
        let r: Response =
            serde_json::from_slice(body).map_err(|e| ProviderError::Malformed(e.to_string()))?;
        let (tin, tout) = r
            .usage
            .map_or((0, 0), |u| (u.input_tokens, u.output_tokens));

        Ok(match r.model {
            Some(m) if looks_pinned(&m, ProviderKind::Anthropic) => ServedBy::pinned(&m, tin, tout),
            Some(m) => ServedBy::unresolved(&m, tin, tout),
            None => ServedBy::unknown(),
        })
    }

    fn served_by_streaming(&self, frame: &sse::Frame<'_>) -> Option<ServedBy> {
        let ev: StreamEvent = serde_json::from_slice(frame.data).ok()?;

        // `message_delta` carries the final output count.
        let (mut tin, mut tout) = ev
            .usage
            .map_or((0, 0), |u| (u.input_tokens, u.output_tokens));

        // `message_start` carries the model and the input count.
        let model = ev.message.and_then(|m| {
            if let Some(u) = m.usage {
                tin = tin.max(u.input_tokens);
                tout = tout.max(u.output_tokens);
            }
            m.model
        });

        match model {
            Some(m) if looks_pinned(&m, ProviderKind::Anthropic) => {
                Some(ServedBy::pinned(&m, tin, tout))
            }
            Some(m) => Some(ServedBy::unresolved(&m, tin, tout)),
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

    fn translate(body: &str) -> Result<Map<String, Value>, ProviderError> {
        Anthropic
            .translate_request(body.as_bytes())
            .map(|v| serde_json::from_slice(&v).unwrap())
    }

    #[test]
    fn a_system_message_becomes_a_top_level_field() {
        let out = translate(
            r#"{"model":"claude-sonnet-4-5","messages":[
                {"role":"system","content":"Be brief."},
                {"role":"user","content":"Hi"}]}"#,
        )
        .unwrap();

        assert_eq!(out["system"], "Be brief.");
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], "user");
    }

    #[test]
    fn max_tokens_is_supplied_when_the_caller_omits_it() {
        let out = translate(r#"{"model":"claude-sonnet-4-5","messages":[]}"#).unwrap();
        assert_eq!(out["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn the_callers_max_tokens_is_preserved() {
        let out =
            translate(r#"{"model":"claude-sonnet-4-5","messages":[],"max_tokens":100}"#).unwrap();
        assert_eq!(out["max_tokens"], 100);
    }

    #[test]
    fn stop_becomes_stop_sequences() {
        let out =
            translate(r#"{"model":"claude-sonnet-4-5","messages":[],"stop":["END"]}"#).unwrap();
        assert_eq!(out["stop_sequences"][0], "END");
    }

    /// A wrong event is worse than a rejected request.
    #[test]
    fn untranslatable_requests_are_refused_rather_than_silently_altered() {
        for body in [
            // Tool calls have a different shape entirely.
            r#"{"model":"c","messages":[],"tools":[{"type":"function"}]}"#,
            // A tool result message.
            r#"{"model":"c","messages":[{"role":"tool","content":"x"}]}"#,
            // Two system messages: joining them changes the prompt.
            r#"{"model":"c","messages":[{"role":"system","content":"a"},
                {"role":"system","content":"b"}]}"#,
            // Structured output has no faithful equivalent yet.
            r#"{"model":"c","messages":[],"response_format":{"type":"json_object"}}"#,
        ] {
            assert!(
                matches!(translate(body), Err(ProviderError::Untranslatable(_))),
                "must refuse rather than rewrite: {body}"
            );
        }
    }

    #[test]
    fn a_request_without_a_model_or_messages_is_refused() {
        assert!(translate(r#"{"messages":[]}"#).is_err());
        assert!(translate(r#"{"model":"c"}"#).is_err());
    }

    #[test]
    fn a_dated_model_in_the_response_is_the_pin() {
        let body = br#"{"model":"claude-sonnet-4-5-20250929",
            "usage":{"input_tokens":1200,"output_tokens":300}}"#;
        let s = Anthropic.served_by(body).unwrap();

        assert_eq!(&*s.model_version, "claude-sonnet-4-5-20250929");
        assert!(s.is_pinned());
        assert_eq!((s.tokens_in, s.tokens_out), (1200, 300));
    }

    #[test]
    fn case_5_an_undated_anthropic_id_is_flagged() {
        let s = Anthropic
            .served_by(br#"{"model":"claude-sonnet-4-5","usage":{"input_tokens":1}}"#)
            .unwrap();
        assert_eq!(&*s.model_version, "unresolved:claude-sonnet-4-5");
        assert_eq!(s.flag, Some(RiskFlag::UnpinnedModel));
    }

    #[test]
    fn a_streamed_response_takes_the_model_from_message_start() {
        let mut pins = StreamPins::default();

        for f in [
            r#"{"type":"message_start","message":{"model":"claude-sonnet-4-5-20250929",
                "usage":{"input_tokens":9,"output_tokens":0}}}"#,
            r#"{"type":"content_block_delta","delta":{"text":"Hi"}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":2}}"#,
        ] {
            if let Some(s) = Anthropic.served_by_streaming(&frame(f)) {
                pins.absorb(s);
            }
        }

        let done = pins.finish();
        assert_eq!(&*done.model_version, "claude-sonnet-4-5-20250929");
        assert_eq!((done.tokens_in, done.tokens_out), (9, 2));
    }
}
