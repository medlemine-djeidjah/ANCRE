# PRD — AI Act evidence platform

Working name: **Ancre**. Working spec, v0.1.

---

## 0. One page

**What.** An LLM gateway that produces regulator-grade evidence as a side effect of serving traffic.

**For whom.** EU enterprises deploying LLMs in regulated or high-risk contexts — banks, insurers, health, HR software, public administration — who will need to demonstrate Article 12 logging, Article 14 human oversight and Article 15 accuracy before December 2027, and who need Article 50 transparency proof today.

**Why it wins.** Every observability vendor sells dashboards to platform engineers. Nobody sells a signed, verifiable evidence pack to a compliance officer. The two products need the same instrumentation but have different buyers, different budgets and different sales cycles, and the compliance budget is larger and less price-sensitive.

**Why not just Langfuse.** Langfuse gives you traces. It does not pin versions in-path, does not model the human overseer as an actor, cannot produce a tamper-evident chain, and has no answer for the GDPR-erasure-versus-retention collision. Those four gaps are the product.

**Honest read.** The gateway market is crowded and consolidating. The compliance-evidence niche is not, but it is slower, more legalistic, and gated on standards that do not exist yet. This is a €1–3M ARR European business with a plausible acquisition path, not a rocket ship. Build it that way.

---

## 1. Problem

Three facts that do not currently coexist in any product:

1. **Article 12 requires automatic event recording over a system's lifetime**, and Articles 19 / 26(6) put a ≥6-month retention duty on providers and deployers respectively. Existing LLM observability is sampled, mutable and TTL'd at 30 days. It is operationally useful and evidentially worthless.
2. **Article 14 requires demonstrable human oversight** — the ability to disregard, override, reverse and interrupt. No product records whether humans actually do. An oversight regime with a 99.7% acceptance rate is theatre, and that is the first thing a market surveillance authority will probe.
3. **GDPR Article 17 erasure and AI Act retention pull in opposite directions.** Every vendor either ignores this or makes the customer choose. It is solvable with crypto-shredding, but only if designed in from the schema up.

The buyer's current state: traces in Langfuse, evals in a notebook, a risk assessment in a Word file, and no way to connect any of them. When the auditor asks "which model version produced this decision on 14 March," the answer is a three-week archaeology project.

## 2. Why now, honestly

Post-Omnibus the calendar is: Article 50 transparency live since 2 August 2026; Annex III high-risk from 2 December 2027; Annex I embedded from 2 August 2028.

The delay cuts both ways. Sixteen months of urgency evaporated, so fear-selling in Q3/Q4 2026 will not work. But sixteen months is also exactly enough runway for a solo founder to build the thing properly and be the incumbent when 2027 budgets form. And two mechanics keep pressure on:

- **Grandfathering resets on substantial modification.** Systems placed on market before the date escape the high-risk regime until materially changed. Every model swap is a candidate trigger. That is a recurring event, which is what a subscription needs.
- **Article 50 is live and unmoved.** Chatbot disclosure and synthetic-content marking are enforceable now. That is a small but real wedge you can sell this quarter.

The window closes if Portkey or Langfuse ship an "AI Act export" button. They will, eventually. The defensible part is not the export — it is the in-path version pinning and the human-oversight model, which require touching the request path and the customer's UI respectively.

## 3. Competition

| Who | What they are | Why they don't do this |
|---|---|---|
| Langfuse, Braintrust, Arize, Opik | Observability + eval | No gateway in path, no pinning, mutable stores, no evidence artifact |
| Portkey, TrueFoundry, LiteLLM, Bifrost, Kong AI Gateway | Gateways | Compliance is a checkbox in a feature grid; logs are ops telemetry, not evidence |
| WitnessAI, Trussed, Compliora, VerifyWise | AI governance / GRC | Consultancies with dashboards. Cannot instrument a request path. Their evidence is what the customer types in |
| Datadog, Dynatrace | APM extending into LLM | Volume-priced, US-hosted, no AI Act semantics |

Nobody sits in the request path *and* speaks regulator. That gap is the whole thesis. It is not a large gap and it will not stay open for four years.

## 4. Customer

**Primary ICP.** EU company, 200–5000 employees, at least one LLM system touching an Annex III category, already has a DPO and either an AI governance owner or a CISO who inherited the problem. Sectors, in order of pain: financial services, insurance, HR tech, health, public sector.

