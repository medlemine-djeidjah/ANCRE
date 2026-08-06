//! Gateway entry point.
//!
//! Cold start fails closed: the resolver is installed before the socket is
//! bound, so a node that has never loaded a snapshot is not listening rather
//! than listening-and-refusing (spec §6).

use std::net::SocketAddr;
use std::sync::Arc;

use ancre_gateway::proxy::GatewayState;
use ancre_gateway::telemetry::{BatchConfig, Batcher, EventSink, SinkError, TelemetryFork};
use ancre_gateway::upstream::{Endpoints, HttpsUpstream};
use ancre_resolver::{PinResolver, StalenessPolicy};
use ancre_types::EmittedEvent;

/// Stands in for the NATS JetStream publisher until M4 wires the bus.
///
/// Deliberately loud: a gateway that silently discards evidence looks exactly
/// like one that is working. Every batch is counted and logged, so running
/// this build in front of real traffic is an obvious mistake rather than a
/// quiet one.
#[derive(Debug, Default)]
struct LoggingSink {
    published: std::sync::atomic::AtomicU64,
}

impl EventSink for LoggingSink {
    async fn publish(&self, batch: Vec<EmittedEvent>) -> Result<(), SinkError> {
        let total = self
            .published
            .fetch_add(batch.len() as u64, std::sync::atomic::Ordering::Relaxed)
            + batch.len() as u64;
        tracing::warn!(
            batch = batch.len(),
            total,
            "audit events discarded: no bus configured (M4 wires NATS)"
        );
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ancre_gateway=debug".into()),
        )
        .init();

    let addr: SocketAddr = std::env::var("ANCRE_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()?;

    let (fork, rx) = TelemetryFork::new(65_536);
    tokio::spawn(Batcher::new(rx, LoggingSink::default(), BatchConfig::default()).run());

    // TODO(M4): subscribe to the control plane and reload on `generation`
    // bumps, with the 10s poll as a backstop. Until then the resolver stays
    // cold, which means every request is refused — correct, and loudly so.
    let resolver = Arc::new(PinResolver::cold(
        StalenessPolicy::default(),
        64 * 1024 * 1024,
    ));

    let state = Arc::new(GatewayState {
        resolver,
        telemetry: fork,
        upstream: HttpsUpstream::new(Endpoints::default())?,
        node_id: std::env::var("ANCRE_NODE_ID")
            .unwrap_or_else(|_| "gw-1".into())
            .into(),
        pinning_enabled: true,
    });

    tracing::warn!(
        "no control plane configured: the resolver is cold, so every request \
         will be refused with 503 until M4 wires snapshot delivery"
    );

    ancre_gateway::serve::serve(addr, state, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
