//! Bounded LRU for prompt bodies.
//!
//! Pins need only the 32-byte hash, which is always resident. Bodies can be
//! megabytes across thousands of tenants, so they load lazily. A cold body
//! costs one fetch on first use and **never affects the pin** — which is the
//! point: resolution must not be able to block on I/O.
//!
//! Nothing in `resolve()` touches this cache. It exists for the request path
//! *after* pins are resolved, where a body is actually needed.

use std::sync::Arc;

use ancre_canon::Hash32;
use moka::sync::Cache;

#[derive(Debug)]
pub struct PromptCache {
    inner: Cache<Hash32, Arc<str>>,
}

impl PromptCache {
    /// Weighted by body length, so the bound is bytes rather than entries —
    /// one 4MB prompt and ten thousand 400-byte ones are very different
    /// resident footprints, and an entry count cannot tell them apart.
    #[must_use]
    pub fn new(max_bytes: u64) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|_k, v: &Arc<str>| u32::try_from(v.len()).unwrap_or(u32::MAX))
                .build(),
        }
    }

    /// Resident hit only. The request path calls this and never waits.
    #[must_use]
    pub fn get_if_resident(&self, hash: Hash32) -> Option<Arc<str>> {
        self.inner.get(&hash)
    }

    /// Verify a fetched body against the hash it claims, then cache it.
    ///
    /// The verification is not defensive politeness. A body that does not hash
    /// to its own pin means the recorded `prompt_version` does not describe
    /// what actually ran, and every event that used it would be a lie. Refuse
    /// it, and let the caller fail the request rather than serve unpinnable
    /// traffic.
    ///
    /// TODO(M4): the control-plane fetch that produces `body` lives in the
    /// gateway; this stays the only place a body enters the cache.
    pub fn insert_verified(&self, hash: Hash32, body: &str) -> Result<Arc<str>, PromptError> {
        let actual = ancre_canon::hash_bytes(body.as_bytes());
        if actual != hash {
            return Err(PromptError::HashMismatch {
                claimed: hash,
                actual,
            });
        }
        let body: Arc<str> = Arc::from(body);
        self.inner.insert(hash, Arc::clone(&body));
        Ok(body)
    }

    /// Approximate resident bytes.
    ///
    /// Bench target: 10k systems, hashes only, < 200MB (resolver spec §9).
    /// Moka's accounting is eventually consistent, so this is for metrics and
    /// capacity planning, never for a correctness decision.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.inner.weighted_size()
    }

    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PromptError {
    #[error("prompt body does not match its pin: claimed {claimed}, got {actual}")]
    HashMismatch { claimed: Hash32, actual: Hash32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verified_body_round_trips() {
        let cache = PromptCache::new(1024 * 1024);
        let body = "You are a helpful assistant.";
        let hash = ancre_canon::hash_bytes(body.as_bytes());

        assert!(cache.get_if_resident(hash).is_none());
        cache.insert_verified(hash, body).unwrap();
        assert_eq!(&*cache.get_if_resident(hash).unwrap(), body);
    }

    #[test]
    fn a_body_that_does_not_match_its_pin_is_refused_and_not_cached() {
        let cache = PromptCache::new(1024 * 1024);
        let claimed = ancre_canon::hash_bytes(b"the prompt we pinned");

        let err = cache
            .insert_verified(claimed, "a different prompt")
            .unwrap_err();
        assert!(matches!(err, PromptError::HashMismatch { .. }));
        assert!(
            cache.get_if_resident(claimed).is_none(),
            "a body that failed verification must never become resident"
        );
    }

    #[test]
    fn a_miss_returns_none_rather_than_blocking() {
        let cache = PromptCache::new(1024);
        assert!(
            cache
                .get_if_resident(ancre_canon::hash_bytes(b"never seen"))
                .is_none()
        );
    }

    #[test]
    fn the_bound_is_bytes_not_entries() {
        // 1KB budget, 100 bodies of 100 bytes each: the cache must evict
        // rather than hold 10KB.
        let cache = PromptCache::new(1_000);
        for i in 0..100u32 {
            let body = format!("{i:0>100}");
            let hash = ancre_canon::hash_bytes(body.as_bytes());
            cache.insert_verified(hash, &body).unwrap();
        }
        cache.inner.run_pending_tasks();
        assert!(
            cache.resident_bytes() <= 1_000,
            "resident {} exceeds the 1000-byte budget",
            cache.resident_bytes()
        );
    }
}
