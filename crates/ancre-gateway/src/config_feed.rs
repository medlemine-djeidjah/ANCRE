//! Snapshot delivery: subscribe, verify, install, and record that it happened.
//!
//! Two paths on purpose. The bus delivers a generation in milliseconds; the
//! poll is a **backstop**, because a missed message must never mean indefinite
//! staleness (PRD §8). A gateway with a dead bus and a live control plane
//! converges in one poll interval. A gateway with both dead keeps serving
//! until the staleness budget runs out and then fails closed for High risk —
//! which is the behaviour the customer's technical file describes, so it is
//! implemented here rather than left to a retry loop's accident.
//!
//! Applying a generation is also an *event*. Article 12(2)(a) wants changes
//! that may constitute a substantial modification to be identifiable from the
//! logs, and a configuration change nobody recorded is exactly the thing that
//! makes a pin unexplainable six months later.

use std::sync::Arc;
use std::time::Duration;

use ancre_resolver::{PinResolver, ReloadOutcome};
use ancre_types::{
    ChangeClass, ConfigSnapshot, EmittedEvent, EventType, Metrics, Outcome, Pins, RiskFlag,
    SnapshotEnvelope, SnapshotError, Timestamp,
};

use crate::telemetry::TelemetryFork;

/// Where a snapshot comes from. A trait so the feed is testable without a
/// control plane, and so the NATS client stays out of these tests.
pub trait SnapshotSource: Send + Sync {
    /// The currently published snapshot. `GET /v1/snapshot`.
    fn fetch(
        &self,
    ) -> impl std::future::Future<Output = Result<SnapshotEnvelope, FeedError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("control plane unreachable: {0}")]
    Unreachable(String),
    /// The envelope did not describe its own contents, or would not build.
    /// **Not installed.** A refused snapshot leaves the previous one in place
    /// and lets the staleness budget do its job.
    #[error("snapshot refused: {0}")]
    Refused(#[from] SnapshotError),
}

/// What one successful apply did.
#[derive(Debug)]
pub struct Applied {
    pub generation: u64,
    pub outcome: ReloadOutcome,
    /// `config.generation.applied` events handed to the telemetry fork.
    pub events: usize,
}

pub struct ConfigFeed<S> {
    source: S,
    resolver: Arc<PinResolver>,
    telemetry: TelemetryFork,
    node_id: Arc<str>,
}

impl<S> std::fmt::Debug for ConfigFeed<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFeed")
            .field("node_id", &self.node_id)
            .field("generation", &self.resolver.generation())
            .finish_non_exhaustive()
    }
}

