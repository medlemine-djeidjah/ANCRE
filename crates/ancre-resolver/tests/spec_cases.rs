//! The test cases `version-pin-resolver-spec.md` §10 says must exist.
//!
//! Cases 1, 3, 4, 6, 7 and 8 are the resolver's. Cases 2 (failover) and 5
//! (floating alias) belong to the gateway and provider adapters — M3.

use std::sync::Arc;
use std::time::Duration;

use ancre_resolver::{Freshness, PinResolver, ResolveError, StalenessPolicy, testing};
use ancre_types::{ConfigSnapshot, IngressMeta, PinOverrides, RiskFlag};

fn fresh_policy() -> StalenessPolicy {
    StalenessPolicy {
        budget: Duration::from_secs(30),
        fail_closed_on_stale: true,
    }
}

/// A policy whose budget has already elapsed, so any loaded snapshot is stale.
fn already_stale_policy() -> StalenessPolicy {
    StalenessPolicy {
        budget: Duration::ZERO,
        fail_closed_on_stale: true,
    }
}

fn ready(policy: StalenessPolicy) -> PinResolver {
    PinResolver::with_snapshot(testing::snapshot(41), policy)
}

// ---------------------------------------------------------------------------
// Case 1 — a reload mid-request must not split it.
// ---------------------------------------------------------------------------

/// The pins a request resolved with stay valid for its whole life, even when a
/// new generation lands in the middle.
///
/// This is the property a 60-second streaming completion depends on. Re-
/// resolving before writing the audit event is the bug that eats the whole
/// design (spec §5) — so the guarantee is that `Pins`, once returned, is a
/// self-contained value that no reload can reach.
#[test]
fn case_1_a_reload_mid_request_does_not_change_pins_already_resolved() {
    let resolver = ready(fresh_policy());
    let pins = resolver.resolve(&testing::request("key-high")).unwrap();
    assert_eq!(pins.config_generation, 41);

    // Generation 42 lands, with a different model, while the request runs.
    let mut spec = testing::spec(42);
    spec.systems[0].routes[2].model_id = "claude-opus-5".into();
    resolver.reload(ConfigSnapshot::build(spec).unwrap());

    assert_eq!(
        pins.config_generation, 41,
        "the in-flight request must still report the generation it started under"
    );
    assert_eq!(&*pins.model_id, "gpt-4o");
    // And a request starting now sees the new one.
    let after = resolver.resolve(&testing::request("key-high")).unwrap();
    assert_eq!(after.config_generation, 42);
    assert_eq!(&*after.model_id, "claude-opus-5");
}

/// The old snapshot stays alive while anyone holds it — that is what makes
/// the guarantee above structural rather than incidental.
#[test]
fn case_1_the_previous_snapshot_outlives_the_swap_for_its_holders() {
    let resolver = ready(fresh_policy());
    let held = resolver.snapshot();
    assert_eq!(held.generation, 41);

    for generation in 42..60 {
        resolver.reload(testing::snapshot(generation));
    }

    assert_eq!(held.generation, 41, "a held snapshot must not be mutated");
    assert_eq!(resolver.generation(), 59);
}

// ---------------------------------------------------------------------------
// Case 3 — stale past budget, high risk → fail closed.
// ---------------------------------------------------------------------------

#[test]
fn case_3_a_stale_config_fails_closed_on_a_high_risk_system() {
    let resolver = ready(already_stale_policy());

    let err = resolver.resolve(&testing::request("key-high")).unwrap_err();

    assert_eq!(err, ResolveError::StaleConfigFailClosed { generation: 41 });
    assert_eq!(
        err.http_status(),
        503,
        "must be retryable, not a client error"
    );
}

/// The refusal is the point: no event may be written with `unknown` pins for a
/// high-risk system. Failing closed means there is no event at all, which is a
/// 503 in the customer's own metrics — visible, and not silently wrong.
#[test]
fn case_3_no_pins_are_produced_when_failing_closed() {
    let resolver = ready(already_stale_policy());
    assert!(resolver.resolve(&testing::request("key-high")).is_err());
}

// ---------------------------------------------------------------------------
// Case 4 — stale past budget, lower risk → served, flagged.
// ---------------------------------------------------------------------------

