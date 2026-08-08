//! Control plane entry point.
//!
//! Three datastores, two timers and a read API. Everything it does is off the
//! request path, which is what makes the failure policy simple: **a loop that
//! fails logs and comes back**, because a control plane that exits takes the
//! fleet's configuration updates with it, and the gateways it serves are
//! designed to survive its absence up to the staleness budget (PRD §8).
//!
//! Startup is the exception. A missing database, an unreadable signing key or
//! a broker that will not take a publish are refused loudly at boot, before
//! anything has been told this instance is healthy — starting successfully and
//! then serving 503 forever is the failure mode that gets diagnosed last.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ancre_chain::CheckpointSigner;
use ancre_control::api::{ControlState, router};
use ancre_control::{
    Checkpointer, ClickHouseChains, NatsSnapshotBus, PgStore, Published, SnapshotBuilder,
};
use ancre_types::Timestamp;

/// How often the registry is re-read and republished if it changed.
///
/// The registry is edited out of band in the MVP — there is no write API — so
/// polling it is the only way an edit is ever noticed. A rebuild that produces
/// the same bytes publishes nothing and allocates no generation, which is what
/// makes a 10-second loop safe to run forever.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(10);

/// How often the checkpointer is offered a turn. Not the checkpoint policy —
/// `tick` decides what is actually due (10 000 events or 5 minutes per chain)
/// and skips chains with nothing new, so ticking more often costs reads.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);