**Buyer** — Head of AI Governance / DPO / CISO. Holds the budget, fears personal exposure, cannot evaluate a trace schema.

**Champion** — the platform or ML engineer who has been asked to "make us AI Act compliant" and has no idea what that means technically. Wins by being handed a mapping table instead of a legal memo.

**Blocker** — the same platform engineer, if you look like another vendor bolting overhead onto their inference path. Hence the latency budget is a *sales* requirement, not an engineering vanity metric.

**Geography.** France and Benelux first. You have the language, the market proximity and, for public-sector adjacency, the sovereignty story. Germany second. Do not chase US customers; they will not buy EU compliance from a French solo founder.

## 5. Positioning

> Ancre sits in front of your models and turns every request into evidence. Signed, verifiable, EU-resident, and exportable as a pack your auditor can actually read.

**We are not:** an eval framework, a prompt IDE, a GRC platform, a legal advisor, an agent orchestration layer, a model router optimising for cost.

**We never say "compliant."** No harmonised standards exist yet, so no product can deliver conformity. We sell *evidence*, *traceability* and *readiness*. Overclaiming here loses deals at legal review and is the single fastest way to destroy credibility with this buyer.

**We say tamper-evident, never tamper-proof.**

## 6. Product principles

1. **The request path is sacred.** Nothing that adds evidence may add meaningful latency. Every feature is measured against the p99 budget before it ships.
2. **Evidence is a by-product, not a workflow.** If the customer has to remember to do something, it will not be in the record.
3. **Unknown is a value, not a null.** Gaps must be countable, because countable gaps are invoices.
4. **Generated, never typed.** Documentation comes out of the registry. The moment a human types a version number into a form, the evidence is hearsay.
5. **Surface candidates, never declare conclusions.** Substantial modification, risk classification and incident severity are legal determinations. The product flags; the human decides; the decision is logged.
6. **Self-host is the default, not a downgrade.** This buyer will not send prompts to a US-hosted SaaS.

---

## 7. Scope

### V0 — "It logs and it's fast" (target: end of October 2026)

Goal: a gateway someone will actually put in their path.

| # | Requirement | Priority |
|---|---|---|
| V0-1 | OpenAI-wire-compatible ingress; adoption = base URL change | P0 |
| V0-2 | Providers: OpenAI, Anthropic, Azure OpenAI, vLLM/OpenAI-compatible self-hosted | P0 |
| V0-3 | Virtual keys, per-key and per-project budgets, hard cutoff | P0 |
| V0-4 | Streaming passthrough (SSE) with no buffering | P0 |
| V0-5 | Version-pin resolver, in-path, per spec | P0 |
| V0-6 | Async telemetry fork, non-blocking, drop-on-full with counter | P0 |
| V0-7 | ClickHouse ingest + `audit_events` schema | P0 |
| V0-8 | Hash-chained events + signed checkpoints | P0 |
| V0-9 | Trace viewer that isn't embarrassing | P1 |
| V0-10 | Docker Compose single-command self-host | P0 |
| V0-11 | Published latency benchmark, reproducible | P1 |

**Explicit non-goals in V0:** semantic cache, guardrails, PII detection, fine-tuning, RAG, agent orchestration, no-code builder, multi-region, cost optimisation.

### V1 — "It produces evidence" (target: March 2027)

| # | Requirement | Priority |
|---|---|---|
| V1-1 | System registry: inventory, role, risk classification with reasoning | P0 |
| V1-2 | MCP proxy: per-tool policy, full tool-call and denial logging | P0 |
| V1-3 | Human oversight API: accept / override / reverse / interrupt events from the customer's UI | P0 |
| V1-4 | Override-rate and automation-bias reporting | P0 |
| V1-5 | Evidence pack export: signed ZIP + standalone verifier | P0 |
| V1-6 | Crypto-shredded payload store with per-subject keys | P0 |
| V1-7 | Retention policy with un-lowerable 180-day floor | P0 |
| V1-8 | Article 50 disclosure verification events | P1 |
| V1-9 | Scheduled eval runs, thresholds, regression alerts | P1 |
| V1-10 | Annex IV documentation generation from registry | P1 |
| V1-11 | SSO (OIDC + SAML), RBAC | P0 for enterprise |

