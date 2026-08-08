//! The Postgres `Registry`, `CheckpointStore` and `KeyDirectory`.
//!
//! One type behind three traits, because they share one pool and one
//! connection budget; splitting them would only make `main` hold three handles
//! to the same database.
//!
//! Nothing in here is hashed into a chain, so unlike the ClickHouse mapping
//! this file is not hash-critical — with two exceptions that are:
//!
//! - **A stored checkpoint must round-trip exactly.** `built_at` is inside the
//!   signed body, so a timestamp that loses a microsecond in Postgres comes
//!   back as a checkpoint whose signature no longer verifies. `timestamptz` is
//!   microsecond-resolution and `Timestamp` is microseconds, so the mapping is
//!   lossless — and `a_checkpoint_survives_a_round_trip_through_postgres`
//!   holds it down against a real server rather than against that claim.
//! - **Every read is ordered.** Two replicas holding identical rows must build
//!   byte-identical snapshots. Postgres gives no order without `ORDER BY`, and
//!   the failure mode of forgetting one is a `content_hash` that differs
//!   between nodes — intermittently, under load.

use ancre_canon::Hash32;
use ancre_chain::{ChainId, Checkpoint, CheckpointBody, SignatureBytes};
use ancre_types::{KeyBindingSpec, Matcher, RiskClass, RouteSpec, SystemConfigSpec, Timestamp};
use sqlx::Row as _;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::api::KeyDirectory;
use crate::checkpointer::{CheckpointStore, PublicKeyRecord};
use crate::registry::{ControlError, Registry};

/// Lock key for generation allocation. An arbitrary constant — advisory locks
/// share one namespace, so what matters is only that nothing else in this
/// database picks the same number.
const GENERATION_LOCK: i64 = 0x616e_6372_655f_6731;

#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connect, with a pool sized for a control plane rather than a gateway:
    /// this is off the request path, and every query here is either a poll
    /// loop or an operator's read.
    pub async fn connect(url: &str) -> Result<Self, ControlError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(db)?;
        Ok(Self { pool })
    }

    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Install `active` as the signing key of record, closing whatever window
    /// was open before it.
    ///
    /// Called once at startup, and it is where rotation actually happens: a
    /// control plane restarted with a different key file retires the old key
    /// **at the same instant** the new one takes over, because a gap between
    /// the two windows leaves any checkpoint sealed inside it unattributable
    /// to a key an auditor was given.
    ///
    /// Refuses to take a key back out of retirement. A key with a closed
    /// window has already been published as no-longer-valid, and re-opening it
    /// would make the export claim two windows for one key — an auditor
    /// checking a signature by timestamp could not tell which applied.
    pub async fn install_active_key(
        &self,
        key_id: &str,
        public_key: &[u8; 32],
        now: Timestamp,
    ) -> Result<(), ControlError> {
        let at = to_pg(now)?;
        let mut tx = self.pool.begin().await.map_err(db)?;

        let existing: Option<(Vec<u8>, Option<time::OffsetDateTime>)> =
            sqlx::query_as("SELECT public_key, valid_to FROM signing_keys WHERE key_id = $1")
                .bind(key_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;

        if let Some((stored, valid_to)) = &existing {
            if stored.as_slice() != public_key.as_slice() {
                return Err(ControlError::Invalid(format!(
                    "key_id {key_id} is already registered with a different public key; \
                     a key id names a key and may not be reassigned"
                )));
            }
            if valid_to.is_some() {
                return Err(ControlError::Invalid(format!(
                    "key_id {key_id} was retired and cannot be made active again"
                )));
            }
            // Already the active key: a plain restart.
            tx.commit().await.map_err(db)?;
            return Ok(());
        }

        // Close the outgoing window at exactly the instant the new one opens.
        sqlx::query("UPDATE signing_keys SET valid_to = $1 WHERE valid_to IS NULL")
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(db)?;

        sqlx::query(
            "INSERT INTO signing_keys (key_id, public_key, valid_from, valid_to) \
             VALUES ($1, $2, $3, NULL)",
        )
        .bind(key_id)
        .bind(public_key.as_slice())
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        tx.commit().await.map_err(db)
    }
}