type Fatal = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), Fatal> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ancre_control=debug".into()),
        )
        .init();

    let addr: SocketAddr = env_or("ANCRE_LISTEN", "0.0.0.0:8081").parse()?;

    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL is required (postgres://user:pass@host/db)")?;
    let postgres = PgStore::connect(&database_url).await?;

    let mut chains = ClickHouseChains::new(
        &env_or("CLICKHOUSE_URL", "http://127.0.0.1:8123"),
        &env_or("CLICKHOUSE_DB", "ancre"),
    );
    if let (Ok(user), Ok(password)) = (
        std::env::var("CLICKHOUSE_USER"),
        std::env::var("CLICKHOUSE_PASSWORD"),
    ) {
        chains = chains.with_credentials(&user, &password);
    }

    let bus = NatsSnapshotBus::connect(&env_or("ANCRE_NATS_URL", "nats://127.0.0.1:4222")).await?;

    // The signing key, and the row that says which key is active from when.
    // Installed before the checkpointer starts, so no checkpoint can be signed
    // by a key the export does not yet carry.
    let signer = load_or_create_signer(&PathBuf::from(env_or(
        "ANCRE_SIGNING_KEY_PATH",
        "/var/lib/ancre/checkpoint-key",
    )))?;
    postgres
        .install_active_key(signer.key_id(), &signer.verifying_key(), Timestamp::now())
        .await?;
    tracing::info!(
        key_id = signer.key_id(),
        public_key = hex::encode(signer.verifying_key()),
        "checkpoint signing key active"
    );

    // One ClickHouse client, two readers: the checkpointer walks it on a timer
    // and the export endpoint streams from it on demand.
    let chains = std::sync::Arc::new(chains);
    let checkpointer = Checkpointer::new(std::sync::Arc::clone(&chains), postgres.clone(), signer);

    let state = Arc::new(ControlState {
        snapshots: SnapshotBuilder::new(
            postgres.clone(),
            bus,
            // E4: `+unknown` until a build.rs stamps the git SHA. Overridable
            // so a deployment that knows its build can say so.
            env_or("ANCRE_GATEWAY_VERSION", "0.1.0+unknown"),
        ),
        checkpoints: postgres.clone(),
        keys: postgres,
        chains,
    });

    // One publish before the socket is bound. A gateway that starts alongside
    // this one then finds a snapshot waiting instead of polling into a 503 —
    // and if the registry cannot be built at all, that is a boot failure and
    // not a mystery an operator finds later in the logs.
    match state.snapshots.publish().await {
        Ok(Published::Bumped { generation }) => {
            tracing::info!(generation, "published");
        }
        Ok(Published::Unchanged { generation }) => {
            tracing::info!(generation, "registry unchanged since the last publish");
        }
        Err(e) => return Err(format!("first publish failed: {e}").into()),
    }

    tokio::spawn(publish_loop(Arc::clone(&state)));
    tokio::spawn(checkpoint_loop(checkpointer));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "control plane listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Rebuild and republish on a timer. Errors are logged and the loop continues:
/// a registry that is briefly unreachable must not stop the one that comes
/// back, and the gateways are already covered by their staleness budget.
async fn publish_loop<R, B, X>(state: Arc<ControlState<SnapshotBuilder<R, B>, PgStore, PgStore, X>>)
where
    R: ancre_control::Registry + 'static,
    B: ancre_control::SnapshotBus + 'static,
    X: ancre_control::api::Chains,
{
    let mut ticker = tokio::time::interval(PUBLISH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match state.snapshots.publish().await {
            Ok(Published::Bumped { generation }) => {
                tracing::info!(generation, "published a new generation");
            }
            Ok(Published::Unchanged { .. }) => {}
            Err(e) => tracing::error!(error = %e, "publish failed"),
        }
    }
}

/// Seal what is due, and report the lag.
///
/// The lag is the metric to alert on: falling behind is not an error, but the
/// gap between a chain's head and its last signed checkpoint is exactly the
/// window in which tampering would go unattested.
async fn checkpoint_loop<S, C>(checkpointer: Checkpointer<S, C>)
where
    S: ancre_control::ChainSource,
    C: ancre_control::CheckpointStore,
{
    let mut ticker = tokio::time::interval(CHECKPOINT_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match checkpointer.tick(Timestamp::now()).await {
            Ok(report) if report.checkpoints_written > 0 => {
                tracing::info!(
                    chains = report.chains_examined,
                    checkpoints = report.checkpoints_written,
                    events = report.events_sealed,
                    max_lag = report.max_lag,
                    "sealed"
                );
            }
            Ok(report) => {
                tracing::debug!(
                    chains = report.chains_examined,
                    max_lag = report.max_lag,
                    "nothing due"
                );
            }
            Err(e) => tracing::error!(error = %e, "checkpoint tick failed"),
        }
    }
}

/// Load the ed25519 secret, or mint one and persist it.
///
/// Generating on first boot rather than refusing to start is a deliberate
/// trade: the alternative is a quickstart whose first step is a key ceremony.
/// What makes it safe is that the key is **written back to the path it was
/// asked for**, so a restart signs with the same key and the checkpoints it
/// already produced keep verifying. An ephemeral key would be the opposite —
/// every restart would orphan the previous run's checkpoints, and the export
/// would be the only place that showed it.
///
/// The `key_id` is derived from the public half, so the same secret always
/// carries the same name and two control planes sharing a key file agree about
/// what to call it.
fn load_or_create_signer(path: &Path) -> Result<CheckpointSigner, Fatal> {
    let secret = match std::fs::read(path) {
        Ok(contents) => parse_key(&contents).ok_or_else(|| {
            format!(
                "{}: expected 64 hex characters or 32 raw bytes",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut secret = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut secret);
            // Persisted before it is used to sign anything. A key that sealed
            // a checkpoint and then failed to reach disk is a checkpoint
            // nobody can ever verify.
            write_key(path, &secret)?;
            tracing::warn!(
                path = %path.display(),
                "no signing key found: generated one and wrote it here. Back this \
                 file up — without it, checkpoints signed by this instance cannot \
                 be attributed to a key an auditor holds"
            );
            secret
        }
        Err(e) => return Err(format!("{}: {e}", path.display()).into()),
    };

    // The id names the key, so it is derived from the public half rather than
    // configured: two control planes handed the same key file must agree about
    // what to call it, or `signing_keys` gets two rows for one key and an
    // auditor cannot tell which window applies.
    let probe = CheckpointSigner::from_bytes(secret, String::new());
    let key_id = format!("cp-{}", &hex::encode(probe.verifying_key())[..16]);
    Ok(CheckpointSigner::from_bytes(secret, key_id))
}

/// Hex or raw, because both are things an operator plausibly produces —
/// `openssl rand -hex 32` and `head -c 32 /dev/urandom` respectively. Anything
/// else is refused rather than hashed into a key: silently deriving a key from
/// whatever bytes were in the file would make a typo in a path produce a
/// working control plane signing with the wrong key.
fn parse_key(contents: &[u8]) -> Option<[u8; 32]> {
    if let Ok(text) = std::str::from_utf8(contents) {
        let trimmed = text.trim();
        if trimmed.len() == 64 {
            let mut out = [0u8; 32];
            if hex::decode_to_slice(trimmed, &mut out).is_ok() {
                return Some(out);
            }
        }
    }
    <[u8; 32]>::try_from(contents).ok()
}

/// Write hex, owner-readable only. Hex rather than raw so the file survives
/// being opened in an editor, and 0600 because the alternative is a private
/// key that a container's world-readable default umask publishes to every
/// process in the image.
fn write_key(path: &Path, secret: &[u8; 32]) -> Result<(), Fatal> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, hex::encode(secret)).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
