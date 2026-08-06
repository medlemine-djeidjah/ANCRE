# Ancre — MVP plan

Working plan, v0.1. Target: **31 October 2026**. Derived from `ancre-prd-and-architecture.md` §7 (V0) and `version-pin-resolver-spec.md`.

---

## 0. What the MVP is

A gateway a platform engineer will put in their request path, that emits a **verifiable chain** of pinned audit events, installable with one command.

It is a thin vertical slice of V0 — every layer of the architecture touched, none of them wide. The point is not feature coverage. The point is to prove the two claims the entire thesis rests on, early enough to abandon them cheaply if they are false:

1. **In-path version pinning costs < 1ms p99.** (Kill criterion: PRD §14.)
2. **A chain verified on an auditor's laptop reproduces byte-identical hashes.** (Test 6 of the resolver spec.)

Everything else in the MVP exists to make those two demonstrable to a stranger.

### In scope

| # | From PRD | Narrowed to |
|---|---|---|
| V0-1 | OpenAI-wire ingress | Chat completions + streaming. Base-URL swap works |
| V0-2 | 4 providers | **OpenAI and Anthropic only** |
| V0-4 | SSE passthrough | No buffering, TTFT measured |
| V0-5 | Version-pin resolver | Full spec, all 8 test cases |
| V0-6 | Async telemetry fork | Bounded channel, drop-on-full, drops counted and logged |
| V0-7 | ClickHouse `audit_events` | Full schema, forward-compatible (see §4) |
| V0-8 | Hash chain + signed checkpoints | ed25519, standalone verifier CLI |
| V0-10 | Docker Compose | `docker compose up` → working gateway |

### Deliberately out, with the reason

| Deferred | Why it's safe to defer |
|---|---|
| Azure OpenAI, vLLM | Same wire format as OpenAI. Additive, not architectural |
| V0-3 budgets / hard cutoff | Sales-relevant, not thesis-relevant. Nov |
| Provider failover | The *pin* semantics for failover (one event per attempt, shared `trace_id`, `attempt_seq`) are designed in now; the failover *logic* is Nov |
| V0-9 trace viewer | `curl` + a ClickHouse query is enough to demo. Dec |
| V0-11 published benchmark post | Benches gate CI from week 3; *publishing* is a Nov marketing act |
| Policy engine (Rego/CEL) | Open question §16.2. `policy_id`/`policy_version` pin to `none` in MVP. Resolving it now costs 2–3 weeks and buys nothing demonstrable |
| RFC 3161 timestamping | Chain works without it. Adds an external dependency to the trust story before anyone has asked |

**Nothing here is the semantic-cache / guardrails / PII / agent category.** PRD §7 non-goals hold absolutely.

---

## 1. Week 0 gate — before the first commit

Your own words in PRD §15, and genuinely blocking.

| Gate | Action | Deadline |
|---|---|---|
| **Static vs. Ancre** | One of them gets the 12h/week from September. Decide and write it down | **By 15 Aug** |

You cannot build a compliance platform and a Discord game on 12 hours a week. This one has revenue at 6 months.

---

## 2. Capacity budget

| Window | Rate | Hours |
|---|---|---|
| 6–31 Aug | Full days | ~140h |
| Sept | ~12h/wk | ~52h |
| Oct | ~12h/wk | ~52h |
| **Total to 31 Oct** | | **~245h** |

August is 57% of the entire budget. **Spend it on the parts that are hard to change later** — canonical encoding, chain, resolver, hot-path shape. September and October are for integration and packaging, which tolerate interruption. Do not spend August on Docker Compose.

---

## 3. Repo layout

Cargo workspace. Three deployable binaries plus a verifier, matching PRD §10's three-container constraint.

