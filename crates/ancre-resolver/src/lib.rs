//! In-path version-pin resolver. Implements `version-pin-resolver-spec.md`.
//!
//! Budget: p50 < 2µs, p99 < 5µs, p99 < 8µs under a 10/s reload storm.
//! The whole latency claim — and therefore the whole technical
//! differentiator — descends from these numbers. Benches gate CI.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ancre_types::{ConfigSnapshot, IngressMeta, Pins, RiskFlag};
use arc_swap::ArcSwap;
use smallvec::SmallVec;

pub mod diff;
pub mod prompts;
pub mod staleness;
#[cfg(feature = "testing")]
pub mod testing;

pub use diff::{ReloadOutcome, SystemChange, classify_change};
pub use staleness::{Freshness, StalenessPolicy};

/// A snapshot plus the local monotonic instant it was installed at.
///
/// Kept together and swapped together, so a reader can never see one
/// snapshot paired with another snapshot's load time.
#[derive(Debug)]
#[allow(clippy::struct_field_names, reason = "installed_at/is_cold read worse")]
struct Loaded {
    snap: Arc<ConfigSnapshot>,
    loaded_at: Instant,
    /// True only for the placeholder installed before the first real snapshot.
    cold: bool,
}

#[derive(Debug)]
pub struct PinResolver {
    current: ArcSwap<Loaded>,
    policy: StalenessPolicy,
    prompts: prompts::PromptCache,
}

impl PinResolver {
    /// Cold start: no snapshot yet → fail closed for **all** systems. Never
    /// serve traffic with `unknown` pins on a high-risk system (spec §6).
    ///
    /// The cold state is an installed snapshot that refuses, not an
    /// `Option<Snapshot>`. An `Option` puts a branch on the hot path and, far
    /// worse, invites a future `unwrap_or_default()` that quietly serves
    /// traffic with empty pins.
    #[must_use]
    pub fn cold(policy: StalenessPolicy, prompt_cache_bytes: u64) -> Self {
        Self {
            current: ArcSwap::from_pointee(Loaded {
                snap: Arc::new(ConfigSnapshot::empty()),
                loaded_at: Instant::now(),
                cold: true,
            }),
            policy,
            prompts: prompts::PromptCache::new(prompt_cache_bytes),
        }
    }

    /// Ready to serve. For tests, benches, and any caller that already has a
    /// snapshot in hand.
    #[must_use]
    pub fn with_snapshot(snapshot: ConfigSnapshot, policy: StalenessPolicy) -> Self {
        let r = Self::cold(policy, 64 * 1024 * 1024);
        r.reload(snapshot);
        r
    }

    /// The hot path. One atomic load, two hash lookups, a short linear scan
    /// over routes, ~12 refcount increments.
    ///
    /// `ArcSwap::load()` is an atomic load plus a hazard-pointer guard — tens
    /// of nanoseconds, no inter-core contention. Do **not** reach for
    /// `RwLock<Arc<T>>`: even uncontended, the read lock touches a shared
    /// cache line, and above ~350 RPS across cores that lands in the p99 you
    /// are selling. The reload-storm bench is what catches this.
    #[inline]
    pub fn resolve(&self, req: &IngressMeta<'_>) -> Result<Pins, ResolveError> {
        let cur = self.current.load();
        if cur.cold {
            return Err(ResolveError::ColdStart);
        }
        let snap = &cur.snap;

        let binding = snap
            .key_binding(&req.api_key_hash)
            .ok_or(ResolveError::UnknownKey)?;
        let system = snap
            .system(&binding.system_id)
            .ok_or_else(|| ResolveError::UnknownSystem(binding.system_id.to_string()))?;

        let freshness = self.policy.freshness(cur.loaded_at);
        if !self.policy.admits(freshness, system.risk_class) {
            return Err(ResolveError::StaleConfigFailClosed {
                generation: snap.generation,
            });
        }
        let stale = freshness == Freshness::Stale;

        // First match wins. `default_route` is guaranteed in bounds by
        // `ConfigSnapshot::build`, so this cannot panic.
        let route = system
            .routes
            .iter()
            .find(|r| r.matcher.matches(req))
            .unwrap_or(&system.routes[system.default_route]);

        let mut risk_flags: SmallVec<[RiskFlag; 4]> = SmallVec::new();
        if stale {
            risk_flags.push(RiskFlag::StaleConfig);
        }
        if !req.overrides.is_empty() {
            risk_flags.push(RiskFlag::PinOverridden);
        }

        // Precedence: request header > route rule (spec §4). A caller-supplied
        // pin allocates, which is fine — an override is rare by construction,
        // and if it ever stops being rare, that is itself the finding.
        let model_version = req
            .overrides
            .model_version
            .map_or_else(|| Arc::clone(&route.model_version), Arc::from);
        let prompt_version = req
            .overrides
            .prompt_version
            .map_or_else(|| Arc::clone(&route.prompt_version), Arc::from);

        Ok(Pins {
            config_generation: snap.generation,
            config_hash: snap.content_hash,
            system_id: Arc::clone(&system.system_id),
            system_version: Arc::clone(&system.system_version),
            ifu_version: Arc::clone(&system.ifu_version),
            model_id: Arc::clone(&route.model_id),
            model_version,
            prompt_id: Arc::clone(&route.prompt_id),
            prompt_version,
            policy_id: Arc::clone(&system.policy_id),
            policy_version: Arc::clone(&system.policy_version),
            gateway_version: Arc::clone(&snap.gateway_version),
            risk_class: system.risk_class,
            resolved_stale: stale,
            risk_flags,
        })
    }

