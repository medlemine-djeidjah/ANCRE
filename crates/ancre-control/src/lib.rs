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
pub mod checkpointer;
pub mod envelope;
pub mod keys;
pub mod registry;
pub mod snapshot;

pub use ancre_types::{SNAPSHOT_SUBJECT, SnapshotEnvelope};
pub use checkpointer::{Checkpointer, PublicKeyRecord, SealReport};
pub use envelope::SnapshotBus;
pub use keys::KeyRing;
pub use registry::{ControlError, Registry};
pub use snapshot::{Published, SnapshotBuilder};
