//! The Postgres registry and checkpoint store, against a real Postgres.
//!
//! The unit tests prove the conversions are self-consistent. They cannot prove
//! that Postgres *accepts* what this writes, that `timestamptz` really keeps a
//! microsecond, that the advisory lock actually serialises two concurrent
//! allocations, or that the partial unique index refuses a second active key.
//! Those only appear against the real server.
//!
//! Skipped unless `ANCRE_TEST_POSTGRES` is set, so `cargo test` on a machine
//! with no Docker stays green:
//!
//! ```sh
//! docker run -d --name ancre-pg -p 15432:5432 \
//!   -e POSTGRES_USER=ancre -e POSTGRES_PASSWORD=ancre -e POSTGRES_DB=ancre \
//!   -v "$PWD/deploy/compose/init/postgres:/docker-entrypoint-initdb.d:ro" \
//!   postgres:16-alpine
//! ANCRE_TEST_POSTGRES=postgres://ancre:ancre@127.0.0.1:15432/ancre \
//!   cargo test -p ancre-control --test postgres
//! ```

use ancre_chain::{ChainId, CheckpointSigner, verify_checkpoint};
use ancre_control::api::KeyDirectory;
use ancre_control::checkpointer::CheckpointStore;
use ancre_control::postgres::PgStore;
use ancre_control::registry::Registry;
use ancre_types::{Matcher, RiskClass, Timestamp};

const T0: Timestamp = Timestamp::from_micros(1_754_400_000_123_456);

/// `None` when the environment variable is unset, so the suite is a no-op
/// rather than a failure on a machine without Docker.
async fn store() -> Option<PgStore> {
    let url = std::env::var("ANCRE_TEST_POSTGRES").ok()?;
    Some(PgStore::connect(&url).await.expect("connect"))
}

macro_rules! store_or_skip {
    () => {
        match store().await {
            Some(s) => s,
            None => {
                eprintln!("skipped: ANCRE_TEST_POSTGRES is not set");
                return;
            }
        }
    };
}

/// Every test starts from an empty registry. The tables are shared, so this
/// runs before each one rather than relying on test order — which `cargo test`
/// does not give.
async fn reset(store: &PgStore) {
    for table in [
        "api_keys",
        "routes",
        "prompts",
        "systems",
        "checkpoints",
        "signing_keys",
        "snapshot_generations",
    ] {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(store.pool())
            .await
            .expect("reset");
    }
}

/// Insert a system with `n` routes, deliberately **not** in position order, so
/// a missing `ORDER BY` in the read shows up.
async fn seed_system(store: &PgStore, system_id: &str, n: i32) {
    sqlx::query(
        "INSERT INTO systems (system_id, system_version, ifu_version, risk_class, \
                              policy_id, policy_version, default_route) \
         VALUES ($1, 'v1', 'ifu-1', 'high', 'none', 'none', 0)",
    )
    .bind(system_id)
    .execute(store.pool())
    .await
    .expect("insert system");

    for position in (0..n).rev() {
        sqlx::query(
            "INSERT INTO routes (system_id, position, matcher, model_id, model_version, \
                                 prompt_id, prompt_version, prompt_hash) \
             VALUES ($1, $2, $3, $4, 'claude-opus-5-20260101', 'p', 'v1', $5)",
        )
        .bind(system_id)
        .bind(position)
        .bind(serde_json::json!({ "Path": format!("/v{position}") }))
        .bind(format!("model-{position}"))
        .bind(ancre_canon::hash_bytes(b"prompt").as_bytes().as_slice())
        .execute(store.pool())
        .await
        .expect("insert route");
    }
}

async fn seed_key(store: &PgStore, byte: u8, tenant_id: &str, system_id: &str) {
    sqlx::query("INSERT INTO api_keys (key_hash, tenant_id, system_id) VALUES ($1, $2, $3)")
        .bind([byte; 32].as_slice())
        .bind(tenant_id)
        .bind(system_id)
        .execute(store.pool())
        .await
        .expect("insert key");
}