impl<S: SnapshotSource> ConfigFeed<S> {
    pub fn new(
        source: S,
        resolver: Arc<PinResolver>,
        telemetry: TelemetryFork,
        node_id: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            source,
            resolver,
            telemetry,
            node_id: node_id.into(),
        }
    }

    /// The snapshot source, for a caller that owns both ends — the config
    /// gate drives a control plane and a gateway in one process.
    #[must_use]
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Verify, install, diff, record.
    ///
    /// The order is the contract: nothing is installed that has not verified,
    /// and nothing is recorded that was not installed. An event describing a
    /// generation this node never served would be a lie in the evidence, which
    /// is worse than a gap in it.
    pub fn apply(&self, envelope: SnapshotEnvelope) -> Result<Applied, FeedError> {
        let generation = envelope.generation;
        let previous = self.resolver.snapshot();
        let next = envelope.install()?;

        let outcome = self.resolver.reload(next);
        let events = self.record(&previous, &outcome);

        Ok(Applied {
            generation,
            outcome,
            events,
        })
    }

    /// One poll: fetch and apply. A generation already installed is skipped
    /// without a reload, so the backstop does not churn `loaded_at` — the
    /// staleness clock must measure the config's age, not the poll loop's.
    ///
    /// Note what a failed poll does *not* do: it does not clear the current
    /// snapshot. The gateway keeps serving what it has until the budget runs
    /// out, which is the whole point of the budget.
    pub async fn poll_once(&self) -> Result<Option<Applied>, FeedError> {
        let envelope = self.source.fetch().await?;
        if envelope.generation == self.resolver.generation() && !self.resolver.is_cold() {
            return Ok(None);
        }
        self.apply(envelope).map(Some)
    }

    /// The backstop loop. Runs until `shutdown` resolves.
    pub async fn run(self, interval: Duration, shutdown: impl Future<Output = ()> + Send) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => return,
                _ = ticker.tick() => match self.poll_once().await {
                    Ok(Some(applied)) => tracing::info!(
                        generation = applied.generation,
                        propagation_ms = applied.outcome.propagation_ms,
                        substantial_candidates = applied.outcome.substantial_candidates().len(),
                        "configuration generation applied",
                    ),
                    Ok(None) => {}
                    // Warn, and keep the snapshot. Staleness is measured by the
                    // resolver and enforced per request; a failed poll is not a
                    // reason to stop serving traffic that is still inside the
                    // budget.
                    Err(e) => tracing::warn!(
                        error = %e,
                        generation = self.resolver.generation(),
                        "config poll failed; serving the installed snapshot until the \
                         staleness budget expires",
                    ),
                },
            }
        }
    }

    /// Emit one `config.generation.applied` per affected chain.
    ///
    /// Per *chain*, not per fleet: a chain is `(tenant_id, system_id)`, so an
    /// event about a system has to land in that system's chain or an auditor
    /// reading one system's history will not see the change that altered its
    /// pins.
    ///
    /// Nothing is emitted for an unchanged reload, or for the first snapshot
    /// after a cold start — `ReloadOutcome::first_snapshot` reports no changes
    /// on purpose, because a restart is not a configuration change, and
    /// reporting every system as new on every restart would bury the one time
    /// it means something.
    fn record(&self, previous: &ConfigSnapshot, outcome: &ReloadOutcome) -> usize {
        if outcome.is_empty() {
            return 0;
        }
        let current = self.resolver.snapshot();
        let now = Timestamp::now();
        let mut emitted = 0;

        for (tenant_id, system_id) in affected_chains(previous, &current, outcome) {
            let class = outcome
                .changes
                .iter()
                .find(|c| *c.system_id == *system_id)
                .map(|c| c.class);

            // A removed system has no config in the new snapshot. Its pins
            // describe what it *was*, under the generation that removed it —
            // "as of generation N, this system is no longer configured" is the
            // statement, and it needs both halves.
            let pins = current
                .system(&system_id)
                .or_else(|| previous.system(&system_id))
                .map_or_else(Pins::null_baseline, |system| Pins {
                    config_generation: current.generation,
                    config_hash: current.content_hash,
                    system_id: Arc::clone(&system.system_id),
                    system_version: Arc::clone(&system.system_version),
                    ifu_version: Arc::clone(&system.ifu_version),
                    model_id: Arc::clone(&system.routes[system.default_route].model_id),
                    model_version: Arc::clone(&system.routes[system.default_route].model_version),
                    prompt_id: Arc::clone(&system.routes[system.default_route].prompt_id),
                    prompt_version: Arc::clone(&system.routes[system.default_route].prompt_version),
                    policy_id: Arc::clone(&system.policy_id),
                    policy_version: Arc::clone(&system.policy_version),
                    gateway_version: Arc::clone(&current.gateway_version),
                    risk_class: system.risk_class,
                    resolved_stale: false,
                    risk_flags: if class == Some(ChangeClass::Substantial) {
                        // A finding in a readiness report, which is exactly
                        // what a risk flag is for. It says "review required" —
                        // it does not declare a substantial modification, and
                        // no code anywhere may treat it as one.
                        smallvec::smallvec![RiskFlag::SubstantialCandidate]
                    } else {
                        smallvec::SmallVec::new()
                    },
                });

            self.telemetry.emit(EmittedEvent {
                tenant_id: Arc::clone(&tenant_id),
                system_id: Arc::clone(&system_id),
                event_id: uuid::Uuid::new_v4(),
                trace_id: Arc::from(""),
                attempt_seq: 0,
                occurred_at: now,
                node_id: Arc::clone(&self.node_id),
                event_type: EventType::ConfigGenerationApplied,
                outcome: Outcome::Ok,
                pins,
                // The two configurations this transition sits between. The
                // frozen schema has no `changed_fields` column and will not
                // grow one — a hashed column added later changes the canonical
                // encoding for every chain ever written. It does not need one:
                // with both content hashes pinned, the field-level diff is
                // *recomputable* from the two snapshots the control plane
                // retains, and a derived answer that can be checked beats a
                // stored one that cannot.
                request_digest: previous.content_hash,
                response_digest: current.content_hash,
                metrics: Metrics {
                    provider: Arc::from(""),
                    http_status: 0,
                    // Control-plane build time to local install time. The
                    // evidence for the bounded-staleness claim, which is why
                    // it is measured and not asserted.
                    latency_ms: outcome.propagation_ms,
                    ttft_ms: 0,
                    tokens_in: 0,
                    tokens_out: 0,
                    // The change class, in the one LowCardinality column a
                    // non-error event leaves free. `GROUP BY error_code` over
                    // `config.generation.applied` is the query an auditor
                    // actually runs: "show me every change that may have been
                    // substantial".
                    error_code: Arc::from(class.map_or("none", ChangeClass::as_str)),
                },
            });
            emitted += 1;
        }

        emitted
    }
}