    /// Build off-thread, then swap. The previous snapshot stays alive as long
    /// as an in-flight request holds a reference: a request that loaded
    /// generation 41 keeps reading 41 until it completes, even if 42 lands
    /// mid-stream. That is the atomicity guarantee, and it is why a 60-second
    /// streaming completion reports the generation it started under.
    pub fn reload(&self, next: ConfigSnapshot) -> ReloadOutcome {
        let prev = self.current.load_full();
        let next = Arc::new(next);

        let outcome = if prev.cold {
            ReloadOutcome::first_snapshot(&next)
        } else {
            diff::diff(&prev.snap, &next)
        };

        self.current.store(Arc::new(Loaded {
            snap: next,
            loaded_at: Instant::now(),
            cold: false,
        }));
        outcome
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.current.load().snap.generation
    }

    #[must_use]
    pub fn is_cold(&self) -> bool {
        self.current.load().cold
    }

    #[must_use]
    pub fn freshness(&self) -> Freshness {
        let cur = self.current.load();
        if cur.cold {
            Freshness::ColdStart
        } else {
            self.policy.freshness(cur.loaded_at)
        }
    }

    #[must_use]
    pub fn prompts(&self) -> &prompts::PromptCache {
        &self.prompts
    }

    /// The current snapshot. Holds it alive for as long as the caller keeps
    /// the `Arc` — which is exactly the guarantee the request path relies on.
    #[must_use]
    pub fn snapshot(&self) -> Arc<ConfigSnapshot> {
        Arc::clone(&self.current.load().snap)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("unknown api key")]
    UnknownKey,
    #[error("unknown system: {0}")]
    UnknownSystem(String),
    /// Served as a 503. High-risk system, config older than the budget,
    /// control plane unreachable.
    ///
    /// Every engineer who sees this default will push back on it; keep it
    /// configurable, keep it on, and log any attempt to turn it off as a
    /// governance event. That log line is worth more than the setting.
    #[error("config stale past budget, failing closed at generation {generation}")]
    StaleConfigFailClosed { generation: u64 },
    #[error("no snapshot yet: cold start fails closed")]
    ColdStart,
}

impl ResolveError {
    /// What the gateway returns.
    ///
    /// 401 for an unknown key; **503** for "we cannot pin this right now".
    /// The latter is retryable and must not be reported as a client error, or
    /// the customer's dashboards will blame their own code for our refusal.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::UnknownKey => 401,
            Self::UnknownSystem(_) => 500,
            Self::StaleConfigFailClosed { .. } | Self::ColdStart => 503,
        }
    }
}

/// Defaults from spec §6. Configurable per tenant; the values a customer runs
/// with go in their technical file, so they must be readable back out of the
/// config, not hardcoded here at the call site.
pub const DEFAULT_STALENESS_BUDGET: Duration = Duration::from_secs(30);
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);