#[tokio::test]
async fn a_system_reads_back_with_its_routes_in_position_order() {
    let store = store_or_skip!();
    reset(&store).await;
    seed_system(&store, "hr-screening", 3).await;

    let systems = store.systems().await.unwrap();
    assert_eq!(systems.len(), 1);
    let s = &systems[0];
    assert_eq!(s.system_id, "hr-screening");
    assert_eq!(s.risk_class, RiskClass::High);
    assert_eq!(s.routes.len(), 3);

    // Inserted in reverse. Routes are first-match-wins, so reading them in the
    // wrong order silently repoints traffic.
    let ids: Vec<_> = s.routes.iter().map(|r| r.model_id.as_str()).collect();
    assert_eq!(ids, ["model-0", "model-1", "model-2"]);
    assert!(matches!(&s.routes[0].matcher, Matcher::Path(p) if p == "/v0"));
}

/// Resolver spec test 6 against the real database: two reads of unchanged rows
/// must produce byte-identical snapshots.
#[tokio::test]
async fn repeated_reads_of_unchanged_rows_produce_the_same_content_hash() {
    let store = store_or_skip!();
    reset(&store).await;
    for i in 0..5 {
        seed_system(&store, &format!("sys-{i}"), 3).await;
        seed_key(&store, i, "acme", &format!("sys-{i}")).await;
    }

    let first = content_hash(&store).await;
    for _ in 0..20 {
        assert_eq!(content_hash(&store).await, first);
    }
}

/// What `SnapshotBuilder::build` does, without the bus.
async fn content_hash(store: &PgStore) -> ancre_canon::Hash32 {
    let mut spec = ancre_types::SnapshotSpec {
        generation: 0,
        gateway_version: "0.1.0+test".into(),
        systems: store.systems().await.unwrap(),
        keys: store.keys().await.unwrap(),
    };
    spec.canonicalize_order();
    ancre_canon::content_hash(&spec.content_view()).unwrap()
}

#[tokio::test]
async fn an_archived_system_and_a_revoked_key_leave_the_snapshot() {
    let store = store_or_skip!();
    reset(&store).await;
    seed_system(&store, "live", 1).await;
    seed_system(&store, "gone", 1).await;
    seed_key(&store, 1, "acme", "live").await;
    seed_key(&store, 2, "acme", "gone").await;

    sqlx::query("UPDATE systems SET archived = true WHERE system_id = 'gone'")
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE key_hash = $1")
        .bind([2u8; 32].as_slice())
        .execute(store.pool())
        .await
        .unwrap();

    let systems = store.systems().await.unwrap();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0].system_id, "live");
    // Its routes must go with it, or the snapshot carries routes for a system
    // that no longer exists.
    assert_eq!(systems[0].routes.len(), 1);

    let keys = store.keys().await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key_hash.as_bytes(), &[1u8; 32]);
}

/// A `Matcher` shape this build cannot read must refuse the whole build. Read
/// as `Any` it would be a route matching every request.
#[tokio::test]
async fn an_unreadable_matcher_refuses_the_build_rather_than_matching_everything() {
    let store = store_or_skip!();
    reset(&store).await;
    seed_system(&store, "hr-screening", 1).await;
    sqlx::query("UPDATE routes SET matcher = $1 WHERE system_id = 'hr-screening'")
        .bind(serde_json::json!({ "RegexFromTheFuture": "^/v2" }))
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.systems().await.unwrap_err();
    assert!(err.to_string().contains("unreadable matcher"), "{err}");
}

