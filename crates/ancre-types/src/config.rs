//! Immutable configuration snapshot, atomically swapped on the read path.
//!
//! Mirrors `version-pin-resolver-spec.md` §2. The snapshot is built off-thread
//! by the control plane and never mutated in place — a request that loaded
//! generation 41 keeps reading 41 until it completes, even if 42 lands
//! mid-stream. That is the atomicity guarantee.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use ancre_canon::Hash32;
use serde::{Deserialize, Serialize};

use crate::risk::RiskClass;

#[derive(Debug)]
pub struct ConfigSnapshot {
    /// Monotonic, assigned by the control plane.
    pub generation: u64,
    /// Content hash over the canonical encoding of this snapshot's *contents*.
    ///
    /// Two nodes on the same generation must produce identical bytes here —
    /// resolver spec test 6, and the first test written.
    ///
    /// `generation` is deliberately **excluded** from the hash. The spec says
    /// "BLAKE3 over canonical encoding of this snapshot", which would include
    /// it; excluding it makes `config_hash` a true content identifier, so a
    /// generation bump that changed nothing is visible as "same hash, new
    /// generation". That is exactly the question substantial-modification
    /// review asks. The sequence is still recorded — as `config_generation`,
    /// its own column.
    pub content_hash: Hash32,
    /// The control plane's clock, at build time. Reporting only.
    ///
    /// **Not** what staleness is measured against — see `ancre-resolver`,
    /// which uses the local monotonic clock, so cross-machine skew cannot
    /// influence a fail-closed decision.
    pub built_at: SystemTime,
    pub gateway_version: Arc<str>,

    pub(crate) systems: HashMap<Arc<str>, Arc<SystemConfig>>,
    /// `sha256(api_key)` → binding. Hashed at auth, before resolution.
    pub(crate) keys: HashMap<Hash32, Arc<KeyBinding>>,
}

impl ConfigSnapshot {
    #[must_use]
    pub fn system(&self, id: &str) -> Option<&Arc<SystemConfig>> {
        self.systems.get(id)
    }

    #[must_use]
    pub fn key_binding(&self, key_hash: &Hash32) -> Option<&Arc<KeyBinding>> {
        self.keys.get(key_hash)
    }

    #[must_use]
    pub fn system_count(&self) -> usize {
        self.systems.len()
    }

    /// Every system, in no particular order. For the reload diff.
    pub fn systems(&self) -> impl Iterator<Item = (&Arc<str>, &Arc<SystemConfig>)> {
        self.systems.iter()
    }

    /// Every key binding, in no particular order.
    ///
    /// The tenant a system belongs to is only knowable through its keys —
    /// `SystemConfig` carries no tenant, because the hot path never needs one
    /// (the binding is found first, by key hash). Anything that has to name a
    /// *chain* rather than a system does need it, since a chain is
    /// `(tenant_id, system_id)`.
    pub fn bindings(&self) -> impl Iterator<Item = &Arc<KeyBinding>> {
        self.keys.values()
    }

