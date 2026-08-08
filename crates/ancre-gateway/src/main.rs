//! Gateway entry point.
//!
//! Cold start fails closed: the first snapshot is installed **before** the
//! socket is bound, so a node that has never loaded a configuration is not
//! listening rather than listening-and-refusing (spec §6). A load balancer
//! reads a closed port as "not ready"; it reads a port that answers 503 as a
//! node worth sending traffic to.
//!
//! Everything after that point is designed to keep serving through failures of
//! everything else: a dead control plane costs staleness, not availability, and
//! a dead bus costs evidence, countably, not latency.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ancre_gateway::config_feed::{ConfigFeed, SnapshotSource};
use ancre_gateway::control::HttpSnapshotSource;
use ancre_gateway::proxy::GatewayState;
use ancre_gateway::telemetry::{BatchConfig, Batcher, TelemetryFork};
use ancre_gateway::upstream::{Endpoints, HttpsUpstream};
use ancre_resolver::{PinResolver, StalenessPolicy};

type Fatal = Box<dyn std::error::Error + Send + Sync>;

/// The poll backstop's interval. The bus normally delivers a generation in
/// milliseconds; this is what bounds staleness when it does not.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// How long to keep retrying the first snapshot before giving up and exiting.
///
/// Exiting is correct here and only here: a node that cannot get its first
/// configuration has nothing to serve, and an orchestrator restarting it is a
/// better outcome than a process that stays up forever in a state it can never
/// leave. Once a snapshot is installed, nothing about the control plane is
/// fatal again.
const COLD_START_BUDGET: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> Result<(), Fatal> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ancre_gateway=debug".into()),
        )
        .init();

    let addr: SocketAddr = env_or("ANCRE_LISTEN", "0.0.0.0:8080").parse()?;
    let node_id = std::env::var("ANCRE_NODE_ID").unwrap_or_else(|_| {
        std::env::var("HOSTNAME").unwrap_or_else(|_| format!("gw-{}", std::process::id()))
    });

    // Evidence first. The batcher is running before the socket is bound, so no
    // request can be served by a process that has nowhere to put its events.
    let nats_url = env_or("ANCRE_NATS_URL", "nats://127.0.0.1:4222");
    let sink = Arc::new(ancre_gateway::bus::NatsSink::connect(&nats_url).await?);
    let (fork, rx) = TelemetryFork::new(65_536);
    tokio::spawn(Batcher::new(rx, Arc::clone(&sink), BatchConfig::default()).run());

    let resolver = Arc::new(PinResolver::cold(staleness_policy(), 64 * 1024 * 1024));

    let control_url = env_or("ANCRE_CONTROL_URL", "http://127.0.0.1:8081");
    let feed = Arc::new(ConfigFeed::new(
        HttpSnapshotSource::new(&control_url)?,
        Arc::clone(&resolver),
        fork.clone(),
        node_id.clone(),
    ));

    cold_start(&feed, &control_url).await?;

    // The fast path and the backstop, both running. Either alone converges;
    // together the common case is milliseconds and the worst case is one poll.
    tokio::spawn(ancre_gateway::control::watch(
        nats_url.clone(),
        Arc::clone(&feed),
        shutdown_signal(),
    ));
    tokio::spawn({
        let feed = Arc::clone(&feed);
        async move { feed.run(POLL_INTERVAL, shutdown_signal()).await }
    });

    let state = Arc::new(GatewayState {
        resolver,
        telemetry: fork,
        upstream: HttpsUpstream::new(endpoints())?,
        node_id: node_id.into(),
        pinning_enabled: true,
    });

    ancre_gateway::serve::serve(addr, state, shutdown_signal()).await
}

/// Retry the first snapshot until it lands or the budget runs out.
///
/// A control plane that is still starting is the normal case in a compose
/// file, not an error, so this waits rather than failing on the first refused
/// connection. What it will not do is give up quietly and start serving: a
/// gateway with no configuration cannot resolve a pin, and a request it cannot
/// pin is a request it must refuse.
async fn cold_start<S: SnapshotSource>(feed: &ConfigFeed<S>, url: &str) -> Result<(), Fatal> {
    let deadline = tokio::time::Instant::now() + COLD_START_BUDGET;
    let mut last = String::new();

    while tokio::time::Instant::now() < deadline {
        match feed.poll_once().await {
            Ok(Some(applied)) => {
                tracing::info!(
                    generation = applied.generation,
                    "first snapshot installed; binding the socket"
                );
                return Ok(());
            }
            // `poll_once` returns None only for a generation already
            // installed, which cannot happen on a cold resolver.
            Ok(None) => unreachable!("a cold resolver has no generation to skip"),
            Err(e) => {
                if last != e.to_string() {
                    last = e.to_string();
                    tracing::warn!(error = %last, control = url, "waiting for the control plane");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    Err(format!(
        "no snapshot from {url} within {}s: refusing to serve traffic this node cannot pin",
        COLD_START_BUDGET.as_secs()
    )
    .into())
}

/// Where the providers live.
///
/// Overridable because the base URL is the whole adoption story — a customer
/// running Azure OpenAI or a self-hosted vLLM must not need a code change, and
/// a quickstart must not need somebody's API key to show a verified chain.
fn endpoints() -> Endpoints {
    let defaults = Endpoints::default();
    let endpoints = Endpoints {
        openai: env_or("ANCRE_OPENAI_BASE", &defaults.openai),
        anthropic: env_or("ANCRE_ANTHROPIC_BASE", &defaults.anthropic),
    };
    if endpoints.openai != defaults.openai || endpoints.anthropic != defaults.anthropic {
        tracing::info!(
            openai = endpoints.openai,
            anthropic = endpoints.anthropic,
            "provider endpoints overridden"
        );
    }
    endpoints
}

/// Fail closed on stale config for High-risk systems, defaulted **on**.
///
/// Turning it off is a governance decision, so it is an explicit environment
/// variable and not a silent default — and the value is logged at startup,
/// because "the gateway was configured to serve stale pins that day" is
/// something an incident review has to be able to establish.
fn staleness_policy() -> StalenessPolicy {
    let budget = std::env::var("ANCRE_STALENESS_BUDGET_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(StalenessPolicy::default().budget, Duration::from_secs);
    let fail_closed = env_or("ANCRE_FAIL_CLOSED_ON_STALE", "true") != "false";

    if !fail_closed {
        tracing::warn!(
            "ANCRE_FAIL_CLOSED_ON_STALE=false: High-risk systems will be served on stale \
             configuration. This is a governance decision and it is recorded here"
        );
    }
    tracing::info!(
        budget_secs = budget.as_secs(),
        fail_closed,
        "staleness policy"
    );

    StalenessPolicy {
        budget,
        fail_closed_on_stale: fail_closed,
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