#[tokio::test]
async fn generations_are_monotonic_and_carry_their_content() {
    let store = store_or_skip!();
    reset(&store).await;
    assert!(store.published().await.unwrap().is_none());

    let a = ancre_canon::hash_bytes(b"a");
    assert_eq!(store.allocate_generation(a).await.unwrap(), 1);
    assert_eq!(store.published().await.unwrap(), Some((1, a)));

    let b = ancre_canon::hash_bytes(b"b");
    assert_eq!(store.allocate_generation(b).await.unwrap(), 2);
    assert_eq!(store.published().await.unwrap(), Some((2, b)));
}

/// The failure the advisory lock exists for: two replicas allocating the
/// first-ever generation at the same time. `SELECT ... FOR UPDATE` on the
/// highest row locks nothing on an empty table, and both would write 1.
#[tokio::test]
async fn concurrent_allocations_never_hand_out_the_same_generation() {
    let store = store_or_skip!();
    reset(&store).await;

    let mut tasks = Vec::new();
    for i in 0..16u8 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .allocate_generation(ancre_canon::hash_bytes(&[i]))
                .await
        }));
    }

    let mut generations = Vec::new();
    for t in tasks {
        generations.push(t.await.unwrap().expect("allocation must not fail"));
    }
    generations.sort_unstable();
    assert_eq!(
        generations,
        (1..=16).collect::<Vec<u64>>(),
        "every allocation must get its own number"
    );
}

fn signer() -> CheckpointSigner {
    CheckpointSigner::from_bytes([7u8; 32], "cp-test".into())
}

fn chain() -> ChainId {
    ChainId {
        tenant_id: "acme".into(),
        system_id: "hr-screening".into(),
    }
}

/// The test this file exists for.
///
/// `built_at` is inside the signed body. A microsecond lost in Postgres is a
/// checkpoint that verifies before it is stored and fails on the auditor's
/// laptop afterwards.
#[tokio::test]
async fn a_checkpoint_survives_a_round_trip_through_postgres() {
    let store = store_or_skip!();
    reset(&store).await;

    let leaves: Vec<_> = (1..=9u8).map(|i| ancre_canon::hash_bytes(&[i])).collect();
    let signer = signer();
    let original = signer
        .seal_range("acme", "hr-screening", 1, &leaves)
        .unwrap();
    store.put(original.clone()).await.unwrap();

    let back = store.list(&chain()).await.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0], original, "the stored form is not the signed form");
    assert!(
        verify_checkpoint(&back[0], &signer.verifying_key()).is_ok(),
        "a checkpoint that does not verify after storage attests nothing"
    );
}

#[tokio::test]
async fn last_sealed_reports_the_highest_range_and_when_it_was_sealed() {
    let store = store_or_skip!();
    reset(&store).await;
    assert!(store.last_sealed(&chain()).await.unwrap().is_none());

    let signer = signer();
    let leaves: Vec<_> = (1..=4u8).map(|i| ancre_canon::hash_bytes(&[i])).collect();
    let first = signer
        .seal_range("acme", "hr-screening", 1, &leaves)
        .unwrap();
    let second = signer
        .seal_range("acme", "hr-screening", 5, &leaves)
        .unwrap();
    store.put(second.clone()).await.unwrap();
    store.put(first).await.unwrap();

    let (seq_to, at) = store.last_sealed(&chain()).await.unwrap().unwrap();
    assert_eq!(seq_to, 8);
    assert_eq!(at, second.body.built_at);

    // Oldest first, whatever order they were written in — the verifier walks
    // them in range order.
    let listed = store.list(&chain()).await.unwrap();
    assert_eq!(
        listed.iter().map(|c| c.body.seq_from).collect::<Vec<_>>(),
        [1, 5]
    );
}

