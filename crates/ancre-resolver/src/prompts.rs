//! Bounded LRU for prompt bodies.
//!
//! Pins need only the 32-byte hash, which is always resident. Bodies can be
//! megabytes across thousands of tenants, so they load lazily. A cold body
//! costs one fetch on first use and **never affects the pin** — which is the
//! point: resolution must not be able to block on I/O.

use std::sync::Arc;

use ancre_canon::Hash32;

#[derive(Debug)]
pub struct PromptCache {
    _inner: (),
}

impl PromptCache {
    #[must_use]
    pub fn new(_max_bytes: u64) -> Self {
        todo!("M2: moka::future::Cache weighted by body length")
    }

    /// Resident hit only. The hot path calls this and never waits.
    #[must_use]
    pub fn get_if_resident(&self, _hash: Hash32) -> Option<Arc<str>> {
        todo!("M2")
    }

    /// Off the hot path: fetch from the control plane, verify the body hashes
    /// to `hash` before inserting. A prompt body that does not match its own
    /// pin is a corrupt cache at best, so it is never cached.
    pub async fn load(&self, _hash: Hash32) -> Option<Arc<str>> {
        todo!("M2")
    }

    /// Bench target: 10k systems, hashes only, < 200MB resident.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        todo!("M2")
    }
}
