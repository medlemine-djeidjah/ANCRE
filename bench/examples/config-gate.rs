//! The control-plane acceptance gate. mvp-plan §5, M4; PRD §8.
//!
//! > The gateway serves through a total control-plane outage up to the
//! > staleness budget.
//!
//! Four things, in the order they would fail in production:
//!
//! 1. A generation reaches a gateway node fast enough that the 10s poll is a
//!    backstop and not the mechanism.
//! 2. A dead control plane does not stop the gateway — it starts a clock. Past
//!    the budget, High risk fails closed and everything else is served with
//!    `stale_config` on every event.
//! 3. A forged snapshot is refused and does not displace the installed one.
//! 4. Applying a generation writes evidence: the right chain, both content
//!    hashes, the measured `propagation_ms`, and the substantial-modification
//!    candidate surfaced rather than declared.
//!
//! Everything is in-process: no broker, no network, no Postgres. The number in
//! (1) is therefore build-and-install time, not wire time, and it is reported
//! as such. The staleness budget here is 200ms rather than the 30s default, so
//! the gate takes a second rather than a minute; the behaviour either side of
//! it is what is being checked, not the constant.
//!
//! ```sh
//! cargo run -p ancre-bench --release --example config-gate
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use ancre_bench::Percentiles;
use ancre_control::envelope::testing::MemoryBus;
use ancre_control::registry::testing::MemoryRegistry;
use ancre_control::{Published, SnapshotBuilder};
use ancre_gateway::config_feed::{ConfigFeed, FeedError, SnapshotSource};
use ancre_gateway::telemetry::TelemetryFork;
use ancre_resolver::{PinResolver, ResolveError, StalenessPolicy, testing};
use ancre_types::{EventType, RiskFlag, SnapshotEnvelope};

/// Big enough that a snapshot build is real work, small enough that 200
/// generations run in a few seconds.
const SYSTEMS: usize = 1_000;
const ROUTES: usize = 5;
const GENERATIONS: usize = 200;

/// Short, so the gate is quick. Production default is 30s.
const BUDGET: Duration = Duration::from_millis(200);

struct ControlPlane(SnapshotBuilder<MemoryRegistry, MemoryBus>);

impl SnapshotSource for ControlPlane {
    async fn fetch(&self) -> Result<SnapshotEnvelope, FeedError> {
        self.0
            .current()
            .await
            .map_err(|e| FeedError::Unreachable(e.to_string()))
    }
}

