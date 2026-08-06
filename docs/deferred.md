# Deferred work

Everything knowingly left incomplete, and why. Kept in the repo rather than in
anyone's head, because the failure mode this guards against is shipping
something that *looks* finished.

Two categories, and the distinction matters:

- **Deferred by plan** — the plan says to do this later. Not debt.
- **Debt** — a shortcut taken to keep moving. Someone has to pay it.

Update this file in the same commit that creates or clears an entry.

Last updated: end of M3.

---

## Blocks a customer install

Nothing here is optional before anyone can run this in front of real traffic.

| # | Item | Where | Kind |
|---|---|---|---|
| B1 | **The gateway serves nothing.** `main` starts with a cold resolver because nothing delivers snapshots. Every request 503s. Loud on purpose — it logs a warning at startup rather than pretending | `ancre-gateway/src/main.rs` | M4 |
| B2 | **Audit events are discarded.** `LoggingSink` warns on every batch instead of publishing to NATS. Running this build in front of traffic loses the evidence it exists to produce | `ancre-gateway/src/main.rs` | M4 |
| B3 | **No Dockerfiles.** `compose.yaml` references `Dockerfile.gateway`, `.control`, `.ingester`; none exist, so `docker compose up` fails immediately | `deploy/compose/` | M5 |
| B4 | **The ingester does nothing.** Seq allocation, chain computation and ClickHouse insert are all `todo!()` | `ancre-ingester/` | M4 |
| B5 | **The control plane does nothing.** Snapshot build, publication and the HTTP API are all `todo!()` | `ancre-control/` | M4 |

## Evidence gaps

Things that affect what an auditor can be shown.

| # | Item | Where | Kind |
|---|---|---|---|
| E1 | **Checkpoints are unsigned.** `CheckpointSigner`, `verify_checkpoint` and `verify_inclusion` are `todo!()`. Chains verify; nothing attests *when* they were sealed | `ancre-chain/src/checkpoint.rs` | M4 |
| E2 | **No inclusion-proof format.** An auditor can verify a whole range but cannot be handed a proof for a subset, which is what they actually ask for. The format is a deliverable — design it with `ancre-verify`, and write it down | `ancre-chain/src/checkpoint.rs` | M4 |
| E3 | **No evidence pack.** `ancre-verify` reads a JSONL chain (`--chain`). The `--pack` flag in the original spec does not exist yet: no manifest, no bundled public keys, no bundled verifier binary | `ancre-verify/` | V1 |
| E4 | **`gateway_version` is a lie.** The constant ends in `+unknown` instead of a git SHA, so the pin cannot identify the build that served a request. A `build.rs` fixes it; until then this pin is not evidence | `ancre-gateway/src/lib.rs` | **debt** |
| E5 | **No `chain.heartbeat` emission.** The event type exists and the ingester has a `heartbeat()` stub, but nothing emits one. Until it does, "this system served nothing" and "this system does not exist" are indistinguishable — the exact thing mvp-plan §8.4 says to prevent | `ancre-ingester/` | M4 |
| E6 | **`telemetry.dropped` is never emitted.** Drops are counted and the window is recorded, but no code turns a `DropWindow` into an event. The gap is countable in a metric and invisible in the chain | `ancre-gateway/src/telemetry.rs` | M4 |
| E7 | **`config.generation.applied` is never emitted.** `ReloadOutcome` carries everything the event needs — `propagation_ms`, changed fields, substantial candidates — and nothing writes it | `ancre-gateway`, `ancre-control` | M4 |
| E8 | **`error_code` is only the HTTP status.** Provider error bodies are not parsed for a code, deliberately: guessing at a provider-specific shape would put a fabricated string in the evidence | `ancre-gateway/src/tap.rs` | by design |

## Deferred by plan

Called out in mvp-plan §0 as safe to defer. Listed so they are not mistaken
for oversights.

| # | Item | Planned |
|---|---|---|
| P1 | **Provider failover.** Spec §10 case 2 has no test because there is no failover logic. The pin *semantics* are in place — `attempt_seq` and a shared `trace_id` are on every event — but nothing populates them beyond `attempt_seq: 0` | Nov |
| P2 | **Azure OpenAI and vLLM.** Same wire format as OpenAI; additive, not architectural | Nov |
| P3 | **Budgets and hard cutoff (V0-3).** Sales-relevant, not thesis-relevant | Nov |
| P4 | **Policy engine.** Rego vs CEL is open question §16.2. `policy_id`/`policy_version` pin to `none`, which is a fact and not a gap | V1 |
| P5 | **RFC 3161 timestamping.** Adds an external dependency to the trust story before anyone has asked | V1 |
| P6 | **Trace viewer (V0-9).** `curl` plus a ClickHouse query is enough to demo | Dec |
| P7 | **Published benchmark post (V0-11).** The benches gate CI now; publishing is a marketing act | Nov |
| P8 | **`docs/mapping-table.md` is a stub.** Two of the Article 12(2) subparagraphs are filled in; the rest, the Annex IV cross-reference, and the French translation are outstanding. This is the lead magnet, so it is GTM work, not engineering | Aug |

## Debt

Shortcuts. Each one is cheap now and expensive later.

| # | Item | Where | Cost if ignored |
|---|---|---|---|
| D1 | **`verify_range` holds every leaf hash resident** to compute the range root — 32 bytes per event, so ~320MB for a 10M-event range. Fine for a pack today | `ancre-chain/src/verify.rs` | An auditor's laptop OOMs on a large range |
| D2 | **`PromptRef::Inline` is never constructed.** `ConfigSnapshot::build` always produces `Lazy`, and nothing fetches bodies, so no prompt body is ever resident. Pins are unaffected — they only need the hash | `ancre-types`, `ancre-resolver` | Prompt bodies cannot be shown next to the events that used them |
| D3 | **Multi-line SSE `data:` fields take the first line.** Legal in SSE, emitted by no LLM provider, and joining fragments would allocate on the hot path | `ancre-provider/src/sse.rs` | A future provider's pins are silently truncated |
| D4 | **`bench-drift` does not gate.** Criterion alone cannot fail a build; it needs `critcmp` against a stored baseline | `.github/workflows/ci.yml` | Slow drift between the absolute gates goes unnoticed |
| D5 | **CI has never run.** No git remote, no GitHub repo. Every command in the workflow passes locally, but the `latency-gate` job on a shared runner will be noisier than a 20-core dev box | `.github/workflows/ci.yml` | The first push is a surprise; expect to tune thresholds or mark the gate advisory |
| D6 | **`#![allow(dead_code, unreachable_pub)]`** at the top of the ingester and control binaries, because `main` is `todo!()` | `ancre-ingester`, `ancre-control` | Real dead code hides behind it once those crates are implemented. Remove as M4 lands |
| D7 | **No `cargo-deny` run locally.** It gates in CI, which has never run | — | A licence or advisory problem surfaces later than it should |
| D8 | **The provider-pinned-id heuristic is shape-based.** `looks_pinned` matches a trailing date. It errs toward flagging on purpose — a false `UnpinnedModel` costs a minute of review, a false "pinned" is evidence that lies | `ancre-provider/src/lib.rs` | A new provider id format reads as unpinned until the matcher learns it |

## Open questions carried from the PRD

Deferring these is the point; see PRD §16.

- Rego via regorus, or a smaller CEL policy layer (§16.2)
- Whether `Substantial` vs `Material` classification is defensible without
  legal review (§16.3) — assume no until reviewed
- Whether the OSS gateway cannibalises the Team tier (§16.4) — needs six
  months of conversion data
- Which sector first (§16.5)