    /// Build from the control plane's serialisable form and derive
    /// `content_hash`. The only constructor — a snapshot without a hash is not
    /// a snapshot.
    ///
    /// Validation happens here rather than at resolve time, so the hot path
    /// can index `routes[default_route]` without a bounds check and without an
    /// `Option`. A malformed snapshot must never reach the request path.
    ///
    /// Bench target: 10k systems / 50k routes in < 500ms (resolver spec §9).
    pub fn build(spec: SnapshotSpec) -> Result<Self, SnapshotError> {
        let content_hash = ancre_canon::content_hash(&spec.content_view())
            .map_err(|e| SnapshotError::Canon(e.to_string()))?;

        let mut systems = HashMap::with_capacity(spec.systems.len());
        for s in spec.systems {
            if s.routes.is_empty() {
                return Err(SnapshotError::NoRoutes(s.system_id));
            }
            if s.default_route >= s.routes.len() {
                return Err(SnapshotError::BadDefaultRoute(s.system_id));
            }

            let system_id: Arc<str> = Arc::from(s.system_id.as_str());
            let routes = s
                .routes
                .into_iter()
                .map(|r| Route {
                    matcher: r.matcher,
                    model_id: Arc::from(r.model_id.as_str()),
                    model_version: Arc::from(r.model_version.as_str()),
                    prompt_id: Arc::from(r.prompt_id.as_str()),
                    prompt_version: Arc::from(r.prompt_version.as_str()),
                    prompt_body: PromptRef::Lazy(r.prompt_hash),
                })
                .collect();

            let config = Arc::new(SystemConfig {
                system_id: Arc::clone(&system_id),
                system_version: Arc::from(s.system_version.as_str()),
                ifu_version: Arc::from(s.ifu_version.as_str()),
                risk_class: s.risk_class,
                policy_id: Arc::from(s.policy_id.as_str()),
                policy_version: Arc::from(s.policy_version.as_str()),
                routes,
                default_route: s.default_route,
            });

            if systems.insert(system_id, config).is_some() {
                return Err(SnapshotError::DuplicateSystem(s.system_id));
            }
        }

        let mut keys = HashMap::with_capacity(spec.keys.len());
        for k in spec.keys {
            if !systems.contains_key(k.system_id.as_str()) {
                return Err(SnapshotError::KeyBindsUnknownSystem(k.system_id));
            }
            // A key bound to two systems would make resolution depend on map
            // ordering — non-deterministic pins, which is the one failure this
            // whole component exists to prevent.
            if keys
                .insert(
                    k.key_hash,
                    Arc::new(KeyBinding {
                        tenant_id: Arc::from(k.tenant_id.as_str()),
                        system_id: Arc::from(k.system_id.as_str()),
                    }),
                )
                .is_some()
            {
                return Err(SnapshotError::DuplicateKey(k.key_hash));
            }
        }

        Ok(Self {
            generation: spec.generation,
            content_hash,
            built_at: SystemTime::now(),
            gateway_version: Arc::from(spec.gateway_version.as_str()),
            systems,
            keys,
        })
    }

    /// An empty snapshot. Serves nothing; exists so that cold start is a
    /// snapshot that refuses rather than an `Option` every read has to unwrap.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            generation: 0,
            content_hash: ancre_canon::GENESIS,
            built_at: SystemTime::now(),
            gateway_version: Arc::from(""),
            systems: HashMap::new(),
            keys: HashMap::new(),
        }
    }
}

/// Wire form of a snapshot: what the control plane publishes and what gets
/// canonically encoded. Ordered collections, so the encoding is reproducible
/// from the source data alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSpec {
    pub generation: u64,
    pub gateway_version: String,
    pub systems: Vec<SystemConfigSpec>,
    pub keys: Vec<KeyBindingSpec>,
}

impl SnapshotSpec {
    /// What `content_hash` is taken over: everything but `generation`.
    ///
    /// Written out explicitly, like `HashedBody`, so a field cannot join the
    /// hashed set by being added to the struct.
    #[must_use]
    pub fn content_view(&self) -> SnapshotContent<'_> {
        SnapshotContent {
            gateway_version: &self.gateway_version,
            systems: &self.systems,
            keys: &self.keys,
        }
    }

    /// Sort every collection into a canonical order.
    ///
    /// The control plane must `ORDER BY` in SQL anyway, but a snapshot
    /// assembled from a `HashMap` anywhere in the pipeline would otherwise
    /// hash differently on each build — and that failure is intermittent, on
    /// one node, under load. Cheap insurance; call it before publishing.
    ///
    /// Route order is **not** sorted: routes are first-match-wins, so their
    /// order is semantic, not incidental.
    pub fn canonicalize_order(&mut self) {
        self.systems.sort_by(|a, b| a.system_id.cmp(&b.system_id));
        self.keys.sort_by(|a, b| a.key_hash.cmp(&b.key_hash));
    }
}

/// The hashed view of a snapshot. Borrowed, and hand-written on purpose.
#[derive(Debug, Serialize)]
pub struct SnapshotContent<'a> {
    pub gateway_version: &'a str,
    pub systems: &'a [SystemConfigSpec],
    pub keys: &'a [KeyBindingSpec],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfigSpec {
    pub system_id: String,
    pub system_version: String,
    pub ifu_version: String,
    pub risk_class: RiskClass,
    pub policy_id: String,
    pub policy_version: String,
    pub routes: Vec<RouteSpec>,
    pub default_route: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteSpec {
    pub matcher: Matcher,
    pub model_id: String,
    pub model_version: String,
    pub prompt_id: String,
    pub prompt_version: String,
    pub prompt_hash: Hash32,
}

