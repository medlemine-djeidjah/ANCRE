//! Virtual key auth.
//!
//! The key hash is computed once, at admission, and handed to the resolver
//! precomputed — hashing inside `resolve()` would put the most expensive
//! operation in the request on the wrong side of the 5µs budget.

use ancre_canon::Hash32;

/// SHA-256 of the presented bearer token.
///
/// Fixed at SHA-256 regardless of the `ancre-canon` hash feature: this is a
/// lookup key, not part of the evidence chain, and it must stay stable when a
/// customer switches the chain hash. Constant-time compare on the way out.
#[must_use]
pub fn key_hash(_bearer: &str) -> Hash32 {
    todo!("M3")
}

/// Header-driven pin overrides.
///
/// Precedence: request header > virtual key binding > route rule > system
/// default. Any override is a governance event — a caller pinning their own
/// model version is exactly the thing an auditor wants to find — so it emits
/// `pin.overridden` naming the overridden fields (spec §4).
#[derive(Debug, Default)]
pub struct Overrides {
    pub fields: Vec<&'static str>,
}

impl Overrides {
    #[must_use]
    pub fn from_headers(_headers: &[(&str, &str)]) -> Self {
        todo!("M3: parse x-ancre-pin-* headers")
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}