impl Registry for PgStore {
    /// Two queries, joined in memory rather than in SQL.
    ///
    /// A join would return one row per route and the systems would have to be
    /// reassembled from it anyway; two ordered reads are simpler and the
    /// ordering guarantee is easier to see. Route order comes from `position`
    /// and is **not** sorted afterwards — routes are first-match-wins, so their
    /// order is semantic.
    async fn systems(&self) -> Result<Vec<SystemConfigSpec>, ControlError> {
        let system_rows = sqlx::query(
            "SELECT system_id, system_version, ifu_version, risk_class, \
                    policy_id, policy_version, default_route \
               FROM systems WHERE NOT archived ORDER BY system_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let route_rows = sqlx::query(
            "SELECT r.system_id, r.matcher, r.model_id, r.model_version, \
                    r.prompt_id, r.prompt_version, r.prompt_hash \
               FROM routes r JOIN systems s USING (system_id) \
              WHERE NOT s.archived \
              ORDER BY r.system_id, r.position",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut routes: std::collections::HashMap<String, Vec<RouteSpec>> =
            std::collections::HashMap::new();
        for r in route_rows {
            let system_id: String = r.try_get("system_id").map_err(db)?;
            let matcher: serde_json::Value = r.try_get("matcher").map_err(db)?;
            // Refused rather than defaulted. A `Matcher` shape this build does
            // not understand, silently read as `Any`, is a route that matches
            // every request — traffic repointed without a pin anyone can see.
            let matcher: Matcher = serde_json::from_value(matcher).map_err(|e| {
                ControlError::Invalid(format!("system {system_id}: unreadable matcher: {e}"))
            })?;

            routes.entry(system_id).or_default().push(RouteSpec {
                matcher,
                model_id: r.try_get("model_id").map_err(db)?,
                model_version: r.try_get("model_version").map_err(db)?,
                prompt_id: r.try_get("prompt_id").map_err(db)?,
                prompt_version: r.try_get("prompt_version").map_err(db)?,
                prompt_hash: hash32(
                    &r.try_get::<Vec<u8>, _>("prompt_hash").map_err(db)?,
                    "prompt_hash",
                )?,
            });
        }

        let mut out = Vec::with_capacity(system_rows.len());
        for s in system_rows {
            let system_id: String = s.try_get("system_id").map_err(db)?;
            let risk_class: String = s.try_get("risk_class").map_err(db)?;
            let risk_class = RiskClass::from_wire(&risk_class).ok_or_else(|| {
                ControlError::Invalid(format!(
                    "system {system_id}: unknown risk_class {risk_class:?}"
                ))
            })?;
            let default_route: i32 = s.try_get("default_route").map_err(db)?;

            out.push(SystemConfigSpec {
                routes: routes.remove(&system_id).unwrap_or_default(),
                system_version: s.try_get("system_version").map_err(db)?,
                ifu_version: s.try_get("ifu_version").map_err(db)?,
                risk_class,
                policy_id: s.try_get("policy_id").map_err(db)?,
                policy_version: s.try_get("policy_version").map_err(db)?,
                default_route: usize::try_from(default_route).unwrap_or(usize::MAX),
                system_id,
            });
        }
        Ok(out)
    }

    async fn keys(&self) -> Result<Vec<KeyBindingSpec>, ControlError> {
        let rows = sqlx::query(
            "SELECT key_hash, tenant_id, system_id FROM api_keys \
              WHERE revoked_at IS NULL ORDER BY key_hash",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        rows.into_iter()
            .map(|r| {
                Ok(KeyBindingSpec {
                    key_hash: hash32(
                        &r.try_get::<Vec<u8>, _>("key_hash").map_err(db)?,
                        "key_hash",
                    )?,
                    tenant_id: r.try_get("tenant_id").map_err(db)?,
                    system_id: r.try_get("system_id").map_err(db)?,
                })
            })
            .collect()
    }

    async fn prompt(&self, hash: Hash32) -> Result<Option<Vec<u8>>, ControlError> {
        let body: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT body FROM prompts WHERE prompt_hash = $1")
                .bind(hash.as_bytes().as_slice())
                .fetch_optional(&self.pool)
                .await
                .map_err(db)?;

        // Verified against its own key on the way out. A body that no longer
        // hashes to the `prompt_version` an event pinned is not the prompt
        // that event used, and serving it into an evidence pack would put a
        // plausible wrong answer in front of an auditor.
        if let Some(body) = &body
            && ancre_canon::hash_bytes(body) != hash
        {
            return Err(ControlError::Invalid(format!(
                "prompt {} does not hash to its own key; the row was edited in place",
                hash.to_hex()
            )));
        }
        Ok(body)
    }

    async fn published(&self) -> Result<Option<(u64, Hash32)>, ControlError> {
        let row: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT generation, content_hash FROM snapshot_generations \
              ORDER BY generation DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;

        row.map(|(g, h)| Ok((u64::try_from(g).unwrap_or(0), hash32(&h, "content_hash")?)))
            .transpose()
    }

    /// Allocate under a transaction-scoped advisory lock.
    ///
    /// Not `SELECT ... FOR UPDATE` on the highest row, which is the obvious
    /// shape and is wrong in exactly one case: an empty table has no row to
    /// lock, so two replicas publishing the first-ever generation both read
    /// "0", both write "1", and one of them takes a primary-key violation for
    /// a snapshot it has already built. An advisory lock exists whether or not
    /// there are rows.
    ///
    /// The lock covers allocation only, not the bus send that follows — see
    /// D14 in `docs/deferred.md` for what that leaves open with more than one
    /// replica.
    async fn allocate_generation(&self, content_hash: Hash32) -> Result<u64, ControlError> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(GENERATION_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(db)?;

        let next: i64 =
            sqlx::query_scalar("SELECT coalesce(max(generation), 0) + 1 FROM snapshot_generations")
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;

        sqlx::query("INSERT INTO snapshot_generations (generation, content_hash) VALUES ($1, $2)")
            .bind(next)
            .bind(content_hash.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(db)?;

        tx.commit().await.map_err(db)?;
        Ok(u64::try_from(next).unwrap_or(0))
    }
}

impl CheckpointStore for PgStore {
    async fn last_sealed(&self, chain: &ChainId) -> Result<Option<(u64, Timestamp)>, ControlError> {
        let row: Option<(i64, time::OffsetDateTime)> = sqlx::query_as(
            "SELECT seq_to, built_at FROM checkpoints \
              WHERE tenant_id = $1 AND system_id = $2 \
              ORDER BY seq_to DESC LIMIT 1",
        )
        .bind(&chain.tenant_id)
        .bind(&chain.system_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;

        Ok(row.map(|(seq, at)| (u64::try_from(seq).unwrap_or(0), from_pg(at))))
    }

    /// `ON CONFLICT DO NOTHING`, and the conflict is not an error.
    ///
    /// Two replicas ticking at once can both seal the same range. The two
    /// checkpoints differ only in `built_at` and therefore in signature, and
    /// neither is more true than the other — both attest the same root over
    /// the same events. Keeping the first is idempotent; failing the tick
    /// would stall checkpointing on a race that cost nothing.
    async fn put(&self, checkpoint: Checkpoint) -> Result<(), ControlError> {
        let b = &checkpoint.body;
        sqlx::query(
            "INSERT INTO checkpoints \
               (tenant_id, system_id, seq_from, seq_to, root_hash, built_at, \
                canon_version, key_id, signature) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (tenant_id, system_id, seq_to) DO NOTHING",
        )
        .bind(&b.tenant_id)
        .bind(&b.system_id)
        .bind(i64::try_from(b.seq_from).unwrap_or(i64::MAX))
        .bind(i64::try_from(b.seq_to).unwrap_or(i64::MAX))
        .bind(b.root_hash.as_bytes().as_slice())
        .bind(to_pg(b.built_at)?)
        .bind(&b.canon_version)
        .bind(&checkpoint.key_id)
        .bind(checkpoint.signature.0.as_slice())
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(db)
    }

    async fn list(&self, chain: &ChainId) -> Result<Vec<Checkpoint>, ControlError> {
        let rows = sqlx::query(
            "SELECT seq_from, seq_to, root_hash, built_at, canon_version, key_id, signature \
               FROM checkpoints WHERE tenant_id = $1 AND system_id = $2 ORDER BY seq_to",
        )
        .bind(&chain.tenant_id)
        .bind(&chain.system_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        rows.into_iter()
            .map(|r| {
                let signature: Vec<u8> = r.try_get("signature").map_err(db)?;
                let signature: [u8; 64] = signature.as_slice().try_into().map_err(|_| {
                    ControlError::Db(format!(
                        "checkpoint signature is {} bytes, not 64",
                        signature.len()
                    ))
                })?;
                let seq_from: i64 = r.try_get("seq_from").map_err(db)?;
                let seq_to: i64 = r.try_get("seq_to").map_err(db)?;
                let built_at: time::OffsetDateTime = r.try_get("built_at").map_err(db)?;

                Ok(Checkpoint {
                    body: CheckpointBody {
                        tenant_id: chain.tenant_id.clone(),
                        system_id: chain.system_id.clone(),
                        seq_from: u64::try_from(seq_from).unwrap_or(0),
                        seq_to: u64::try_from(seq_to).unwrap_or(0),
                        root_hash: hash32(
                            &r.try_get::<Vec<u8>, _>("root_hash").map_err(db)?,
                            "root_hash",
                        )?,
                        built_at: from_pg(built_at),
                        canon_version: r.try_get("canon_version").map_err(db)?,
                    },
                    signature: SignatureBytes(signature),
                    key_id: r.try_get("key_id").map_err(db)?,
                })
            })
            .collect()
    }
}

impl KeyDirectory for PgStore {
    /// Read from the table rather than from the in-process `KeyRing`, so a
    /// rotation performed by another replica is exported by this one too.
    async fn public_keys(&self) -> Result<Vec<PublicKeyRecord>, ControlError> {
        let rows = sqlx::query(
            "SELECT key_id, public_key, valid_from, valid_to FROM signing_keys \
              ORDER BY valid_from, key_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        rows.into_iter()
            .map(|r| {
                let valid_to: Option<time::OffsetDateTime> = r.try_get("valid_to").map_err(db)?;
                Ok(PublicKeyRecord {
                    key_id: r.try_get("key_id").map_err(db)?,
                    // Hex, because this is copied by hand out of an evidence
                    // pack.
                    public_key: hex::encode(r.try_get::<Vec<u8>, _>("public_key").map_err(db)?),
                    valid_from: from_pg(r.try_get("valid_from").map_err(db)?),
                    valid_to: valid_to.map(from_pg),
                })
            })
            .collect()
    }
}

fn db<E: std::fmt::Display>(e: E) -> ControlError {
    ControlError::Db(e.to_string())
}

fn hash32(bytes: &[u8], field: &str) -> Result<Hash32, ControlError> {
    <[u8; 32]>::try_from(bytes)
        .map(Hash32::from_bytes)
        .map_err(|_| ControlError::Db(format!("{field} is {} bytes, not 32", bytes.len())))
}

/// Microseconds → `timestamptz`. Lossless in both directions: Postgres stores
/// microseconds since 2000-01-01 and `Timestamp` holds microseconds since the
/// epoch, so the only failure is a value outside the representable range —
/// which is refused rather than clamped, because a clamped `built_at` inside a
/// signed checkpoint body is a signature over a time that never happened.
fn to_pg(ts: Timestamp) -> Result<time::OffsetDateTime, ControlError> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ts.as_micros()) * 1_000)
        .map_err(|e| ControlError::Db(format!("timestamp {} is out of range: {e}", ts.as_micros())))
}

fn from_pg(dt: time::OffsetDateTime) -> Timestamp {
    Timestamp::from_micros(i64::try_from(dt.unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversion the signed body depends on. A microsecond lost here is a
    /// checkpoint that verifies before it is stored and fails afterwards.
    #[test]
    fn a_timestamp_survives_the_postgres_mapping_to_the_microsecond() {
        for micros in [
            0,
            1,
            1_754_400_000_123_456,
            -1,
            -1_000_000_000_000,
            4_102_444_800_999_999,
        ] {
            let ts = Timestamp::from_micros(micros);
            assert_eq!(from_pg(to_pg(ts).unwrap()), ts, "{micros}");
        }
    }

    #[test]
    fn a_wrong_length_hash_is_refused_rather_than_padded() {
        assert!(hash32(&[0u8; 32], "x").is_ok());
        for len in [0usize, 16, 31, 33, 64] {
            let err = hash32(&vec![0u8; len], "root_hash").unwrap_err();
            assert!(err.to_string().contains("not 32"), "{err}");
        }
    }
}
