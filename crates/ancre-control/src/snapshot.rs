//! Snapshot build and publication.
//!
//! Postgres → `SnapshotSpec` → canonical encoding → `content_hash` →
//! generation bump → NATS publish. Gateway nodes also poll every 10s as a
//! backstop, because a missed message must never mean indefinite staleness.

use ancre_types::SnapshotSpec;

#[derive(Debug)]
pub struct SnapshotBuilder {
    _pool: (),
}

impl SnapshotBuilder {
    /// Build the wire form from Postgres.
    ///
    /// Every collection must be **explicitly ordered by a stable key** in SQL.
    /// Postgres does not guarantee row order without `ORDER BY`, and an
    /// unordered collection here produces a different `content_hash` on each
    /// build — which fails resolver spec test 6 intermittently, on one node,
    /// under load. Order everything.
    pub async fn build(&self) -> Result<SnapshotSpec, ControlError> {
        todo!("M4: ORDER BY on every query")
    }

    /// Bump `generation` and publish. The generation counter lives in Postgres
    /// and is allocated transactionally with the snapshot content, so two
    /// control plane replicas cannot mint the same generation with different
    /// contents.
    pub async fn publish(&self, _spec: SnapshotSpec) -> Result<u64, ControlError> {
        todo!("M4")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("postgres: {0}")]
    Db(String),
    #[error("nats: {0}")]
    Bus(String),
    #[error("canonical encoding: {0}")]
    Canon(String),
}
