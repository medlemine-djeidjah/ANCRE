//! Who may read a customer's evidence.
//!
//! Until now the whole read API was open, on the reasoning that a checkpoint is
//! a signature over a hash and reveals nothing. That reasoning holds for
//! checkpoints and keys. It never held for `GET /v1/chains/…/events`, which
//! serves **content** — no prompts or completions, but `system_id`, timings,
//! token counts and model versions are a competitive picture of how a customer
//! runs their AI (`docs/deferred.md`, D17).
//!
//! A dashboard is what forces the issue. An obscure JSONL endpoint is a gap; a
//! browsable window onto the same data with a login box next to it is an
//! incident waiting for a port to be exposed.
//!
//! ## The split, and why it is not "authenticate everything"
//!
//! | Open | Protected |
//! |---|---|
//! | `/healthz` | `/v1/chains/…` — the events, and the chain list |
//! | `/v1/checkpoints/…` | `/v1/snapshot` — the fleet's whole configuration |
//! | `/v1/pubkeys` | `/v1/prompts/{hash}` — prompt bodies |
//!
//! The left column is what an auditor needs to *verify* something they were
//! handed, and an auditor who has to obtain a credential first is an auditor
//! who verifies less. Signatures over hashes leak nothing: you cannot learn a
//! customer's traffic from a root hash you cannot invert. The right column is
//! the customer's business.
//!
//! ## What this is not
//!
//! One shared operator token, not user accounts. There are no roles, no per
//! tenant scoping, and no audit trail of who read what. That is honest for an
//! MVP whose registry is edited with `psql`, and it is the wrong shape for a
//! multi-tenant SaaS — a customer who can log in can read *every* tenant's
//! chains. `docs/deferred.md` carries that as D22.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// The cookie a browser session gets. `HttpOnly` so no script can read it,
/// `SameSite=Strict` so no other origin can cause it to be sent, and `Path=/`
/// because the API and the UI share an origin by design.
const SESSION_COOKIE: &str = "ancre_session";

/// How long a browser session lasts before it has to be re-established.
///
/// Eight hours: long enough for a working day of an audit review, short enough
/// that a laptop left open in a client's office stops being a credential
/// overnight.
const SESSION_MAX_AGE_SECS: u64 = 8 * 60 * 60;

#[derive(Clone)]
pub struct Admin {
    token: Arc<str>,
}

// Hand-written: the token must never reach a log line, and `#[derive(Debug)]`
// on a struct that holds one is how it eventually does.
impl std::fmt::Debug for Admin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admin").finish_non_exhaustive()
    }
}

impl Admin {
    #[must_use]
    pub fn new(token: impl Into<Arc<str>>) -> Self {
        Self {
            token: token.into(),
        }
    }

    /// Read the operator token, or mint one and say so.
    ///
    /// Generating on first boot rather than refusing to start is the same trade
    /// the signing key makes: the alternative is a quickstart whose first step
    /// is a secret ceremony. What makes it tolerable is that the generated
    /// token is **printed once, loudly**, and is not persisted — a restart
    /// mints a new one. A token that survived a restart without anybody
    /// choosing it would be a permanent credential nobody knows they have.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("ANCRE_ADMIN_TOKEN") {
            Ok(t) if !t.trim().is_empty() => {
                let token = t.trim();
                if is_a_placeholder(token) {
                    // Compose ships a default so that `docker compose up`
                    // works with no configuration at all. That default is a
                    // published string, which makes it a credential in the
                    // same sense that `admin/admin` is one — so it is named
                    // out loud on every boot rather than left to be
                    // discovered by whoever reads the compose file last.
                    tracing::warn!(
                        "ANCRE_ADMIN_TOKEN is a well-known default value. Anyone who \
                         has read this project's compose file can read every chain \
                         this control plane serves. Set a real one before exposing \
                         port 8081 to anything"
                    );
                }
                Self::new(token)
            }
            _ => {
                let mut bytes = [0u8; 24];
                rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
                let token = hex::encode(bytes);
                tracing::warn!(
                    token = %token,
                    "ANCRE_ADMIN_TOKEN is unset, so one was generated for this process \
                     only. It is printed here because there is nowhere else to find it, \
                     and it will be different after a restart. Set the variable for any \
                     deployment you expect to log in to twice"
                );
                Self::new(token)
            }
        }
    }

    /// Constant-time comparison.
    ///
    /// A short-circuiting `==` on a secret leaks its prefix to anyone who can
    /// time the endpoint. The token is high-entropy and the network noise is
    /// larger than the signal, so this is cheap insurance rather than a fix for
    /// a live threat — but the version of this function that does the wrong
    /// thing is the same length as the one that does the right thing.
    #[must_use]
    pub fn admits(&self, presented: &str) -> bool {
        let a = self.token.as_bytes();
        let b = presented.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    /// `Set-Cookie` for a established session.
    #[must_use]
    pub fn session_cookie(&self) -> String {
        format!(
            "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_MAX_AGE_SECS}",
            self.token
        )
    }

    /// `Set-Cookie` that clears the session.
    #[must_use]
    pub fn cleared_cookie() -> String {
        format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
    }
}