```
ancre/
├─ Cargo.toml                  # workspace, shared lints, cargo-deny config
├─ crates/
│  ├─ ancre-canon/             # deterministic CBOR + BLAKE3.  THE TRUST ROOT
│  ├─ ancre-types/             # Pins, RiskClass, AuditEvent, ConfigSnapshot
│  ├─ ancre-chain/             # event hashing, chain verify, checkpoint sign/verify
│  ├─ ancre-resolver/          # PinResolver, ArcSwap snapshot, staleness
│  ├─ ancre-provider/          # OpenAI + Anthropic wire adapters, SSE
│  ├─ ancre-gateway/     [bin]  # hyper proxy, auth, telemetry fork
│  ├─ ancre-ingester/    [bin]  # NATS → seq/chain → ClickHouse
│  ├─ ancre-control/     [bin]  # axum, Postgres, snapshot build, checkpoint signer
│  └─ ancre-verify/      [bin]  # standalone verifier, ships in every evidence pack
├─ bench/                      # criterion, gating in CI
├─ deploy/compose/
└─ docs/mapping-table.md       # requirement → schema. Also the lead magnet
```

`ancre-canon` is a separate crate on purpose. It is the smallest, most audited, least-changing thing in the system, and every other crate's correctness reduces to it. It should have no dependencies beyond `ciborium` and `blake3`, and it should reach 1.0 and stop moving.

`ancre-verify` must compile with **zero workspace-internal dependencies other than `ancre-canon` and `ancre-chain`** — an auditor's laptop should build it in one `cargo install`, and it must not be able to reach the network.

---

## 4. The `audit_events` schema

Design this **once, in week 1**, and treat it as frozen. Adding a hashed field later changes the canonical encoding, which means old chains verify under a different rule set — you would have to re-key or maintain a rule-set registry forever. `canon_version` exists so that if you must break it, you can, explicitly.

Columns that V1 needs (`payload_ref`, `subject_key_id` for crypto-shredding) **exist in the MVP and are empty strings**, so V1 doesn't bump `canon_version`.

```sql
CREATE TABLE audit_events (
  -- identity
  tenant_id         LowCardinality(String),
  system_id         LowCardinality(String),
  seq               UInt64,          -- assigned by INGESTER, gapless per (tenant, system)
  event_id          UUID,
  trace_id          String,
  attempt_seq       UInt16,          -- failover: one event per attempt

  -- chain
  prev_hash         FixedString(32),
  event_hash        FixedString(32),
  canon_version     LowCardinality(String),

  -- time / origin
  occurred_at       DateTime64(6, 'UTC'),
  ingested_at       DateTime64(6, 'UTC'),
  node_id           LowCardinality(String),

  -- event
  event_type        LowCardinality(String),  -- llm.request | provider.failover
                                             -- | config.generation.applied
                                             -- | pin.overridden | telemetry.dropped
  outcome           LowCardinality(String),  -- ok | error | denied | interrupted

  -- pins.  NEVER NULL.  Unknown is the literal string 'unknown' + a risk flag
  config_generation UInt64,
  config_hash       FixedString(32),
  system_version    LowCardinality(String),
  ifu_version       LowCardinality(String),
  model_id          LowCardinality(String),
  model_version     String,
  prompt_id         LowCardinality(String),
  prompt_version    String,
  policy_id         LowCardinality(String),
  policy_version    String,
  gateway_version   LowCardinality(String),
  risk_class        Enum8('unclassified'=0,'minimal'=1,'transparency'=2,'high'=3),
  resolved_stale    UInt8,
  risk_flags        Array(LowCardinality(String)),

  -- payload: digests now, crypto-shredded bodies in V1
  request_digest    FixedString(32),
  response_digest   FixedString(32),
  payload_ref       String,          -- '' in MVP
  subject_key_id    String,          -- '' in MVP

  -- metrics
  provider          LowCardinality(String),
  http_status       UInt16,
  latency_ms        UInt32,
  ttft_ms           UInt32,
  tokens_in         UInt32,
  tokens_out        UInt32,
  error_code        LowCardinality(String)
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(occurred_at)
ORDER BY (tenant_id, system_id, seq)
TTL toDateTime(occurred_at) + INTERVAL 7 YEAR;
```

### Chain rule

```
event_hash = BLAKE3( canon_version ‖ prev_hash ‖ canonical_cbor(hashed_body) )
```

`hashed_body` = every column above **except** `event_hash`, `ingested_at`. `seq` and `prev_hash` **are** hashed — that is what makes reordering detectable.