/// The chains an outcome touches: `(tenant_id, system_id)` pairs.
///
/// A system's tenant is only knowable through its key bindings, so a system
/// with no key bound to it produces no event — correct, and quietly so: with
/// no key there is no chain for the event to belong to, and inventing a tenant
/// to hold it would put a fabricated identifier in the evidence.
fn affected_chains(
    previous: &ConfigSnapshot,
    current: &ConfigSnapshot,
    outcome: &ReloadOutcome,
) -> Vec<(Arc<str>, Arc<str>)> {
    let mut out = Vec::new();
    for system_id in outcome
        .changes
        .iter()
        .map(|c| c.system_id.as_str())
        .chain(outcome.systems_added.iter().map(String::as_str))
        .chain(outcome.systems_removed.iter().map(String::as_str))
    {
        for binding in current.bindings().chain(previous.bindings()) {
            if *binding.system_id == *system_id
                && !out.iter().any(|(t, s): &(Arc<str>, Arc<str>)| {
                    **t == *binding.tenant_id && **s == *system_id
                })
            {
                out.push((
                    Arc::clone(&binding.tenant_id),
                    Arc::clone(&binding.system_id),
                ));
            }
        }
    }
    // Two nodes applying the same generation must emit the same set in the
    // same order — `bindings()` iterates a HashMap.
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancre_resolver::{StalenessPolicy, testing};
    use ancre_types::SnapshotSpec;

    struct Source(std::sync::Mutex<Result<SnapshotEnvelope, String>>);

    impl Source {
        fn new(spec: SnapshotSpec) -> Self {
            Self(std::sync::Mutex::new(Ok(
                SnapshotEnvelope::seal_now(spec).unwrap()
            )))
        }

        fn serve(&self, spec: SnapshotSpec) {
            *self.0.lock().unwrap() = Ok(SnapshotEnvelope::seal_now(spec).unwrap());
        }

        fn kill(&self) {
            *self.0.lock().unwrap() = Err("connection refused".into());
        }
    }

    impl SnapshotSource for Source {
        async fn fetch(&self) -> Result<SnapshotEnvelope, FeedError> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .map_err(FeedError::Unreachable)
        }
    }

    fn feed(spec: SnapshotSpec) -> (ConfigFeed<Source>, crate::telemetry::Receiver) {
        let (fork, rx) = TelemetryFork::new(1024);
        let resolver = Arc::new(PinResolver::cold(StalenessPolicy::default(), 1 << 20));
        (
            ConfigFeed::new(Source::new(spec), resolver, fork, "gw-1"),
            rx,
        )
    }

    #[tokio::test]
    async fn the_first_poll_installs_a_snapshot_and_leaves_cold_start() {
        let (feed, _rx) = feed(testing::spec(41));
        assert!(feed.resolver.is_cold());

        let applied = feed.poll_once().await.unwrap().unwrap();
        assert_eq!(applied.generation, 41);
        assert!(!feed.resolver.is_cold());
        assert_eq!(feed.resolver.generation(), 41);
    }

    /// A restart is not a configuration change.
    #[tokio::test]
    async fn the_first_snapshot_emits_no_events() {
        let (feed, _rx) = feed(testing::spec(41));
        assert_eq!(feed.poll_once().await.unwrap().unwrap().events, 0);
    }

    #[tokio::test]
    async fn polling_an_unchanged_generation_does_not_reload() {
        let (feed, _rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();

        assert!(feed.poll_once().await.unwrap().is_none());
        assert!(feed.poll_once().await.unwrap().is_none());
    }

    /// The bus is not trusted, and neither is the poll response.
    #[tokio::test]
    async fn a_tampered_envelope_is_refused_and_the_old_snapshot_stays_installed() {
        let (feed, _rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();

        let mut spec = testing::spec(42);
        spec.systems[0].routes[0].model_id = "claude-opus-5".into();
        let mut envelope = SnapshotEnvelope::seal_now(testing::spec(42)).unwrap();
        envelope.spec = spec; // hash now describes different contents

        assert!(matches!(feed.apply(envelope), Err(FeedError::Refused(_))));
        assert_eq!(
            feed.resolver.generation(),
            41,
            "a refused snapshot must not displace the installed one"
        );
    }

    #[tokio::test]
    async fn a_model_swap_emits_one_event_per_affected_chain() {
        let (feed, mut rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();

        let mut spec = testing::spec(42);
        spec.systems[0].routes[0].model_id = "claude-opus-5".into();
        feed.source.serve(spec);

        let applied = feed.poll_once().await.unwrap().unwrap();
        assert_eq!(applied.events, 1);

        let events = rx.drain_now();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, EventType::ConfigGenerationApplied);
        assert_eq!(&*e.system_id, testing::SYSTEM_A);
        assert_eq!(&*e.tenant_id, "acme");
        assert_eq!(e.pins.config_generation, 42);
        assert_eq!(&*e.metrics.error_code, "substantial");
        assert!(e.pins.risk_flags.contains(&RiskFlag::SubstantialCandidate));
    }

    /// The transition is pinned from both ends, so the field-level diff stays
    /// recomputable without a column the frozen schema does not have.
    #[tokio::test]
    async fn the_event_pins_both_configurations_of_the_transition() {
        let (feed, mut rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();
        let before = feed.resolver.snapshot().content_hash;

        let mut spec = testing::spec(42);
        spec.systems[0].system_version = "2.2.0".into();
        feed.source.serve(spec);
        feed.poll_once().await.unwrap();

        let e = rx.drain_now().pop().unwrap();
        assert_eq!(e.request_digest, before);
        assert_eq!(e.response_digest, feed.resolver.snapshot().content_hash);
        assert_ne!(e.request_digest, e.response_digest);
    }

    /// Surfaced, never declared: a prompt change is `material`, and only a
    /// model or major-version change carries the review flag.
    #[tokio::test]
    async fn a_material_change_is_recorded_without_the_substantial_flag() {
        let (feed, mut rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();

        let mut spec = testing::spec(42);
        spec.systems[0].routes[0].prompt_version = "b3:different".into();
        feed.source.serve(spec);
        feed.poll_once().await.unwrap();

        let e = rx.drain_now().pop().unwrap();
        assert_eq!(&*e.metrics.error_code, "material");
        assert!(e.pins.risk_flags.is_empty());
    }

    #[tokio::test]
    async fn a_removed_system_is_recorded_in_its_own_chain_before_it_disappears() {
        let (feed, mut rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();

        let mut spec = testing::spec(42);
        spec.systems.remove(1);
        spec.keys.remove(1);
        feed.source.serve(spec);

        assert_eq!(feed.poll_once().await.unwrap().unwrap().events, 1);
        let e = rx.drain_now().pop().unwrap();
        assert_eq!(&*e.system_id, testing::SYSTEM_B);
        assert_eq!(
            e.pins.config_generation, 42,
            "recorded under the generation that removed it"
        );
    }

    #[tokio::test]
    async fn propagation_is_measured_and_recorded() {
        let (feed, mut rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();

        let mut spec = testing::spec(42);
        spec.systems[0].system_version = "2.2.0".into();
        feed.source.serve(spec);
        feed.poll_once().await.unwrap();

        let e = rx.drain_now().pop().unwrap();
        assert!(
            e.metrics.latency_ms < 1_000,
            "propagation_ms must be the install delay, not a clock difference: {}",
            e.metrics.latency_ms
        );
    }

    /// The behaviour the customer's technical file describes: a dead control
    /// plane does not stop the gateway, it starts the staleness clock.
    #[tokio::test]
    async fn a_dead_control_plane_leaves_the_installed_snapshot_serving() {
        let (feed, _rx) = feed(testing::spec(41));
        feed.poll_once().await.unwrap();
        feed.source.kill();

        for _ in 0..5 {
            assert!(matches!(
                feed.poll_once().await,
                Err(FeedError::Unreachable(_))
            ));
        }
        assert_eq!(feed.resolver.generation(), 41);
        assert!(!feed.resolver.is_cold());
    }

    /// Two nodes applying the same generation must produce the same events, or
    /// an auditor comparing two nodes' chains sees a difference that is really
    /// a HashMap's iteration order.
    #[tokio::test]
    async fn the_emitted_set_is_stable_across_hashmap_ordering() {
        let mut spec = testing::spec(42);
        spec.systems[0].routes[0].model_id = "claude-opus-5".into();
        spec.systems[1].routes[0].model_id = "claude-opus-5".into();

        let mut first: Option<Vec<(String, String)>> = None;
        for _ in 0..25 {
            let (feed, mut rx) = feed(testing::spec(41));
            feed.poll_once().await.unwrap();
            feed.source.serve(spec.clone());
            feed.poll_once().await.unwrap();

            let ids: Vec<(String, String)> = rx
                .drain_now()
                .iter()
                .map(|e| (e.tenant_id.to_string(), e.system_id.to_string()))
                .collect();
            assert_eq!(ids.len(), 2);
            match &first {
                None => first = Some(ids),
                Some(f) => assert_eq!(*f, ids),
            }
        }
    }
}
