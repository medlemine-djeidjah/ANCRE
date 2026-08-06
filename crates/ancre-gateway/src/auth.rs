//! Virtual key auth and header-driven pin overrides.
//!
//! The key hash is computed once, at admission, and handed to the resolver
//! precomputed — hashing inside `resolve()` would put the most expensive
//! operation in the request on the wrong side of the 5µs budget.

use ancre_canon::Hash32;
use ancre_types::PinOverrides;

/// Header names the gateway reserves. Namespaced so they cannot collide with
/// anything a provider defines.
pub const HEADER_MODEL_VERSION: &str = "x-ancre-pin-model-version";
pub const HEADER_PROMPT_VERSION: &str = "x-ancre-pin-prompt-version";

/// Hash of the presented bearer token.
///
/// Uses the workspace hash primitive, which is BLAKE3 by default. That is a
/// deliberate choice about *what this is*: a lookup key into the snapshot's
/// key map, not a password digest and not part of the evidence chain. It needs
/// to be fast and collision-resistant, not slow — the token is high-entropy
/// and machine-generated, so there is nothing to brute force.
#[must_use]
pub fn key_hash(bearer: &str) -> Hash32 {
    ancre_canon::hash_bytes(bearer.as_bytes())
}

/// Pull the bearer token out of an `Authorization` header value.
///
/// Case-insensitive on the scheme, per RFC 9110.
#[must_use]
pub fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Parse pin overrides out of request headers.
///
/// Precedence: request header > virtual key binding > route rule > system
/// default (resolver spec §4). Any override is a governance event — a caller
/// pinning their own model version is exactly what an auditor wants to find —
/// so the resolver flags it and the gateway emits `pin.overridden` naming the
/// fields.
#[must_use]
pub fn overrides_from_headers<'a>(headers: &'a [(&'a str, &'a str)]) -> PinOverrides<'a> {
    let find = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
            .filter(|v| !v.is_empty())
    };
    PinOverrides {
        model_version: find(HEADER_MODEL_VERSION),
        prompt_version: find(HEADER_PROMPT_VERSION),
    }
}

/// Which fields a caller overrode, for the `pin.overridden` event.
#[must_use]
pub fn overridden_fields(o: &PinOverrides<'_>) -> Vec<&'static str> {
    let mut f = Vec::new();
    if o.model_version.is_some() {
        f.push("model_version");
    }
    if o.prompt_version.is_some() {
        f.push("prompt_version");
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_bearer_token() {
        assert_eq!(bearer_token("Bearer sk-abc123"), Some("sk-abc123"));
        assert_eq!(bearer_token("bearer sk-abc123"), Some("sk-abc123"));
        assert_eq!(bearer_token("BEARER sk-abc123"), Some("sk-abc123"));
    }

    #[test]
    fn rejects_anything_that_is_not_a_bearer_token() {
        assert_eq!(bearer_token("Basic dXNlcjpwYXNz"), None);
        assert_eq!(bearer_token("sk-abc123"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token(""), None);
    }

    #[test]
    fn the_key_hash_is_stable_and_distinguishes_keys() {
        assert_eq!(key_hash("sk-abc"), key_hash("sk-abc"));
        assert_ne!(key_hash("sk-abc"), key_hash("sk-abd"));
    }

    #[test]
    fn override_headers_are_case_insensitive_on_the_name() {
        let headers = [("X-Ancre-Pin-Model-Version", "gpt-4o-2024-08-06")];
        let o = overrides_from_headers(&headers);
        assert_eq!(o.model_version, Some("gpt-4o-2024-08-06"));
        assert!(!o.is_empty());
    }

    #[test]
    fn an_empty_override_header_is_not_an_override() {
        // Otherwise a proxy that adds empty headers would flag every request
        // as a governance event, and a flag that fires constantly is noise
        // nobody reviews.
        let headers = [("x-ancre-pin-model-version", "")];
        assert!(overrides_from_headers(&headers).is_empty());
    }

    #[test]
    fn no_headers_means_no_overrides() {
        assert!(overrides_from_headers(&[]).is_empty());
        assert!(overridden_fields(&overrides_from_headers(&[])).is_empty());
    }

    #[test]
    fn overridden_fields_names_what_the_caller_pinned() {
        let headers = [
            ("x-ancre-pin-model-version", "gpt-4o-2024-08-06"),
            ("x-ancre-pin-prompt-version", "b3:abcd"),
        ];
        assert_eq!(
            overridden_fields(&overrides_from_headers(&headers)),
            vec!["model_version", "prompt_version"]
        );
    }
}