/// What actually travels from the control plane to a gateway node.
///
/// The envelope carries the spec **and** its content hash, and the receiver
/// recomputes the hash before installing anything. The bus is not part of the
/// trust boundary: anyone who can reach it can replay a message, and a
/// truncated or edited one must be refused rather than installed. Pins are
/// evidence, and evidence assembled from an unverified message is not.
///
/// It lives here rather than in `ancre-control` because both ends need it, and
/// the hot-path crate must not take a dependency on the control plane to read
/// its own configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEnvelope {
    pub generation: u64,
    /// Over the spec's *contents*, `generation` excluded — the same hash the
    /// gateway will carry in every `config_hash` pin.
    pub content_hash: Hash32,
    pub spec: SnapshotSpec,
}

/// Subject the control plane publishes on, and gateways subscribe to.
pub const SNAPSHOT_SUBJECT: &str = "ancre.config.snapshot";

impl SnapshotEnvelope {
    /// Wrap a spec whose content hash has already been computed.
    #[must_use]
    pub fn seal(spec: SnapshotSpec, content_hash: Hash32) -> Self {
        Self {
            generation: spec.generation,
            content_hash,
            spec,
        }
    }

    /// Compute the hash and wrap. For callers that do not already hold one.
    pub fn seal_now(spec: SnapshotSpec) -> Result<Self, SnapshotError> {
        let content_hash = ancre_canon::content_hash(&spec.content_view())
            .map_err(|e| SnapshotError::Canon(e.to_string()))?;
        Ok(Self::seal(spec, content_hash))
    }

    /// Recompute and compare, before anything is installed.
    ///
    /// Checks the generation too. A gateway that installed a spec whose
    /// `generation` field disagreed with the envelope would resolve pins
    /// claiming one generation while the fleet's records say another, and the
    /// disagreement would surface only in an audit.
    pub fn verify(&self) -> Result<(), SnapshotError> {
        if self.spec.generation != self.generation {
            return Err(SnapshotError::EnvelopeMismatch(format!(
                "envelope generation {} does not match the spec's {}",
                self.generation, self.spec.generation
            )));
        }
        let recomputed = ancre_canon::content_hash(&self.spec.content_view())
            .map_err(|e| SnapshotError::Canon(e.to_string()))?;
        if recomputed != self.content_hash {
            return Err(SnapshotError::EnvelopeMismatch(format!(
                "content hash mismatch: envelope says {}, contents hash to {recomputed}",
                self.content_hash
            )));
        }
        Ok(())
    }

