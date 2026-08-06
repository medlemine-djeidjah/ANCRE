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
    /// Content hash over the canonical encoding of this snapshot. Two nodes on
    /// the same generation must produce identical bytes here — resolver spec
    /// test 6, and the first test to write.
    pub content_hash: Hash32,
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

    /// Build from the control plane's serialisable form and derive
    /// `content_hash`. The only constructor — a snapshot without a hash is not
    /// a snapshot.
    ///
    /// Bench target: 10k systems / 50k routes in < 500ms (resolver spec §9).
    pub fn build(_spec: SnapshotSpec) -> Result<Self, SnapshotError> {
        todo!("M2: materialise maps, canonical-encode, derive content_hash")
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
    pub policy_version: Arc<str>,
    /// Ordered, first match wins.
    pub routes: Vec<Route>,
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
    pub fn matches(&self, _req: &IngressMeta<'_>) -> bool {
        todo!("M2: match on self, no allocation")
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
}

#[derive(Debug)]
pub struct KeyBinding {
    pub tenant_id: Arc<str>,
    pub system_id: Arc<str>,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("system {0} has default_route index out of bounds")]
    BadDefaultRoute(String),
    #[error("duplicate system id: {0}")]
    DuplicateSystem(String),
    #[error("canonical encoding failed")]
    Canon,
}