The gateway emits unordered events with `event_id` and local timestamps. **The ingester allocates `seq` and computes the chain** (PRD §9). A gateway node dying mid-flight therefore cannot create a gap.

### Checkpoints

Every N=10 000 events or T=5 minutes, the control plane signs and writes to Postgres:

```
ed25519_sign( chain_id, seq_from, seq_to, root_hash, built_at, canon_version )
```

`root_hash` is a BLAKE3 tree hash over the range, not just the last `event_hash` — it lets a verifier check a *subset* of the range without replaying the whole chain, which is what an auditor actually does.

---

## 5. Milestones

### M1 — Trust root (8–16 Aug, ~45h)

Workspace, CI, `ancre-canon`, `ancre-chain`, `ancre-verify` skeleton.

**Write resolver-spec test 6 first**: two independently-built snapshots of the same logical config produce byte-identical `config_hash`. Non-deterministic canonical encoding is invisible until an auditor can't verify a chain, and by then every chain you have ever written is suspect.

- [ ] Deterministic CBOR: sorted map keys, no floats, no indefinite lengths, canonical integer widths
- [ ] Property test: `decode(encode(x)) == x` and `encode(x)` stable across 10 000 shuffled-input runs
- [ ] `ancre-verify` verifies a chain from a JSONL fixture, exit code 0/1
- [ ] `cargo-deny` + `cargo-audit` gating in CI from commit 1

**Done when:** a deliberately corrupted event in a 100k-event fixture is caught by the verifier, and it names the seq.

### M2 — Resolver (17–23 Aug, ~45h)

`ancre-resolver` per spec, with the criterion benches gating CI.

- [ ] `ConfigSnapshot` + `ArcSwap`, `Arc<str>` throughout, `PromptRef::Lazy` with bounded LRU
- [ ] Staleness: fresh / stale / cold-start, fail-closed on High
- [ ] Benches: `resolve` p50 < 2µs, p99 < 5µs, **p99 < 8µs under a 10/s reload storm**
- [ ] Spec test cases 1, 3, 4, 7, 8

**Run the reload-storm bench on day 1 of this milestone.** It is the one that catches an `RwLock` mistake, and it is cheap to run before the code is worth defending.

**Gate:** if p99 resolve can't get under 5µs by 23 Aug, stop and re-plan. The whole latency claim descends from this number.

### M3 — Hot path (24–31 Aug, ~50h)

`ancre-gateway`: hyper proxy, OpenAI + Anthropic, SSE straight through, telemetry fork.

- [ ] Base-URL swap works against the real OpenAI SDK, unmodified
- [ ] SSE: first byte forwarded before the second is read. No buffering, anywhere
- [ ] `model_version` read from the **provider's response metadata**, not the request. Floating alias → `unresolved:<alias>` + `RiskFlag::UnpinnedModel`
- [ ] Telemetry fork: bounded channel → batcher → NATS, **never awaited by the request**. Full channel drops, increments `dropped_events`, and the drop is itself an event at recovery
- [ ] Spec test cases 2, 5

**Done when:** an end-to-end p99 overhead measurement exists against a **null-gateway baseline** (same binary, pinning compiled out). "Overhead" only means something as a delta; measuring against direct-to-provider conflates your cost with network variance and will produce a number you can't defend in a sales conversation.

### M4 — Persistence (September, ~52h)

`ancre-ingester` and minimal `ancre-control`.

- [ ] NATS JetStream consume → seq allocation → chain → ClickHouse batch insert
- [ ] Idempotent on redelivery, keyed on `event_id` — JetStream is at-least-once
- [ ] Checkpoint signer, ed25519, keys in Postgres, public key exported
- [ ] Control plane: snapshot build from Postgres, `generation` bump, NATS publish, 10s poll backstop
- [ ] `config.generation.applied` event with `propagation_ms`
- [ ] Substantial-modification diff on reload — **surfaces the candidate, never declares it**

**Done when:** kill ClickHouse for 10 minutes under load; gateway is unaffected, ingester catches up, chain verifies with no gap.

