# Gateway version-pin resolver

Working spec, v0.1. Component that stamps every audit event with the exact system / IFU / model / prompt / policy / gateway versions in force when the request ran, within the sub-1ms p99 overhead budget.

---

## 1. The requirement in one line

For any audit event, an auditor must be able to reconstruct: *this decision was produced by model M at version V, using prompt P at version W, under policy Q at version R, on system version S — and here is the byte-identical artifact for each.*

Three properties fall out of that:

- **Atomicity.** A single request resolves against exactly one configuration. A reload mid-request must not split it.
- **Verifiability.** Version identifiers must be derivable from content, not assigned by a database. Otherwise the DB is a trusted third party and the evidence is only as good as its access control.
- **Bounded staleness.** You must be able to state, in a document, the maximum time a gateway node can serve traffic under a superseded configuration.

---

## 2. Data structure

Immutable snapshot, atomically swapped. No locks on the read path.

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

pub type Hash32 = [u8; 32];

pub struct ConfigSnapshot {
    pub generation:    u64,          // monotonic, assigned by control plane
    pub content_hash:  Hash32,       // BLAKE3 over canonical encoding of this snapshot
    pub built_at:      SystemTime,
    pub gateway_version: Arc<str>,

    systems: HashMap<Arc<str>, Arc<SystemConfig>>,
    keys:    HashMap<Hash32, Arc<KeyBinding>>,   // sha256(api_key) -> binding
}

pub struct SystemConfig {
    pub system_id:      Arc<str>,
    pub system_version: Arc<str>,
    pub ifu_version:    Arc<str>,
    pub risk_class:     RiskClass,          // High | Transparency | Minimal | Unclassified
    pub policy_id:      Arc<str>,
    pub policy_version: Arc<str>,           // content hash of the compiled policy
    pub routes:         Vec<Route>,         // ordered, first match wins
    pub default_route:  usize,
}

pub struct Route {
    pub matcher:        Matcher,            // path, model alias, header predicate
    pub model_id:       Arc<str>,
    pub model_version:  Arc<str>,           // provider-reported pinned id, never a floating alias
    pub prompt_id:      Arc<str>,
    pub prompt_version: Arc<str>,           // BLAKE3 of the rendered template source
    pub prompt_body:    PromptRef,
}

pub enum PromptRef {
    Inline(Arc<str>),      // resident
    Lazy(Hash32),          // fetch on first use, cached; hash is always resident
}
```

The `PromptRef` split matters at multi-tenant scale. Pins only need the *hash*, which is 32 bytes. Prompt bodies can be megabytes across thousands of tenants. Keep hashes eagerly resident so resolution never blocks; load bodies lazily into a bounded LRU. A cold prompt body costs one fetch on first request, not on every request, and it never affects the pin.

`Arc<str>` everywhere rather than `String`: cloning into the request context is a refcount increment, not an allocation.

---

## 3. Swap mechanism

```rust
use arc_swap::ArcSwap;

pub struct PinResolver {
    current: ArcSwap<ConfigSnapshot>,
    fail_closed_on_stale: bool,
    staleness_budget: Duration,
}
```

`ArcSwap::load()` on the read path is a single atomic load plus a hazard-pointer style guard — tens of nanoseconds, no contention between cores. Do not use `RwLock<Arc<T>>`: even uncontended, the read lock touches a shared cache line, and under 350+ RPS across cores that shows up in your p99, which is the number you are selling.

Reload is build-off-thread, then swap:

```rust
pub async fn reload(&self, next: ConfigSnapshot) {
    let prev = self.current.load_full();
    let next = Arc::new(next);
    self.current.store(Arc::clone(&next));
    emit_config_generation_applied(&prev, &next);   // audit event, see §6
}
```

The old snapshot stays alive as long as any in-flight request holds a reference. That is the atomicity guarantee: a request that loaded generation 41 keeps reading generation 41 until it completes, even if 42 lands mid-stream.

---

## 4. Resolution

```rust
#[derive(Clone)]
pub struct Pins {
    pub config_generation: u64,
    pub config_hash:       Hash32,
    pub system_id:         Arc<str>,
    pub system_version:    Arc<str>,
    pub ifu_version:       Arc<str>,
    pub model_id:          Arc<str>,
    pub model_version:     Arc<str>,
    pub prompt_id:         Arc<str>,
    pub prompt_version:    Arc<str>,
    pub policy_id:         Arc<str>,
    pub policy_version:    Arc<str>,
    pub gateway_version:   Arc<str>,
    pub risk_class:        RiskClass,
    pub resolved_stale:    bool,
    pub risk_flags:        SmallVec<[RiskFlag; 4]>,
}