### V2 — "It closes the loop" (2027 H2)

Incident lifecycle with Article 73 deadline clocks; post-market monitoring reports; drift detection; prompt/config optimiser (GEPA/MIPRO-style) against the eval harness; multi-tenant managed EU offering; biometric Article 12(3) extension on demand.

The optimiser is last deliberately. It is the most interesting engineering and the least likely to close a deal.

---

## 8. Non-functional requirements

| Category | Requirement |
|---|---|
| Latency | p99 added overhead < 1ms; TTFT delta < 2ms; pin resolution p99 < 5µs |
| Throughput | 500 RPS per 2 vCPU node, linear horizontal scale |
| Availability | Gateway serves through a total control-plane outage (up to staleness budget) and through a total ClickHouse outage |
| Durability | Zero audit event loss under normal operation; drops counted and themselves logged |
| Data residency | Managed offering: EU only, EU-owned infrastructure, no US subprocessors |
| Payload egress | Self-host mode: zero prompt/completion data leaves the customer network. Non-negotiable |
| Verifiability | Two nodes on the same generation produce byte-identical config hashes |
| Retention | ≥180 days enforced floor; 7-year default ceiling |
| Startup | Cold start fails closed. No traffic served with unknown pins on a high-risk system |

---

## 9. Architecture

### Components

| Component | Responsibility | Runs where |
|---|---|---|
| **Gateway** | Request path. Auth, pin resolution, routing, policy, provider calls, MCP proxy, telemetry fork | Customer network, N replicas |
| **Control plane API** | Registry CRUD, keys, policies, prompts, config generation, checkpoint signing | Customer network, 1–2 replicas |
| **Ingester** | Bus → ClickHouse batching, chain verification, checkpointing | Customer network |
| **Eval worker** | Scheduled and triggered eval runs, scoring, optimiser jobs | Customer network, scale-to-zero |
| **Web UI** | Traces, registry, oversight reports, evidence export | Static, served by control plane |
| **Verifier CLI** | Standalone chain verification, ships inside every evidence pack | Auditor's laptop |

### Request flow

1. Ingress → TLS termination → key hash lookup → **pins resolved once** into `RequestCtx`
2. Policy evaluation against `policy_version` in the pinned snapshot
3. Provider call (or MCP tool call), streamed straight through
4. Telemetry fork: bounded channel → batcher → NATS JetStream. **Never awaited by the request**
5. Ingester consumes, computes `event_hash` chained on `prev_hash`, batch-inserts to ClickHouse
6. Checkpointer signs `(chain_id, seq_to, root_hash)` every N events or T minutes into Postgres

### Failure behaviour

| Failure | Behaviour |
|---|---|
| ClickHouse down | Gateway unaffected. NATS buffers. Ingester catches up |
| NATS down | Bounded channel fills, events dropped, `dropped_events` counter incremented and itself logged at recovery. Traffic served |
| Control plane down | Gateway serves on last snapshot until staleness budget; then fail closed for high-risk systems, flagged for others |
| Provider down | Failover per route config, one audit event per attempt with distinct model pins |
| Gateway node dies mid-chain | Chains are per (tenant, system) and `seq` is allocated by the ingester, not the gateway — a dead node cannot create a gap |

Note that last point: **the gateway does not assign `seq` or compute the chain.** It emits unordered events with monotonic local timestamps; the ingester serialises and chains them. This keeps the hot path free of coordination and means node failure never breaks a chain.

### Deployment topologies

1. **OSS self-host** — Compose or Helm, Apache 2.0 gateway + core, free. Distribution channel.
2. **Enterprise self-host** — licensed, adds SSO, RBAC, evidence pack, audit modules. Customer's infrastructure, air-gappable.
3. **BYOC / in-VPC** — you operate it inside their cloud account. Highest price, highest support cost.
4. **Managed EU** — hosted on Scaleway or OVH. Last, because this buyer prefers self-host and because it puts you on the hook for SOC 2 sooner.

---

## 10. Tech stack

I am revising the advice I gave earlier about splitting the control plane into TypeScript or Python. For a self-hosted enterprise product, **a small number of deployable artifacts beats developer velocity**, because every extra runtime is another thing the customer's platform team has to accept. Rust for gateway *and* control plane, sharing a types crate; Python only where the ML ecosystem genuinely lives.

