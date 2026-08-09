//! Ancre control plane.
//!
//! Registry CRUD, key management, snapshot build and publication, checkpoint
//! signing. **Off the request path entirely** — the gateway serves through a
//! total control-plane outage up to the staleness budget (PRD §8), which is
//! why every failure in here is an alert and none of them is a 5xx a customer
//! sees.
//!
//! Two boundaries are traits — the registry and the chain source — so that the
//! properties worth testing are testable without Postgres or ClickHouse:
//! determinism of the build, monotonicity of the generation, and the exact
//! abutment of checkpoint ranges.

pub mod api;
pub mod auth;
pub mod bus;
pub mod checkpointer;
pub mod clickhouse;
pub mod envelope;
pub mod export;
pub mod keys;
pub mod overview;
pub mod postgres;
pub mod registry;
pub mod snapshot;
pub mod ui;

pub use ancre_types::{SNAPSHOT_SUBJECT, SnapshotEnvelope};
pub use bus::NatsSnapshotBus;
pub use checkpointer::{ChainSource, CheckpointStore, Checkpointer, PublicKeyRecord, SealReport};
pub use clickhouse::ClickHouseChains;
pub use envelope::SnapshotBus;
pub use export::ChainExport;
pub use keys::KeyRing;
pub use overview::{ChainListing, ChainOverview, ChainSummary};
pub use postgres::PgStore;
pub use registry::{ControlError, Registry};
pub use snapshot::{Published, SnapshotBuilder};