#[tokio::main(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "four phases of one scenario read better in one place"
)]
async fn main() -> std::process::ExitCode {
    let spec = testing::large_spec(SYSTEMS, ROUTES);
    let control = ControlPlane(SnapshotBuilder::new(
        MemoryRegistry::new(spec.systems, spec.keys),
        MemoryBus::default(),
        "0.1.0+abc123",
    ));

    let policy = StalenessPolicy {
        budget: BUDGET,
        fail_closed_on_stale: true,
    };
    let resolver = Arc::new(PinResolver::cold(policy, 64 * 1024 * 1024));
    let (fork, mut rx) = TelemetryFork::new(1 << 16);
    let feed = ConfigFeed::new(control, Arc::clone(&resolver), fork, "gw-1");

    // -----------------------------------------------------------------
    println!("phase 1: {GENERATIONS} generations over {SYSTEMS} systems");
    // -----------------------------------------------------------------
    let mut propagation: Vec<Duration> = Vec::with_capacity(GENERATIONS);
    let mut installed = 0u64;

    for i in 0..GENERATIONS {
        // A real edit, so the content hash moves and a generation is minted.
        feed.source()
            .0
            .registry()
            .edit(|systems| systems[i % SYSTEMS].system_version = format!("2.1.{i}"));

        let published = feed.source().0.publish().await.unwrap();
        assert!(matches!(published, Published::Bumped { .. }));

        let started = Instant::now();
        let applied = feed
            .poll_once()
            .await
            .expect("the control plane is up")
            .expect("a new generation must install");
        propagation.push(started.elapsed());

        installed = applied.generation;
        let _ = rx.drain_now();
    }
    let propagation = Percentiles::from_samples(&mut propagation);

    // -----------------------------------------------------------------
    println!("phase 2: control plane dies, budget is {BUDGET:?}");
    // -----------------------------------------------------------------
    feed.source().0.registry().kill();

    let poll_failed = feed.poll_once().await.is_err();
    let high_before = resolver.resolve(&testing::request_for_large(0)).is_ok();
    let low_before = resolver.resolve(&testing::request_for_large(1)).is_ok();

    tokio::time::sleep(BUDGET + Duration::from_millis(50)).await;

    let high_after = resolver.resolve(&testing::request_for_large(0));
    let low_after = resolver.resolve(&testing::request_for_large(1));

    // -----------------------------------------------------------------
    println!("phase 3: a forged snapshot arrives");
    // -----------------------------------------------------------------
    feed.source().0.registry().revive();
    let mut forged = SnapshotEnvelope::seal_now(testing::spec(installed + 1)).unwrap();
    forged.spec.systems[0].routes[0].model_id = "attacker-chosen-model".into();
    let refused = feed.apply(forged).is_err();
    let generation_after_forgery = resolver.generation();

    // -----------------------------------------------------------------
    println!("phase 4: a model swap, and what it writes");
    // -----------------------------------------------------------------
    let before_hash = resolver.snapshot().content_hash;
    feed.source().0.registry().edit(|systems| {
        systems[0].routes[0].model_id = "claude-opus-5".into();
    });
    feed.source().0.publish().await.unwrap();
    // Deliberately not unwrapped: if an earlier phase left the node holding a
    // snapshot it should have refused, this poll finds nothing new — and that
    // has to be reported as a failed check, not a panicked gate.
    let applied = feed.poll_once().await.ok().flatten();
    let events = rx.drain_now();

    let swap_event = events.iter().find(|e| {
        e.event_type == EventType::ConfigGenerationApplied
            && e.pins.risk_flags.contains(&RiskFlag::SubstantialCandidate)
    });

    // -----------------------------------------------------------------
    println!("\nresults");
    println!("  propagation:                {propagation}");
    println!("  generations installed:      {installed}");
    println!("  events from the model swap: {}", events.len());
    println!();

    let mut ok = true;

    // The poll is a backstop, not the mechanism: a generation has to land in
    // far less than one poll interval or the staleness budget is being spent
    // on the delivery path.
    ok &= ancre_bench::check(
        "propagation p99",
        propagation.p99,
        Duration::from_millis(250),
    );
    ok &= expect(
        "every generation installed in order",
        installed == GENERATIONS as u64,
        &format!("installed {installed} of {GENERATIONS}"),
    );

    ok &= expect(
        "a dead control plane is a failed poll, not a failed gateway",
        poll_failed && high_before && low_before,
        "the gateway stopped serving while still inside the budget",
    );
    ok &= expect(
        "past the budget, a High-risk system fails closed",
        matches!(high_after, Err(ResolveError::StaleConfigFailClosed { .. })),
        &format!("{high_after:?}"),
    );
    ok &= expect(
        "past the budget, a lower-risk system is served and flagged stale",
        low_after
            .as_ref()
            .is_ok_and(|p| p.resolved_stale && p.risk_flags.contains(&RiskFlag::StaleConfig)),
        &format!("{low_after:?}"),
    );

    ok &= expect(
        "a forged snapshot is refused",
        refused,
        "the envelope was installed",
    );
    ok &= expect(
        "a forged snapshot does not displace the installed generation",
        generation_after_forgery == installed,
        &format!("generation is {generation_after_forgery}, expected {installed}"),
    );

    ok &= expect(
        "the model swap is recorded in the affected chain",
        swap_event.is_some_and(|e| &*e.system_id == "system-000000" && &*e.tenant_id == "acme"),
        &format!("{:?}", swap_event.map(|e| e.system_id.to_string())),
    );
    ok &= expect(
        "the event pins both configurations of the transition",
        swap_event.is_some_and(|e| {
            e.request_digest == before_hash && e.response_digest == resolver.snapshot().content_hash
        }),
        "the before/after content hashes are not both pinned",
    );
    ok &= expect(
        "the change is surfaced as a candidate, never declared",
        swap_event.is_some_and(|e| &*e.metrics.error_code == "substantial"),
        "no change class recorded",
    );
    ok &= expect(
        "propagation_ms is recorded on the event",
        swap_event.is_some_and(|e| u64::from(e.metrics.latency_ms) < 5_000)
            && applied
                .as_ref()
                .is_some_and(|a| a.outcome.propagation_ms < 5_000),
        "propagation_ms is missing or implausible",
    );

    println!(
        "\n  Note: in-process. No broker, no network, no Postgres — phase 1\n  \
         measures build-and-install time, not wire time. The staleness budget\n  \
         here is {BUDGET:?}; the shipped default is 30s."
    );

    println!();
    if ok {
        println!("all targets met");
        std::process::ExitCode::SUCCESS
    } else {
        println!("ACCEPTANCE FAILED — see mvp-plan §5, M4 and PRD §8");
        std::process::ExitCode::FAILURE
    }
}

fn expect(label: &str, ok: bool, detail: &str) -> bool {
    println!(
        "  [{}] {label:<58} {}",
        if ok { "PASS" } else { "FAIL" },
        if ok { "" } else { detail }
    );
    ok
}