### Data plane — Rust

| Concern | Choice | Why, and what was rejected |
|---|---|---|
| Runtime | tokio | Nothing else is close |
| HTTP | hyper 1.x + axum + tower | axum for ergonomics, raw hyper for the proxy hot path where axum's extractors cost more than they're worth |
| TLS | rustls + aws-lc-rs | No OpenSSL dependency; matters enormously for a static-binary distribution story |
| Config snapshot | arc-swap | Rejected `RwLock<Arc<T>>` — shared cache line on reads shows up in p99 |
| Hashing | blake3 | Faster than SHA-256, tree-hashing helps checkpoint roots. Note: SHA-256 may be required by some customers' crypto policies — make it a compile-time feature |
| Canonical encoding | ciborium with deterministic settings | Rejected JSON — no canonical form without writing one yourself |
| Signing | ed25519-dalek | Small, fast, well-audited |
| Timestamping | RFC 3161 client | Removes "you backdated your own signature" from the objection list |
| Cache | moka | Prompt bodies, key lookups. Rejected Redis — one less thing the customer must run |
| Rate limiting | governor | GCRA, no allocation on the happy path |
| Policy | regorus (Rust Rego) | Deterministic, hashable compiled AST → `policy_version` falls out for free. Alternative: cel-rust if Rego is too heavy. Rejected writing your own DSL |
| DB | sqlx (Postgres) | Compile-time checked queries. Rejected Diesel — async story is worse |
| ClickHouse | clickhouse-rs | Native protocol, batch insert |
| Bus | async-nats | JetStream for durability |
| Telemetry | opentelemetry-rust, GenAI semconv | Customers already have OTLP collectors; export compatibility is an adoption lever |
| Tracing | tracing + tracing-subscriber | Platform's own observability |
| Benchmarks | criterion, gating in CI | The latency claim is a product feature; regressions must fail the build |

### Infrastructure

| Concern | Choice | Why |
|---|---|---|
| Event store | ClickHouse | You already run it. Nothing else does this volume at this cost. Caveat: `ALTER DELETE` exists, so append-only is enforced by the hash chain, not the engine |
| Control plane store | Postgres 16 | Boring, correct |
| Payload blobs | S3 API — MinIO self-host, Scaleway Object Storage managed | Per-subject envelope encryption for crypto-shredding |
| Bus | NATS JetStream | Single binary, trivial self-host. Rejected Kafka (operational weight) and Redpanda (still heavier than NATS for this volume). Revisit if a customer exceeds ~50k events/s |
| Auth | Zitadel or Dex for OIDC/SAML brokering | Do not build SAML. Rejected Keycloak — too heavy to bundle |
| Secrets | age/SOPS for config; Vault integration as an enterprise feature | |

### Eval and optimiser — Python

Separate service, off the hot path, so language choice is free and the ML ecosystem wins. Python 3.12, uv, pydantic, your existing Opik/GEPA/MIPRO work. Talks to the control plane over the same HTTP API a customer would use — dogfooding the public API is how it stays good.

### Frontend — TypeScript

React + Vite + TanStack Router/Query + Tailwind + shadcn/ui. uPlot for dense time series (Recharts falls over on 100k points); a hand-rolled canvas waterfall for traces. Embedded in the control plane binary via `rust-embed` so there is no separate web deployment.

### Build and supply chain

GitHub Actions; `cargo-deny` and `cargo-audit` gating; SBOM per release; signed release artifacts. Take this seriously — LiteLLM had a PyPI supply-chain compromise in March 2026, and you are selling to security-conscious buyers who will ask.

**Artifact count in production: three containers** (gateway, control plane, ingester) plus ClickHouse, Postgres, NATS, MinIO. That is already at the edge of what a mid-market platform team will accept. Do not add a fourth.

---

## 11. Pricing

| Tier | Price | Contents |
|---|---|---|
| OSS | Free, Apache 2.0 | Gateway, routing, keys, budgets, traces, self-host |
| Team | €490/mo | Hosted control plane or licensed self-host, registry, eval runs, 5 seats |
| Enterprise | from €35k/yr | Evidence packs, oversight reporting, SSO/RBAC/audit, crypto-shredding, in-VPC, support SLA |
| Readiness audit | €8–15k | Fixed-scope 5-day engagement, delivers the gap report |
| Implementation | €15–30k | Onboarding an enterprise deal |

