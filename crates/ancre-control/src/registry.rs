//! The registry: where a customer's systems, routes and keys actually live.
//!
//! A trait, for the same reason `EventStore` is one in the ingester — the
//! property that matters here is *determinism*, and determinism is testable
//! without a database. The Postgres implementation is a set of queries; the
//! thing that breaks is the ordering, and a fake that returns rows in a
//! deliberately hostile order catches that in milliseconds.

use ancre_canon::Hash32;
use ancre_types::{KeyBindingSpec, SystemConfigSpec};

/// The source of snapshot content, plus the generation counter.
///
/// **Every collection must come back in a stable order.** Postgres does not
/// guarantee row order without `ORDER BY`, so the SQL behind each of these is
/// required to sort, and `SnapshotBuilder` sorts again on top. Belt and
/// braces, because the failure mode is a `content_hash` that differs between
/// two replicas holding identical data — which is resolver spec test 6 failing
/// intermittently, on one node, under load.
pub trait Registry: Send + Sync {
    /// ```sql
    /// SELECT system_id, system_version, ifu_version, risk_class,
    ///        policy_id, policy_version, default_route
    ///   FROM systems
    ///  WHERE tenant_id = $1 AND NOT archived
    ///  ORDER BY system_id;
    /// -- routes, in their semantic order — first match wins, so `position` is
    /// -- data, not presentation:
    /// SELECT system_id, position, matcher, model_id, model_version,
    ///        prompt_id, prompt_version, prompt_hash
    ///   FROM routes
    ///  WHERE tenant_id = $1
    ///  ORDER BY system_id, position;
    /// ```
    fn systems(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<SystemConfigSpec>, ControlError>> + Send;

    /// ```sql
    /// SELECT key_hash, tenant_id, system_id
    ///   FROM api_keys
    ///  WHERE revoked_at IS NULL
    ///  ORDER BY key_hash;
    /// ```
    fn keys(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<KeyBindingSpec>, ControlError>> + Send;

    /// The last generation this control plane published, and what it contained.
    ///
    /// `None` before the first publish. The content hash comes back with it so
    /// a rebuild that changed nothing can be recognised as a no-op rather than
    /// minting a generation every poll interval forever.
    fn published(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<(u64, Hash32)>, ControlError>> + Send;

    /// Allocate the next generation for `content_hash`, transactionally.
    ///
    /// ```sql
    /// BEGIN;
    /// SELECT generation FROM snapshot_generations
    ///   ORDER BY generation DESC LIMIT 1 FOR UPDATE;
    /// INSERT INTO snapshot_generations (generation, content_hash, built_at)
    ///   VALUES ($next, $2, now());
    /// COMMIT;
    /// ```
    ///
    /// The row lock is the point. Two control-plane replicas that both read
    /// "41" and both write "42" would mint the same generation with different
    /// contents — and then two gateway nodes would report identical
    /// `config_generation` pins for configurations that differ. The pin would
    /// be a number that identifies nothing, which is worse than no pin at all,
    /// because it looks like evidence.
    fn allocate_generation(
        &self,
        content_hash: Hash32,
    ) -> impl std::future::Future<Output = Result<u64, ControlError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("postgres: {0}")]
    Db(String),
    #[error("nats: {0}")]
    Bus(String),
    #[error("canonical encoding: {0}")]
    Canon(String),
    #[error("snapshot rejected: {0}")]
    Invalid(String),
    #[error("chain: {0}")]
    Chain(#[from] ancre_chain::ChainError),
}

#[cfg(feature = "testing")]
pub mod testing {
    //! An in-memory registry, so the build-publish path can be tested — and
    //! benched — without Postgres.

    use std::sync::Mutex;

    use super::{ControlError, Hash32, KeyBindingSpec, Registry, SystemConfigSpec};

    #[derive(Debug)]
    pub struct MemoryRegistry {
        rows: Mutex<Rows>,
        /// Set to make every call fail, for the control-plane-outage tests.
        pub down: std::sync::atomic::AtomicBool,
    }

    #[derive(Debug, Default)]
    struct Rows {
        systems: Vec<SystemConfigSpec>,
        keys: Vec<KeyBindingSpec>,
        generation: u64,
        published: Option<(u64, Hash32)>,
    }

    impl MemoryRegistry {
        #[must_use]
        pub fn new(systems: Vec<SystemConfigSpec>, keys: Vec<KeyBindingSpec>) -> Self {
            Self {
                rows: Mutex::new(Rows {
                    systems,
                    keys,
                    ..Rows::default()
                }),
                down: std::sync::atomic::AtomicBool::new(false),
            }
        }

        pub fn edit(&self, f: impl FnOnce(&mut Vec<SystemConfigSpec>)) {
            f(&mut self.rows.lock().unwrap().systems);
        }

        pub fn kill(&self) {
            self.down.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        pub fn revive(&self) {
            self.down.store(false, std::sync::atomic::Ordering::SeqCst);
        }

        fn check(&self) -> Result<(), ControlError> {
            if self.down.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ControlError::Db("connection refused".into()));
            }
            Ok(())
        }
    }

    impl Registry for MemoryRegistry {
        async fn systems(&self) -> Result<Vec<SystemConfigSpec>, ControlError> {
            self.check()?;
            // Deliberately hostile order: a Vec has one, Postgres without an
            // ORDER BY does not, and the builder must not depend on either.
            let mut systems = self.rows.lock().unwrap().systems.clone();
            systems.reverse();
            Ok(systems)
        }

        async fn keys(&self) -> Result<Vec<KeyBindingSpec>, ControlError> {
            self.check()?;
            let mut keys = self.rows.lock().unwrap().keys.clone();
            keys.reverse();
            Ok(keys)
        }

        async fn published(&self) -> Result<Option<(u64, Hash32)>, ControlError> {
            self.check()?;
            Ok(self.rows.lock().unwrap().published)
        }

        async fn allocate_generation(&self, content_hash: Hash32) -> Result<u64, ControlError> {
            self.check()?;
            let mut rows = self.rows.lock().unwrap();
            rows.generation += 1;
            let g = rows.generation;
            rows.published = Some((g, content_hash));
            Ok(g)
        }
    }
}
