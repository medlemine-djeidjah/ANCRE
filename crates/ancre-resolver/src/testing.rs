//! Snapshot fixtures, behind the `testing` feature.
//!
//! Shared by this crate's tests and by the benches, so both exercise the same
//! shapes. Deterministic: the same call produces the same `content_hash` on
//! every machine.

use ancre_canon::Hash32;
use ancre_types::{
    ConfigSnapshot, IngressMeta, KeyBindingSpec, Matcher, PinOverrides, RiskClass, RouteSpec,
    SnapshotSpec, SystemConfigSpec,
};

pub const SYSTEM_A: &str = "hr-screening";
pub const SYSTEM_B: &str = "credit-scoring";

#[must_use]
pub fn key_hash(name: &str) -> Hash32 {
    ancre_canon::hash_bytes(name.as_bytes())
}

#[must_use]
pub fn route(alias: &str, model: &str) -> RouteSpec {
    RouteSpec {
        matcher: if alias == "*" {
            Matcher::Any
        } else {
            Matcher::ModelAlias(alias.into())
        },
        model_id: model.into(),
        model_version: format!("{model}-2024-08-06"),
        prompt_id: "screen-cv".into(),
        prompt_version: "b3:9f2c".into(),
        prompt_hash: ancre_canon::hash_bytes(b"prompt"),
    }
}

#[must_use]
pub fn system(id: &str, risk: RiskClass) -> SystemConfigSpec {
    SystemConfigSpec {
        system_id: id.into(),
        system_version: "2.1.0".into(),
        ifu_version: "b3:aa11".into(),
        risk_class: risk,
        policy_id: "none".into(),
        policy_version: "none".into(),
        routes: vec![
            route("fast", "gpt-4o-mini"),
            route("careful", "claude-opus-5"),
            route("*", "gpt-4o"),
        ],
        default_route: 2,
    }
}

/// Two systems: `hr-screening` is High risk, `credit-scoring` is Minimal —
/// so one fixture covers both sides of every fail-closed test.
#[must_use]
pub fn spec(generation: u64) -> SnapshotSpec {
    SnapshotSpec {
        generation,
        gateway_version: "0.1.0+abc123".into(),
        systems: vec![
            system(SYSTEM_A, RiskClass::High),
            system(SYSTEM_B, RiskClass::Minimal),
        ],
        keys: vec![
            KeyBindingSpec {
                key_hash: key_hash("key-high"),
                tenant_id: "acme".into(),
                system_id: SYSTEM_A.into(),
            },
            KeyBindingSpec {
                key_hash: key_hash("key-minimal"),
                tenant_id: "acme".into(),
                system_id: SYSTEM_B.into(),
            },
        ],
    }
}

#[must_use]
pub fn snapshot(generation: u64) -> ConfigSnapshot {
    ConfigSnapshot::build(spec(generation)).expect("fixture must be a valid snapshot")
}

pub const NO_HEADERS: &[(&str, &str)] = &[];

/// A request bound to whichever system the key maps to.
#[must_use]
pub fn request(key: &str) -> IngressMeta<'static> {
    IngressMeta {
        path: "/v1/chat/completions",
        model_alias: None,
        api_key_hash: key_hash(key),
        headers: NO_HEADERS,
        overrides: PinOverrides::default(),
    }
}

/// A snapshot sized for the bench targets: 10k systems, 50k routes
/// (resolver spec §9).
#[must_use]
pub fn large_spec(systems: usize, routes_per_system: usize) -> SnapshotSpec {
    let mut spec = SnapshotSpec {
        generation: 1,
        gateway_version: "0.1.0+abc123".into(),
        systems: Vec::with_capacity(systems),
        keys: Vec::with_capacity(systems),
    };

    for i in 0..systems {
        let id = format!("system-{i:06}");
        let mut s = system(
            &id,
            if i % 3 == 0 {
                RiskClass::High
            } else {
                RiskClass::Minimal
            },
        );
        s.routes = (0..routes_per_system)
            .map(|r| {
                if r == routes_per_system - 1 {
                    route("*", "gpt-4o")
                } else {
                    route(&format!("alias-{r}"), "gpt-4o-mini")
                }
            })
            .collect();
        s.default_route = routes_per_system - 1;
        spec.systems.push(s);

        spec.keys.push(KeyBindingSpec {
            key_hash: key_hash(&format!("key-{i:06}")),
            tenant_id: "acme".into(),
            system_id: id,
        });
    }

    spec
}

/// A request bound to system `i` of a `large_spec` snapshot.
#[must_use]
pub fn request_for_large(i: usize) -> IngressMeta<'static> {
    IngressMeta {
        path: "/v1/chat/completions",
        model_alias: None,
        api_key_hash: key_hash(&format!("key-{i:06}")),
        headers: NO_HEADERS,
        overrides: PinOverrides::default(),
    }
}