Land with the audit, expand to Enterprise. The audit is not a loss leader — it is the discovery process, it is cash-positive from month one, and it tells you which columns real buyers care about. That last part is information you cannot get any other way.

## 12. Go to market

1. **Audit engagements first, product second.** Sell five readiness audits before writing V1. If you cannot sell five, the thesis is wrong and you have lost weeks instead of a year.
2. **Open source the gateway for distribution.** Nobody buys compliance from an unknown. They will run a fast Apache-2.0 gateway from a stranger.
3. **Publish the mapping table as content.** The requirement→schema mapping is genuinely useful and nobody else has published one. It is the best lead magnet available to you and it costs nothing.
4. **Channel: EU AI Act consultancies and DPO networks.** They have the buyer relationships and no technical product. Referral or reseller.
5. **Write in French.** Every competitor's material is English. The French compliance buyer is meaningfully underserved and you are native in that market.

## 13. Milestones against your actual calendar

| Window | Capacity | Target |
|---|---|---|
| Aug 2026 | Full days | V0-1 through V0-8. Gateway that proxies, pins, chains. Nothing else |
| Sept–Dec 2026 | ~12h/wk, CIRIL started | V0 complete, published benchmark, OSS launch, first 2 audit engagements sold |
| Jan–Mar 2027 | ~12h/wk | V1 core: registry, MCP proxy, oversight API, evidence pack |
| Apr–Aug 2027 | ~12h/wk | First paying Enterprise customer. Decide: raise, stay lifestyle, or kill |
| Sept–Dec 2027 | — | Annex III deadline 2 Dec. Be installed before budgets close in Q3 |

## 14. Success metrics and kill criteria

**Leading:** OSS installs, gateway p99 overhead, audit engagements sold, design-partner conversations per month.

**Lagging:** paying enterprise customers, ARR, evidence packs actually submitted to an auditor.

**Kill criteria — write these down now while you are unattached to the idea:**

- No audit engagement sold by 31 December 2026 → the buyer does not exist or you cannot reach them. Stop.
- V0 p99 overhead cannot get under 2ms by end of October → your only technical differentiator is gone. Stop.
- Langfuse or Portkey ships in-path version pinning plus a signed evidence export before you reach V1 → reposition to services, or stop.
- You are still the only engineer in September 2027 with a paying customer → you are the bottleneck and the product will die of support load. Raise or sell.

## 15. Risks

| Risk | Severity | Response |
|---|---|---|
| **This competes with *Static* for the same 12h/week** | High | You cannot build a compliance platform and a Discord game on 12 hours. Pick one this month. This one has revenue at 6 months; the game has revenue at 18 if it works at all |
| Deadline moves again | Medium | Article 50 revenue is unaffected. Diversify the pitch toward ISO 42001 and sectoral regulators (DORA, MDR) which do not move |
| No harmonised standards → cannot claim conformity | Medium | Never claim it. Sell evidence and readiness. This is a positioning constraint, not a blocker |
| Incumbent ships an export button | High | Depth: in-path pinning and human-oversight modelling require touching the request path and the customer's UI. Hard to bolt on |
| SOC 2 / ISO 42001 gate on first enterprise deal | Medium | Self-host first — it removes most questionnaire surface. Budget €15–25k and 4 months when a deal requires it |
| Solo founder support load | High | Self-host default, Enterprise priced high enough to fund support, no managed tier until there is a second person |

## 16. Open questions

1. Does CIRIL's Décideur fall under Annex III via "access to essential public services"? Free market research and a possible first design partner.
2. Rego via regorus, or a smaller CEL policy layer? Decide by prototyping `policy_version` derivation on both.
3. Is `Substantial` vs `Material` modification classification defensible without legal review? Assume no until reviewed.
4. Does the OSS gateway cannibalise Team tier, or feed Enterprise? Watch conversion for six months before adding restrictions.
5. Which sector first — finance has budget and the longest cycle; HR tech has the clearest Annex III exposure and less money. Pick one, do not straddle.
