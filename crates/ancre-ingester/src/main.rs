//! Ingester entry point.
//!
//! The narrowest of the three binaries, and the one with the strictest
//! constraint: **one writer per chain.** Two processes appending to the same
//! `(tenant_id, system_id)` fork it, and a fork verifies perfectly on each side
//! of the split — the worst kind of corruption, because nothing reports it.
//!
//! So there is exactly one consumer task here, owning every `ChainWriter`, and
//! scaling past one ingester means giving each a slice of the subject space
//! (`ANCRE_SUBJECT_FILTER`) rather than pointing a second one at the same
//! subjects. The subject already carries the chain, so that is configuration
//! and not a wire change.

use std::process::ExitCode;

use ancre_ingester::bus::{ConsumerConfig, NatsConsumer};
use ancre_ingester::clickhouse::ClickHouseStore;
use ancre_ingester::pipeline::Ingester;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "ingester stopped");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ancre_ingester=debug".into()),
        )
        .init();

    let mut store = ClickHouseStore::new(
        &env_or("CLICKHOUSE_URL", "http://127.0.0.1:8123"),
        &env_or("CLICKHOUSE_DB", "ancre"),
    );
    if let (Ok(user), Ok(password)) = (
        std::env::var("CLICKHOUSE_USER"),
        std::env::var("CLICKHOUSE_PASSWORD"),
    ) {
        store = store.with_credentials(&user, &password);
    }

    // The node id goes into every event this process chains, so it must
    // identify the process and not the fleet. A hostname is what a container
    // orchestrator already gives, and "which replica wrote this" is a question
    // an incident asks.
    let node_id = std::env::var("ANCRE_NODE_ID").unwrap_or_else(|_| {
        std::env::var("HOSTNAME").unwrap_or_else(|_| format!("ing-{}", std::process::id()))
    });

    let config = ConsumerConfig {
        filter_subject: env_or(
            "ANCRE_SUBJECT_FILTER",
            &ConsumerConfig::default().filter_subject,
        ),
        durable_name: env_or(
            "ANCRE_CONSUMER_NAME",
            &ConsumerConfig::default().durable_name,
        ),
        ..ConsumerConfig::default()
    };

    let url = env_or("ANCRE_NATS_URL", "nats://127.0.0.1:4222");
    let consumer =
        NatsConsumer::connect(&url, Ingester::new(store, node_id.clone()), config.clone()).await?;

    tracing::info!(
        node_id,
        nats = url,
        durable = config.durable_name,
        filter = config.filter_subject,
        "ingester consuming"
    );

    let report = consumer
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;

    // The shutdown line is the one an operator reads after a deploy. `deferred`
    // is the number that matters: non-zero means batches were handed back
    // because the store refused them, and those events are still on the stream
    // waiting for the next process.
    tracing::info!(
        batches = report.batches,
        messages = report.messages,
        inserted = report.inserted,
        duplicates = report.duplicates,
        redelivered = report.redelivered,
        undecodable = report.undecodable,
        "ingester stopped"
    );
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