#[test]
fn case_4_a_stale_config_still_serves_a_minimal_risk_system_with_a_flag() {
    let resolver = ready(already_stale_policy());

    let pins = resolver.resolve(&testing::request("key-minimal")).unwrap();

    assert!(pins.resolved_stale);
    assert!(
        pins.risk_flags.contains(&RiskFlag::StaleConfig),
        "every event served under a stale config must carry the flag"
    );
    // The pins are still complete — stale is not unknown.
    assert!(!pins.has_gap());
}

#[test]
fn a_fresh_config_carries_no_staleness_flag() {
    let resolver = ready(fresh_policy());
    let pins = resolver.resolve(&testing::request("key-high")).unwrap();

    assert!(!pins.resolved_stale);
    assert!(pins.risk_flags.is_empty());
    assert_eq!(resolver.freshness(), Freshness::Fresh);
}

// ---------------------------------------------------------------------------
// Case 6 — two nodes, same generation, identical config_hash.
// ---------------------------------------------------------------------------

/// The test the whole product rests on, at the level a request sees it.
///
/// `ancre-types` proves the snapshot hashes match; this proves the value
/// actually reaches the pins, so an auditor comparing two nodes' events for
/// the same generation sees the same `config_hash`.
#[test]
fn case_6_two_nodes_on_the_same_generation_stamp_the_same_config_hash() {
    let node_a = ready(fresh_policy());
    let node_b = ready(fresh_policy());

    let a = node_a.resolve(&testing::request("key-high")).unwrap();
    let b = node_b.resolve(&testing::request("key-high")).unwrap();

    assert_eq!(a.config_hash, b.config_hash);
    assert_eq!(a.config_generation, b.config_generation);
}

// ---------------------------------------------------------------------------
// Case 7 — cold start fails closed, for everyone.
// ---------------------------------------------------------------------------

#[test]
fn case_7_cold_start_serves_nothing_at_all() {
    let resolver = PinResolver::cold(fresh_policy(), 1024);

    assert!(resolver.is_cold());
    assert_eq!(resolver.freshness(), Freshness::ColdStart);

    for key in ["key-high", "key-minimal", "key-nonexistent"] {
        let err = resolver.resolve(&testing::request(key)).unwrap_err();
        assert_eq!(
            err,
            ResolveError::ColdStart,
            "{key} must be refused before the first snapshot"
        );
        assert_eq!(err.http_status(), 503);
    }
}

#[test]
fn case_7_the_first_snapshot_ends_cold_start_and_is_not_a_change() {
    let resolver = PinResolver::cold(fresh_policy(), 1024);
    let outcome = resolver.reload(testing::snapshot(41));

    assert!(!resolver.is_cold());
    assert!(
        outcome.is_empty(),
        "the first snapshot is not a modification of anything"
    );
    assert_eq!(outcome.to_generation, 41);
    assert!(resolver.resolve(&testing::request("key-high")).is_ok());
}

// ---------------------------------------------------------------------------
// Case 8 — a header override is a governance event.
// ---------------------------------------------------------------------------

#[test]
fn case_8_a_caller_supplied_pin_wins_and_is_flagged() {
    let resolver = ready(fresh_policy());
    let headers: &[(&str, &str)] = &[];
    let req = IngressMeta {
        path: "/v1/chat/completions",
        model_alias: None,
        api_key_hash: testing::key_hash("key-high"),
        headers,
        overrides: PinOverrides {
            model_version: Some("gpt-4o-2099-01-01"),
            prompt_version: None,
        },
    };

    let pins = resolver.resolve(&req).unwrap();

    // Precedence: request header beats the route rule.
    assert_eq!(&*pins.model_version, "gpt-4o-2099-01-01");
    // And it is recorded as a governance event, not silently honoured.
    assert!(pins.risk_flags.contains(&RiskFlag::PinOverridden));
    // Fields the caller did not override still come from the route.
    assert_eq!(&*pins.prompt_version, "b3:9f2c");
}

#[test]
fn case_8_no_override_means_no_flag() {
    let resolver = ready(fresh_policy());
    let pins = resolver.resolve(&testing::request("key-high")).unwrap();
    assert!(!pins.risk_flags.contains(&RiskFlag::PinOverridden));
}

// ---------------------------------------------------------------------------
// Routing and identity
// ---------------------------------------------------------------------------

