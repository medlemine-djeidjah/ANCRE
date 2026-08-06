//! In-path version-pin resolver. Implements `version-pin-resolver-spec.md`.
//!
//! Budget: p50 < 2µs, p99 < 5µs, p99 < 8µs under a 10/s reload storm.
//! The whole latency claim — and therefore the whole technical
//! differentiator — descends from these numbers. Benches gate CI.

use std::sync::Arc;
use std::time::Duration;

use ancre_types::{ConfigSnapshot, IngressMeta, Pins};
use arc_swap::ArcSwap;

pub mod prompts;
pub mod staleness;

pub use staleness::{Freshness, StalenessPolicy};

#[derive(Debug)]
pub struct PinResolver {
    current: ArcSwap<ConfigSnapshot>,
    // Both read once `resolve` is implemented; the allow goes then.
    #[allow(dead_code)]
    policy: StalenessPolicy,
    #[allow(dead_code)]
    prompts: prompts::PromptCache,
}

impl PinResolver {
    /// Cold start: no snapshot yet → fail closed for **all** systems. Never
    /// serve traffic with `unknown` pins on a high-risk system (spec §6).
    #[must_use]
    pub fn cold() -> Self {
        todo!("M2: an empty snapshot that resolves to ColdStart, not an Option")
    }

    /// The hot path. One atomic load, two hash lookups, a short linear scan
    /// over routes, ~12 refcount increments.
    ///
    /// `ArcSwap::load()` is an atomic load plus a hazard-pointer guard — tens
    /// of nanoseconds, no inter-core contention. Do **not** reach for
    /// `RwLock<Arc<T>>`: even uncontended, the read lock touches a shared
    /// cache line, and above ~350 RPS across cores that lands in the p99 you
    /// are selling. The reload-storm bench is what catches this; run it on day
    /// one of M2, before the code is worth defending.
    #[inline]
    pub fn resolve(&self, _req: &IngressMeta<'_>) -> Result<Pins, ResolveError> {
        todo!("M2: per spec §4 — no allocation, no lock, no await")
    }

    /// Build off-thread, then swap. The previous snapshot stays alive as long
    /// as an in-flight request holds a reference: a request that loaded
    /// generation 41 keeps reading 41 until it completes, even if 42 lands
    /// mid-stream. That is the atomicity guarantee, and it is why a 60-second
    /// streaming completion reports the generation it started under.
    pub fn reload(&self, next: ConfigSnapshot) -> ReloadOutcome {
        let prev = self.current.load_full();
        let next = Arc::new(next);
        self.current.store(Arc::clone(&next));
        Self::diff(&prev, &next)
    }

    /// Diff consecutive generations. Article 12(2)(a) wants situations that
    /// *may* constitute a substantial modification to be identifiable from the
    /// logs.
    ///
    /// This **surfaces candidates and never declares conclusions** (PRD §6.5).
    /// A substantial modification can reset a grandfathering position and pull
    /// a system back into scope — that is a legal determination. The product
    /// flags; the human decides; the decision is logged.
    fn diff(_prev: &ConfigSnapshot, _next: &ConfigSnapshot) -> ReloadOutcome {
        todo!("M4: per spec §8 — classify_change over each system, collect changed fields")
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.current.load().generation
    }
}

/// Feeds the `config.generation.applied` event.
///
/// `propagation_ms` is the evidence for the bounded-staleness claim. Its p99
/// goes in the sales deck *and* in the customer's Article 11 documentation, so
/// it is measured, not asserted.
#[derive(Debug)]
pub struct ReloadOutcome {
    pub from_generation: u64,
    pub to_generation: u64,
    pub propagation_ms: u32,
    pub changed_fields: Vec<String>,
    /// Systems whose change *may* constitute a substantial modification.
    /// Alert-worthy. Never auto-classified.
    pub substantial_candidates: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("unknown api key")]
    UnknownKey,
    #[error("unknown system: {0}")]
    UnknownSystem(String),
    /// Served as a 503. High-risk system, config older than the budget,
    /// control plane unreachable.
    ///
    /// Every engineer who sees this default will push back on it;
    /// keep it configurable, keep it on, and log any attempt to turn it off as
    /// a governance event. That log line is worth more than the setting.
    #[error("config stale past budget, failing closed at generation {generation}")]
    StaleConfigFailClosed { generation: u64 },
    #[error("no snapshot yet: cold start fails closed")]
    ColdStart,
}

/// Defaults from spec §6. Configurable per tenant; the values a customer runs
/// with go in their technical file, so they must be readable back out of the
/// config, not hardcoded here at the call site.
pub const DEFAULT_STALENESS_BUDGET: Duration = Duration::from_secs(30);
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);