/// Values shipped as defaults or typed in a hurry. Not an exhaustive list and
/// not meant to be — it catches the ones this repository itself publishes.
fn is_a_placeholder(token: &str) -> bool {
    const KNOWN: &[&str] = &[
        "ancre-insecure-default",
        "ancre-demo-operator",
        "changeme",
        "change-me",
        "password",
        "admin",
        "secret",
    ];
    KNOWN.iter().any(|k| k.eq_ignore_ascii_case(token))
}

/// A bearer token, or the session cookie. Both carry the same secret.
///
/// Two forms because there are two callers with different constraints: a
/// browser cannot hold a bearer token without putting it somewhere a script can
/// read, and a `curl` in a runbook should not have to fake a cookie.
pub fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        })
        .filter(|t| !t.is_empty())
    {
        return Some(bearer.to_string());
    }

    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|jar| {
            jar.split(';').find_map(|c| {
                let (name, value) = c.trim().split_once('=')?;
                (name == SESSION_COOKIE).then(|| value.to_string())
            })
        })
        .filter(|t| !t.is_empty())
}

/// Refuse anything that does not present the operator token.
///
/// # Errors
/// 401 with a body a human can act on. Not 403: the caller may well be allowed
/// in, they simply have not said who they are, and telling them to authenticate
/// is more useful than telling them they are forbidden.
pub async fn require_admin(State(admin): State<Admin>, request: Request, next: Next) -> Response {
    match presented_token(request.headers()) {
        Some(token) if admin.admits(&token) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"authentication required","hint":"POST /api/session with {\"token\":\"…\"}, or send Authorization: Bearer <ANCRE_ADMIN_TOKEN>"}"#,
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.insert(name, value.parse().unwrap());
        }
        h
    }

    #[test]
    fn the_right_token_is_admitted_and_a_wrong_one_is_not() {
        let admin = Admin::new("s3cret");
        assert!(admin.admits("s3cret"));
        assert!(!admin.admits("s3cres"));
        assert!(!admin.admits(""));
    }

    /// A prefix match must not pass. The obvious wrong implementation —
    /// `presented.starts_with(&token)` or a length-insensitive compare — lets a
    /// caller in with the empty string or with a truncation.
    #[test]
    fn a_prefix_or_a_longer_string_is_refused() {
        let admin = Admin::new("s3cret");
        assert!(!admin.admits("s3c"));
        assert!(!admin.admits("s3cret-and-then-some"));
    }

    #[test]
    fn a_bearer_token_is_read() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer s3cret")]);
        assert_eq!(presented_token(&h).as_deref(), Some("s3cret"));
    }

    #[test]
    fn a_session_cookie_is_read_from_a_jar_with_other_cookies_in_it() {
        let h = headers(&[(header::COOKIE, "theme=dark; ancre_session=s3cret; other=1")]);
        assert_eq!(presented_token(&h).as_deref(), Some("s3cret"));
    }

    #[test]
    fn a_cookie_that_merely_contains_the_name_is_not_the_cookie() {
        let h = headers(&[(header::COOKIE, "not_ancre_session=nope")]);
        assert_eq!(presented_token(&h), None);
    }

    #[test]
    fn nothing_presented_is_none_rather_than_an_empty_match() {
        assert_eq!(presented_token(&HeaderMap::new()), None);
        let h = headers(&[(header::AUTHORIZATION, "Basic abc")]);
        assert_eq!(presented_token(&h), None, "only bearer is a bearer");
    }

    /// The cookie must be unreadable by script and unsendable cross-origin.
    /// Losing either attribute turns a session into an XSS or CSRF primitive.
    #[test]
    fn the_session_cookie_is_httponly_and_samesite_strict() {
        let cookie = Admin::new("s3cret").session_cookie();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Max-Age=28800"), "{cookie}");
    }

    #[test]
    fn logging_out_clears_the_cookie() {
        assert!(Admin::cleared_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn the_shipped_defaults_are_recognised_as_placeholders() {
        // If someone changes compose's default, this is the test that should
        // fail — a published credential that stops being announced is worse
        // than one that never was.
        assert!(is_a_placeholder("ancre-insecure-default"));
        assert!(is_a_placeholder("ancre-demo-operator"));
        assert!(is_a_placeholder("CHANGEME"));
        assert!(!is_a_placeholder("a-real-32-byte-random-value"));
    }

    #[test]
    fn the_token_never_appears_in_debug_output() {
        let rendered = format!("{:?}", Admin::new("s3cret"));
        assert!(!rendered.contains("s3cret"), "{rendered}");
    }
}