impl PinResolver {
    #[inline]
    pub fn resolve(&self, req: &Ingress) -> Result<Pins, ResolveError> {
        let snap = self.current.load();

        let key_hash = req.api_key_hash();                          // precomputed at auth
        let binding = snap.keys.get(&key_hash).ok_or(ResolveError::UnknownKey)?;
        let system  = snap.systems.get(&binding.system_id).ok_or(ResolveError::UnknownSystem)?;

        let route = system.routes.iter()
            .find(|r| r.matcher.matches(req))
            .unwrap_or(&system.routes[system.default_route]);

        let stale = snap.built_at.elapsed().unwrap_or_default() > self.staleness_budget;
        if stale && system.risk_class == RiskClass::High && self.fail_closed_on_stale {
            return Err(ResolveError::StaleConfigFailClosed { generation: snap.generation });
        }

        Ok(Pins {
            config_generation: snap.generation,
            config_hash:       snap.content_hash,
            system_id:         Arc::clone(&system.system_id),
            system_version:    Arc::clone(&system.system_version),
            ifu_version:       Arc::clone(&system.ifu_version),
            model_id:          Arc::clone(&route.model_id),
            model_version:     Arc::clone(&route.model_version),
            prompt_id:         Arc::clone(&route.prompt_id),
            prompt_version:    Arc::clone(&route.prompt_version),
            policy_id:         Arc::clone(&system.policy_id),
            policy_version:    Arc::clone(&system.policy_version),
            gateway_version:   Arc::clone(&snap.gateway_version),
            risk_class:        system.risk_class,
            resolved_stale:    stale,
            risk_flags:        if stale { smallvec![RiskFlag::StaleConfig] } else { smallvec![] },
        })
    }
}
```

Cost: one atomic load, two hash lookups, a short linear scan over routes, ~12 refcount increments. Target **< 5µs p99**, which leaves the rest of your 100µs auth-and-lookup budget for the actual key verification.

Precedence, if you support overrides: request header > virtual key binding > route rule > system default. Any header-driven override is itself a risk flag — a caller pinning their own model version is a governance event, log it as `pin.overridden` with the overridden field names.

---

## 5. Carry the pins, don't re-resolve

Resolve **once**, at request admission, into the request context. Every downstream emitter reads from that struct.

```rust
pub struct RequestCtx {
    pub trace_id: TraceId,
    pub pins:     Pins,      // resolved once, never mutated
    pub started:  Instant,
}
```

Re-resolving before writing the audit event is the bug that eats this whole design. A streaming completion can run for 90 seconds; a reload in that window would make the event report a configuration the request never used.

**Retries and failover are the exception.** If a provider call fails over to a different model, that attempt genuinely ran under a different model pin. Emit one event per attempt, each with its own `model_id` / `model_version`, all sharing `trace_id`, with an `attempt_seq` field. Do not overwrite the original pin. The failover itself gets a `provider.failover` event carrying both the from- and to- model versions.

---

## 6. Propagation and staleness

Control plane writes to Postgres, bumps `generation`, publishes to NATS. Gateway nodes subscribe and reload. Nodes also poll on an interval as a backstop, because a missed message must not mean indefinite staleness.

```
staleness_budget      = 30s   (default; configurable per tenant)
poll_interval         = 10s
nats_reload_latency   = ~50ms typical
```

Three states, and the behaviour must be documented because it goes in the customer's technical file:

| State | Condition | Behaviour |
|---|---|---|
| Fresh | `built_at` within budget | normal |
| Stale | budget exceeded, control plane unreachable | high-risk systems: **fail closed** (503, `StaleConfigFailClosed`). Others: serve with `RiskFlag::StaleConfig` and `resolved_stale = true` on every event |
| Cold start | no snapshot yet | fail closed for all systems. Never serve traffic with `unknown` pins on a high-risk system |

Fail-closed on stale config for high-risk systems is the compliance-correct default and it will get pushback from every engineer who sees it. Make it configurable, default it on for `RiskClass::High`, and log any attempt to turn it off as a governance event. That log line is worth more than the setting.

Every reload emits an audit event on the node's own chain:

```
event_type = "config.generation.applied"
from_generation, to_generation, node_id, applied_at, propagation_ms, changed_fields[]
```

`propagation_ms` is your evidence for the bounded-staleness claim. Chart the p99 of it; that number goes in the sales deck and in the customer's Article 11 documentation.

---

## 7. Version identifiers must be content-derived

| Pin | Derivation |
|---|---|
| `prompt_version` | BLAKE3 of the canonicalised template source (normalised line endings, no trailing whitespace), hex-truncated to 16 chars |
| `policy_version` | BLAKE3 of the compiled policy AST, not the source text — comment changes must not bump it |
| `system_version` | semver assigned by the control plane, plus the snapshot `content_hash` as a tiebreaker |
| `model_version` | the provider's own pinned identifier as returned in the response body. **Never** a floating alias |
| `ifu_version` | content hash of the generated instructions-for-use artifact |
| `gateway_version` | build semver + git SHA |

The model one is where people get burned. If a customer routes to a floating alias, the underlying weights can change without any signal and every pin you recorded is a lie. Resolve the alias against the provider's response metadata and record what actually served. If a provider does not return a pinned identifier, record `unresolved:<alias>` and raise a `RiskFlag::UnpinnedModel` — that is a finding in the readiness report, and it is a real one.

**Never write NULL.** Unknown is the literal string `unknown` plus a risk flag. NULL hides a gap; `unknown` shows up in a `GROUP BY` and turns into a line item on an invoice.

---

## 8. Substantial modification detection

Article 12(2)(a) wants situations that may constitute a substantial modification to be identifiable from the logs. Diff consecutive generations on reload:

```rust
pub fn classify_change(prev: &SystemConfig, next: &SystemConfig) -> ChangeClass {
    if prev.model_id != next.model_id                 { return ChangeClass::Substantial; }
    if major(&prev.system_version) != major(&next.system_version) { return ChangeClass::Substantial; }
    if prev.policy_version != next.policy_version     { return ChangeClass::Material; }
    if prev.prompt_version != next.prompt_version     { return ChangeClass::Material; }
    ChangeClass::Minor
}
```

`Substantial` sets `substantial_modification = 1` on the `config.changed` event and should fire an alert, because under the Act a substantial modification can reset the grandfathering position and pull a system back into scope. This is a legal determination, not an automated one — so the product's job is to *surface the candidate*, never to declare the answer. Word the UI accordingly: "may constitute a substantial modification — review required."

Prompt changes classified as `Material` rather than `Substantial` is a judgement call. Flag it, let the customer's own policy decide, and log the decision.

---

## 9. Benchmarks to hold yourself to

Criterion benches, run in CI, fail the build on regression:

| Bench | Target |
|---|---|
| `resolve` p50 | < 2µs |
| `resolve` p99 | < 5µs |
| `resolve` p99 under concurrent `reload` storm (10/s) | < 8µs |
| Snapshot build, 10k systems / 50k routes | < 500ms |
| Snapshot resident memory, 10k systems, hashes only | < 200MB |
| End-to-end gateway overhead p99 with pins enabled | < 1ms |

The reload-storm bench is the one that catches the `RwLock` mistake. Run it early.

---

## 10. Test cases that must exist

1. Reload lands mid-stream on a 60s streaming response → event reports the *old* generation.
2. Failover from model A to model B → two events, distinct model pins, shared `trace_id`, incrementing `attempt_seq`.
3. Control plane unreachable past budget, high-risk system → 503, no event with `unknown` pins.
4. Control plane unreachable past budget, minimal-risk system → served, every event carries `StaleConfig`.
5. Provider returns a floating alias → `UnpinnedModel` flag, request still served.
6. Two nodes, same generation → byte-identical `config_hash`. If this ever fails, your canonical encoding is non-deterministic and the whole evidence chain is unsound.
7. Cold start before first snapshot → fail closed, no traffic served.
8. Header override present → `pin.overridden` event listing the overridden fields.

Test 6 is the one worth writing first. Non-deterministic canonical encoding is the failure mode that stays invisible until an auditor tries to verify a chain and can't.