    /// Verify, then build. The only way a snapshot from the wire should ever
    /// become a `ConfigSnapshot` — the two steps are joined here so no caller
    /// can perform the second without the first.
    pub fn install(self) -> Result<ConfigSnapshot, SnapshotError> {
        self.verify()?;
        ConfigSnapshot::build(self.spec)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyBindingSpec {
    pub key_hash: Hash32,
    pub tenant_id: String,
    pub system_id: String,
}

#[derive(Debug)]
pub struct SystemConfig {
    pub system_id: Arc<str>,
    pub system_version: Arc<str>,
    pub ifu_version: Arc<str>,
    pub risk_class: RiskClass,
    pub policy_id: Arc<str>,
    /// Content hash of the *compiled* policy, so comment-only edits do not
    /// bump it. `none` in the MVP — no policy engine (mvp-plan §0).
    ///
    /// Note `none` is not `unknown`: it states that no policy engine is
    /// configured, which is a fact, not a gap. It needs no risk flag, and it
    /// is still countable with a `GROUP BY`.
    pub policy_version: Arc<str>,
    /// Ordered, first match wins.
    pub routes: Vec<Route>,
    /// Guaranteed in bounds by `ConfigSnapshot::build`.
    pub default_route: usize,
}

#[derive(Debug)]
pub struct Route {
    pub matcher: Matcher,
    pub model_id: Arc<str>,
    /// The provider's own pinned identifier. **Never** a floating alias — see
    /// `ancre-provider`, which reads this back off the response body and
    /// raises `RiskFlag::UnpinnedModel` when the provider won't say.
    pub model_version: Arc<str>,
    pub prompt_id: Arc<str>,
    /// BLAKE3 of the canonicalised template source.
    pub prompt_version: Arc<str>,
    pub prompt_body: PromptRef,
}

/// Pins need only the 32-byte hash; bodies can be megabytes across thousands
/// of tenants. Keep hashes eagerly resident so resolution never blocks, load
/// bodies into a bounded LRU on first use (resolver spec §2).
#[derive(Debug, Clone)]
pub enum PromptRef {
    Inline(Arc<str>),
    Lazy(Hash32),
}

impl PromptRef {
    /// The hash, whichever variant. Always available without I/O — that is the
    /// property that keeps resolution non-blocking.
    #[must_use]
    pub fn hash(&self) -> Option<Hash32> {
        match self {
            Self::Lazy(h) => Some(*h),
            Self::Inline(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Matcher {
    /// Matches everything. The default route's matcher.
    Any,
    Path(String),
    ModelAlias(String),
    Header {
        name: String,
        value: String,
    },
    All(Vec<Matcher>),
}

impl Matcher {
    /// Hot path: a short linear scan over routes, so this must not allocate.
    #[must_use]
    pub fn matches(&self, req: &IngressMeta<'_>) -> bool {
        match self {
            Self::Any => true,
            Self::Path(p) => req.path == p,
            Self::ModelAlias(m) => req.model_alias == Some(m.as_str()),
            Self::Header { name, value } => req
                .headers
                .iter()
                // Header names are case-insensitive per RFC 9110. Compared
                // with `eq_ignore_ascii_case` rather than by lowercasing,
                // which would allocate on every route on every request.
                .any(|(n, v)| n.eq_ignore_ascii_case(name) && *v == value),
            Self::All(ms) => ms.iter().all(|m| m.matches(req)),
        }
    }
}

/// The minimum a matcher needs from the request. Deliberately not the request
/// itself — the resolver must not be able to touch the body.
#[derive(Debug)]
pub struct IngressMeta<'a> {
    pub path: &'a str,
    pub model_alias: Option<&'a str>,
    pub api_key_hash: Hash32,
    pub headers: &'a [(&'a str, &'a str)],
    /// Parsed at auth, not here. Precedence is request header > virtual key
    /// binding > route rule > system default (resolver spec §4).
    pub overrides: PinOverrides<'a>,
}

/// Caller-supplied pins, already parsed out of the request headers.
///
/// Any override is a governance event: a caller pinning their own model
/// version is exactly what an auditor wants to find, so it sets
/// `RiskFlag::PinOverridden` and the gateway emits `pin.overridden` naming the
/// fields.
#[derive(Debug, Default, Clone, Copy)]
pub struct PinOverrides<'a> {
    pub model_version: Option<&'a str>,
    pub prompt_version: Option<&'a str>,
}

impl PinOverrides<'_> {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model_version.is_none() && self.prompt_version.is_none()
    }
}

#[derive(Debug)]
pub struct KeyBinding {
    pub tenant_id: Arc<str>,
    pub system_id: Arc<str>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("system {0} has a default_route index that is out of bounds")]
    BadDefaultRoute(String),
    #[error("system {0} has no routes")]
    NoRoutes(String),
    #[error("duplicate system id: {0}")]
    DuplicateSystem(String),
    #[error("duplicate api key hash: {0}")]
    DuplicateKey(Hash32),
    #[error("key binds to unknown system: {0}")]
    KeyBindsUnknownSystem(String),
    #[error("canonical encoding failed: {0}")]
    Canon(String),
    /// The envelope does not describe its own contents. Refused before
    /// anything is installed.
    #[error("snapshot envelope rejected: {0}")]
    EnvelopeMismatch(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(alias: &str, model: &str) -> RouteSpec {
        RouteSpec {
            matcher: Matcher::ModelAlias(alias.into()),
            model_id: model.into(),
            model_version: format!("{model}-2024-08-06"),
            prompt_id: "screen-cv".into(),
            prompt_version: "b3:9f2c".into(),
            prompt_hash: ancre_canon::hash_bytes(b"prompt"),
        }
    }

    fn system(id: &str) -> SystemConfigSpec {
        SystemConfigSpec {
            system_id: id.into(),
            system_version: "2.1.0".into(),
            ifu_version: "b3:aa11".into(),
            risk_class: RiskClass::High,
            policy_id: "none".into(),
            policy_version: "none".into(),
            routes: vec![route("fast", "gpt-4o-mini"), route("any", "gpt-4o")],
            default_route: 1,
        }
    }

    fn spec() -> SnapshotSpec {
        SnapshotSpec {
            generation: 41,
            gateway_version: "0.1.0+abc".into(),
            systems: vec![system("hr-screening"), system("credit-scoring")],
            keys: vec![
                KeyBindingSpec {
                    key_hash: ancre_canon::hash_bytes(b"key-a"),
                    tenant_id: "acme".into(),
                    system_id: "hr-screening".into(),
                },
                KeyBindingSpec {
                    key_hash: ancre_canon::hash_bytes(b"key-b"),
                    tenant_id: "acme".into(),
                    system_id: "credit-scoring".into(),
                },
            ],
        }
    }

    /// **Resolver spec §10, test 6.** Two nodes on the same generation must
    /// produce a byte-identical `config_hash`.
    ///
    /// If this ever fails, the canonical encoding is non-deterministic and the
    /// whole evidence chain is unsound.
    #[test]
    fn two_independently_built_snapshots_have_identical_content_hashes() {
        let a = ConfigSnapshot::build(spec()).unwrap();
        let b = ConfigSnapshot::build(spec()).unwrap();
        assert_eq!(a.content_hash, b.content_hash);
    }

    #[test]
    fn content_hash_survives_a_reordered_spec_once_canonicalised() {
        let mut a = spec();
        let mut b = spec();
        b.systems.reverse();
        b.keys.reverse();

        a.canonicalize_order();
        b.canonicalize_order();

        assert_eq!(
            ConfigSnapshot::build(a).unwrap().content_hash,
            ConfigSnapshot::build(b).unwrap().content_hash
        );
    }

    #[test]
    fn generation_is_not_part_of_the_content_hash() {
        let mut later = spec();
        later.generation = 9_999;
        assert_eq!(
            ConfigSnapshot::build(spec()).unwrap().content_hash,
            ConfigSnapshot::build(later).unwrap().content_hash,
            "a generation bump that changed nothing must be visible as such"
        );
    }

    #[test]
    fn any_content_change_changes_the_hash() {
        let mut changed = spec();
        changed.systems[0].routes[0].model_version = "gpt-4o-2099-01-01".into();
        assert_ne!(
            ConfigSnapshot::build(spec()).unwrap().content_hash,
            ConfigSnapshot::build(changed).unwrap().content_hash
        );
    }

    #[test]
    fn route_order_is_semantic_and_changes_the_hash() {
        // First match wins, so swapping two routes is a real config change
        // even though the set is identical.
        let mut swapped = spec();
        swapped.systems[0].routes.swap(0, 1);
        let mut a = spec();
        a.canonicalize_order();
        swapped.canonicalize_order();
        assert_ne!(
            ConfigSnapshot::build(a).unwrap().content_hash,
            ConfigSnapshot::build(swapped).unwrap().content_hash
        );
    }

    #[test]
    fn a_default_route_out_of_bounds_is_refused_at_build_not_at_resolve() {
        let mut bad = spec();
        bad.systems[0].default_route = 7;
        assert_eq!(
            ConfigSnapshot::build(bad).unwrap_err(),
            SnapshotError::BadDefaultRoute("hr-screening".into())
        );
    }

    #[test]
    fn a_system_with_no_routes_is_refused() {
        let mut bad = spec();
        bad.systems[0].routes.clear();
        bad.systems[0].default_route = 0;
        assert_eq!(
            ConfigSnapshot::build(bad).unwrap_err(),
            SnapshotError::NoRoutes("hr-screening".into())
        );
    }

    #[test]
    fn duplicate_system_ids_are_refused() {
        let mut bad = spec();
        bad.systems[1].system_id = "hr-screening".into();
        assert_eq!(
            ConfigSnapshot::build(bad).unwrap_err(),
            SnapshotError::DuplicateSystem("hr-screening".into())
        );
    }

    /// One key bound to two systems would make the resolved pins depend on
    /// hash-map ordering. Non-deterministic pins is the exact failure this
    /// component exists to prevent.
    #[test]
    fn one_key_bound_twice_is_refused() {
        let mut bad = spec();
        bad.keys[1].key_hash = bad.keys[0].key_hash;
        assert!(matches!(
            ConfigSnapshot::build(bad).unwrap_err(),
            SnapshotError::DuplicateKey(_)
        ));
    }

    #[test]
    fn a_key_bound_to_a_missing_system_is_refused() {
        let mut bad = spec();
        bad.keys[0].system_id = "does-not-exist".into();
        assert_eq!(
            ConfigSnapshot::build(bad).unwrap_err(),
            SnapshotError::KeyBindsUnknownSystem("does-not-exist".into())
        );
    }

    #[test]
    fn matchers_do_what_they_say() {
        let headers = [("X-Tenant", "acme"), ("Accept", "text/event-stream")];
        let req = IngressMeta {
            path: "/v1/chat/completions",
            model_alias: Some("fast"),
            api_key_hash: ancre_canon::hash_bytes(b"key-a"),
            headers: &headers,
            overrides: PinOverrides::default(),
        };

        assert!(Matcher::Any.matches(&req));
        assert!(Matcher::Path("/v1/chat/completions".into()).matches(&req));
        assert!(!Matcher::Path("/v1/embeddings".into()).matches(&req));
        assert!(Matcher::ModelAlias("fast".into()).matches(&req));
        assert!(!Matcher::ModelAlias("slow".into()).matches(&req));
        assert!(
            Matcher::All(vec![
                Matcher::Path("/v1/chat/completions".into()),
                Matcher::ModelAlias("fast".into()),
            ])
            .matches(&req)
        );
    }

    #[test]
    fn header_matching_is_case_insensitive_on_the_name_only() {
        let headers = [("X-Tenant", "acme")];
        let req = IngressMeta {
            path: "/",
            model_alias: None,
            api_key_hash: ancre_canon::GENESIS,
            headers: &headers,
            overrides: PinOverrides::default(),
        };

        // RFC 9110: field names are case-insensitive, values are not.
        assert!(
            Matcher::Header {
                name: "x-tenant".into(),
                value: "acme".into()
            }
            .matches(&req)
        );
        assert!(
            !Matcher::Header {
                name: "x-tenant".into(),
                value: "ACME".into()
            }
            .matches(&req)
        );
    }

    #[test]
    fn a_sealed_envelope_verifies_and_installs() {
        let env = SnapshotEnvelope::seal_now(spec()).unwrap();
        assert!(env.verify().is_ok());
        assert_eq!(env.install().unwrap().generation, 41);
    }

    /// The bus is not trusted. An edited message must be refused rather than
    /// installed — the pins it would produce would be evidence of a
    /// configuration nobody approved.
    #[test]
    fn an_edited_spec_is_refused_before_it_can_be_installed() {
        let mut env = SnapshotEnvelope::seal_now(spec()).unwrap();
        env.spec.systems[0].routes[0].model_id = "claude-opus-5".into();

        assert!(matches!(
            env.clone().install(),
            Err(SnapshotError::EnvelopeMismatch(_))
        ));
        assert!(env.verify().is_err());
    }

    #[test]
    fn a_swapped_content_hash_is_refused() {
        let mut env = SnapshotEnvelope::seal_now(spec()).unwrap();
        env.content_hash = ancre_canon::hash_bytes(b"not this");
        assert!(env.verify().is_err());
    }

    /// A generation the spec does not agree with would make every pin carrying
    /// it point at a configuration that never existed.
    #[test]
    fn a_generation_that_disagrees_with_the_spec_is_refused() {
        let mut env = SnapshotEnvelope::seal_now(spec()).unwrap();
        env.generation += 1;
        assert!(env.verify().is_err());
    }

    /// The envelope crosses a process boundary, so the round trip is part of
    /// the contract — a hash that only survives in memory is not a check.
    #[test]
    fn verification_survives_a_json_round_trip() {
        let env = SnapshotEnvelope::seal_now(spec()).unwrap();
        let back: SnapshotEnvelope =
            serde_json::from_slice(&serde_json::to_vec(&env).unwrap()).unwrap();
        assert!(back.verify().is_ok());
    }
}