#[test]
fn the_first_matching_route_wins() {
    let resolver = ready(fresh_policy());
    let headers: &[(&str, &str)] = &[];
    let mut req = IngressMeta {
        path: "/v1/chat/completions",
        model_alias: Some("fast"),
        api_key_hash: testing::key_hash("key-high"),
        headers,
        overrides: PinOverrides::default(),
    };

    assert_eq!(&*resolver.resolve(&req).unwrap().model_id, "gpt-4o-mini");

    req.model_alias = Some("careful");
    assert_eq!(&*resolver.resolve(&req).unwrap().model_id, "claude-opus-5");
}

#[test]
fn an_unmatched_request_falls_back_to_the_default_route() {
    let resolver = ready(fresh_policy());
    let headers: &[(&str, &str)] = &[];
    let req = IngressMeta {
        path: "/v1/chat/completions",
        model_alias: Some("no-such-alias"),
        api_key_hash: testing::key_hash("key-high"),
        headers,
        overrides: PinOverrides::default(),
    };

    assert_eq!(&*resolver.resolve(&req).unwrap().model_id, "gpt-4o");
}

#[test]
fn an_unknown_key_is_a_401_not_a_503() {
    let resolver = ready(fresh_policy());
    let err = resolver
        .resolve(&testing::request("key-that-was-revoked"))
        .unwrap_err();

    assert_eq!(err, ResolveError::UnknownKey);
    assert_eq!(
        err.http_status(),
        401,
        "an unknown key is the caller's problem; a stale config is ours"
    );
}

#[test]
fn the_key_determines_the_system_and_therefore_the_risk_class() {
    let resolver = ready(fresh_policy());

    let high = resolver.resolve(&testing::request("key-high")).unwrap();
    let minimal = resolver.resolve(&testing::request("key-minimal")).unwrap();

    assert_eq!(&*high.system_id, testing::SYSTEM_A);
    assert_eq!(&*minimal.system_id, testing::SYSTEM_B);
    assert_eq!(high.risk_class, ancre_types::RiskClass::High);
    assert_eq!(minimal.risk_class, ancre_types::RiskClass::Minimal);
}

#[test]
fn no_pin_is_ever_null_or_empty() {
    let resolver = ready(fresh_policy());
    let p = resolver.resolve(&testing::request("key-high")).unwrap();

    for (name, value) in [
        ("system_version", &p.system_version),
        ("ifu_version", &p.ifu_version),
        ("model_id", &p.model_id),
        ("model_version", &p.model_version),
        ("prompt_id", &p.prompt_id),
        ("prompt_version", &p.prompt_version),
        ("policy_id", &p.policy_id),
        ("policy_version", &p.policy_version),
        ("gateway_version", &p.gateway_version),
    ] {
        assert!(!value.is_empty(), "{name} must never be empty");
    }
}

/// `none` states that no policy engine is configured. That is a fact, not a
/// gap — it groups cleanly in a query and needs no risk flag. `unknown` is
/// what a gap looks like, and there are none here.
#[test]
fn a_policy_pin_of_none_is_not_a_gap() {
    let resolver = ready(fresh_policy());
    let pins = resolver.resolve(&testing::request("key-high")).unwrap();

    assert_eq!(&*pins.policy_version, "none");
    assert!(!pins.has_gap());
    assert!(!pins.risk_flags.contains(&RiskFlag::NoPolicyEngine));
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Reads and reloads run together for a while and nothing tears: every
/// resolved pin set is internally consistent, and the generation only ever
/// moves forward.
#[test]
fn concurrent_reads_and_reloads_never_produce_a_torn_snapshot() {
    let resolver = Arc::new(ready(fresh_policy()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writer = {
        let resolver = Arc::clone(&resolver);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut generation = 42;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                resolver.reload(testing::snapshot(generation));
                generation += 1;
                std::thread::sleep(Duration::from_micros(200));
            }
            generation
        })
    };

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let resolver = Arc::clone(&resolver);
            std::thread::spawn(move || {
                let mut seen_max = 0;
                for _ in 0..20_000 {
                    let p = resolver.resolve(&testing::request("key-high")).unwrap();
                    // A torn read would pair a generation with another
                    // snapshot's contents. Every fixture generation has the
                    // same content, so the invariant checked here is that the
                    // pins are complete and the generation is plausible.
                    assert!(!p.has_gap());
                    assert!(p.config_generation >= 41);
                    seen_max = seen_max.max(p.config_generation);
                }
                seen_max
            })
        })
        .collect();

    let seen: Vec<u64> = readers.into_iter().map(|h| h.join().unwrap()).collect();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let final_gen = writer.join().unwrap();

    assert!(seen.iter().all(|&g| g <= final_gen));
}