### M5 — Packaging (October, ~52h)

- [ ] `docker compose up` → gateway + control + ingester + ClickHouse + Postgres + NATS, working, seeded
- [ ] Quickstart that a stranger completes in under 10 minutes
- [ ] `ancre verify --pack ./chain` on a laptop, no network
- [ ] Chaos pass: node dies mid-chain, NATS down, control plane down past budget
- [ ] Reproducible benchmark harness (published in November)

**Done when:** you hand the repo URL to someone who has never seen it and they get a verified chain without asking you a question.

---

## 6. GTM track — runs in parallel, not after

Your own kill criterion is *no audit engagement sold by 31 December 2026 → stop*. That verdict is only informative if selling starts now. Discovering in December that you can't reach the buyer costs you the same year as discovering it in August, minus the information.

Budget ~3h/week of the 12. It is the highest-information-per-hour work available.

| Window | Action |
|---|---|
| **Aug** | Publish `docs/mapping-table.md` — AI Act requirement → schema column. It is a *byproduct of §4*, nobody else has published one, and it is the best lead magnet you have. In French |
| **Aug** | **Pick one sector.** Recommendation: **HR tech** — clearest Annex III exposure, shortest cycle, and you need a sale by 31 Dec more than you need a large one. Finance is the better business and the wrong first customer for a solo founder on a deadline |
| **Sept** | 20 conversations. Target: Head of AI Governance / DPO / CISO, FR + BE, 200–5000 employees. Sell the **€8–15k readiness audit**, not the product |
| **Sept** | Approach 3 EU AI Act consultancies / DPO networks re: referral (PRD §12.4) |
| **Oct** | OSS launch of the gateway. Apache 2.0. Distribution, not revenue |
| **Oct–Dec** | Close 2 audit engagements. Each one tells you which schema columns a real buyer cares about — information unobtainable any other way |

Run every audit engagement **against the MVP**. The gap report is the deliverable; the gateway is how you produce it without three weeks of archaeology. That is also the demo.

---

## 7. Checkpoints against the kill criteria

| Date | Check | If it fails |
|---|---|---|
| 15 Aug | Static decided, 12h/wk committed to Ancre | Services-only, or stop |
| 23 Aug | `resolve` p99 < 5µs | Re-plan the architecture |
| 31 Aug | E2E overhead p99 < 2ms vs. null baseline | **Only technical differentiator is gone. Stop** (PRD §14) |
| 30 Sept | ≥1 audit engagement in a real pipeline | Buyer reachability is the problem, not the product |
| 31 Oct | Stranger installs and verifies unaided | Packaging debt; delay OSS launch, don't launch broken |
| 31 Dec | ≥1 audit sold | **Stop** (PRD §14) |

Standing watch, no date: if Langfuse or Portkey ships in-path pinning **plus** signed evidence export, reposition to services (PRD §14).

---

## 8. Decisions to make in week 1

1. **`canon_version` string format.** Pick it now; you can never change it silently. Suggest `ancre-canon/1`.
2. **BLAKE3 vs. SHA-256.** PRD §10 says compile-time feature. Build that feature flag in M1, not later — retrofitting a hash abstraction through a chain crate is miserable.
3. **Checkpoint signing key custody.** In MVP, control-plane-local file via age/SOPS. Document that enterprise moves it to Vault, because the first security questionnaire will ask.
4. **`chain_id` = `(tenant_id, system_id)`?** Yes for MVP. Note the consequence: a system with no traffic has no chain, so absence of evidence and absence of a system look identical. Emit a heartbeat event per chain per day so gaps stay countable — *unknown is a value, not a null* (PRD §6.3).

## 9. Open questions the MVP deliberately does not answer

Carried from PRD §16, and deferring them is the point:

- Rego vs. CEL (§16.2) — no policy engine in the MVP at all
- `Substantial` vs `Material` defensibility (§16.3) — the product surfaces candidates; classification stays a legal determination
- OSS cannibalisation of Team tier (§16.4) — needs six months of conversion data
- CIRIL Décideur as Annex III (§16.1) — market research, not MVP scope