/// Two replicas can seal the same range at once. The two checkpoints differ
/// only in `built_at`, and neither is more true — keeping the first is
/// idempotent, and failing the tick would stall checkpointing over nothing.
#[tokio::test]
async fn sealing_the_same_range_twice_keeps_the_first_and_does_not_fail() {
    let store = store_or_skip!();
    reset(&store).await;

    let signer = signer();
    let leaves: Vec<_> = (1..=3u8).map(|i| ancre_canon::hash_bytes(&[i])).collect();
    let first = signer
        .seal_range("acme", "hr-screening", 1, &leaves)
        .unwrap();
    let mut second = first.clone();
    second.body.built_at = Timestamp::from_micros(first.body.built_at.as_micros() + 1_000);

    store.put(first.clone()).await.unwrap();
    store.put(second).await.expect("a re-seal is not an error");

    let listed = store.list(&chain()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], first);
}

#[tokio::test]
async fn a_prompt_is_served_by_its_own_hash_and_refused_if_edited() {
    let store = store_or_skip!();
    reset(&store).await;

    let body = b"screen this CV".as_slice();
    let hash = ancre_canon::hash_bytes(body);
    sqlx::query("INSERT INTO prompts (prompt_hash, body) VALUES ($1, $2)")
        .bind(hash.as_bytes().as_slice())
        .bind(body)
        .execute(store.pool())
        .await
        .unwrap();

    assert_eq!(store.prompt(hash).await.unwrap().as_deref(), Some(body));
    assert!(
        store
            .prompt(ancre_canon::hash_bytes(b"nothing"))
            .await
            .unwrap()
            .is_none()
    );

    // Edited in place: the body no longer hashes to the key an event pinned.
    sqlx::query("UPDATE prompts SET body = $1 WHERE prompt_hash = $2")
        .bind(b"screen this CV, but nicer".as_slice())
        .bind(hash.as_bytes().as_slice())
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.prompt(hash).await.unwrap_err();
    assert!(err.to_string().contains("does not hash"), "{err}");
}

#[tokio::test]
async fn installing_a_key_is_idempotent_across_restarts() {
    let store = store_or_skip!();
    reset(&store).await;

    for _ in 0..3 {
        store
            .install_active_key("cp-a", &[1u8; 32], T0)
            .await
            .unwrap();
    }

    let keys = store.public_keys().await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key_id, "cp-a");
    assert_eq!(keys[0].valid_from, T0);
    assert!(keys[0].valid_to.is_none());
}

/// Rotation must leave no gap: a checkpoint sealed in one would be
/// unattributable to any key an auditor was given.
#[tokio::test]
async fn rotating_the_key_closes_the_old_window_where_the_new_one_opens() {
    let store = store_or_skip!();
    reset(&store).await;
    let at = Timestamp::from_micros(T0.as_micros() + 60_000_000);

    store
        .install_active_key("cp-a", &[1u8; 32], T0)
        .await
        .unwrap();
    store
        .install_active_key("cp-b", &[2u8; 32], at)
        .await
        .unwrap();

    let keys = store.public_keys().await.unwrap();
    assert_eq!(keys.len(), 2, "the retired key stays exported");
    assert_eq!(keys[0].valid_to, Some(keys[1].valid_from));
    assert!(keys[1].valid_to.is_none(), "exactly one active key");
    assert_eq!(keys[0].public_key, hex::encode([1u8; 32]));
}

#[tokio::test]
async fn a_retired_key_cannot_be_made_active_again() {
    let store = store_or_skip!();
    reset(&store).await;

    store
        .install_active_key("cp-a", &[1u8; 32], T0)
        .await
        .unwrap();
    store
        .install_active_key("cp-b", &[2u8; 32], T0)
        .await
        .unwrap();

    let err = store
        .install_active_key("cp-a", &[1u8; 32], T0)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("retired"), "{err}");
}

/// A key id names a key. Reassigning one would make a signature verifiable
/// against a public key the export says signed something else.
#[tokio::test]
async fn a_key_id_cannot_be_pointed_at_a_different_key() {
    let store = store_or_skip!();
    reset(&store).await;

    store
        .install_active_key("cp-a", &[1u8; 32], T0)
        .await
        .unwrap();
    let err = store
        .install_active_key("cp-a", &[9u8; 32], T0)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("different public key"), "{err}");
}
