# Deferred work

Everything knowingly left incomplete, and why. Kept in the repo rather than in
anyone's head, because the failure mode this guards against is shipping
something that *looks* finished.

Two categories, and the distinction matters:

- **Deferred by plan** — the plan says to do this later. Not debt.
- **Debt** — a shortcut taken to keep moving. Someone has to pay it.

Update this file in the same commit that creates or clears an entry.

Last updated: end of M4.

M4 landed in two halves, and the seam between them is the honest summary of
where this repo is: **the logic is written and gated; none of the transports
are.** Chaining, rollback, dedupe, heartbeats, snapshot build, generation
allocation, publication, checkpoint scheduling, key rotation, the read API and
the gateway's config feed all exist and are tested against in-memory
implementations of four traits — `EventStore`, `Registry`, `ChainSource`,
`SnapshotBus`. Behind those traits there is no ClickHouse, no Postgres and no
NATS, so all three binaries still exit non-zero saying so. That is B1–B5.

---

## Blocks a customer install

Nothing here is optional before anyone can run this in front of real traffic.

| # | Item | Where | Kind |
|---|---|---|---|
| B1 | **The gateway serves nothing.** `ConfigFeed` verifies, installs and records a generation, and is tested — but `main` constructs no `SnapshotSource`, so the resolver stays cold and every request 503s. Loud on purpose: it warns at startup rather than pretending | `ancre-gateway/src/main.rs` | M5 |
| B2 | **Audit events are discarded.** `LoggingSink` warns on every batch instead of publishing to NATS. Running this build in front of traffic loses the evidence it exists to produce | `ancre-gateway/src/main.rs` | M5 |
| B3 | **No Dockerfiles.** `compose.yaml` references `Dockerfile.gateway`, `.control`, `.ingester`; none exist, so `docker compose up` fails immediately | `deploy/compose/` | M5 |
| B4 | **The ingester has no transport.** Chaining, rollback, dedupe and heartbeats are implemented and tested against a fake store. The NATS consumer and the ClickHouse `EventStore` impl are not written | `ancre-ingester/src/main.rs` | M5 |
| B5 | **The control plane has no datastores.** Build, publication, generation allocation, checkpointing, key rotation and the read API are implemented and tested; the Postgres `Registry`, the ClickHouse `ChainSource` and the NATS `SnapshotBus` are not written, so `main` exits non-zero | `ancre-control/src/main.rs` | M5 |

## Evidence gaps

Things that affect what an auditor can be shown.

| # | Item | Where | Kind |
|---|---|---|---|
| E3 | **No evidence pack.** `ancre-verify` reads a JSONL chain (`--chain`). The `--pack` flag in the original spec does not exist yet: no manifest, no bundled public keys, no bundled verifier binary | `ancre-verify/` | V1 |
| E4 | **`gateway_version` is a lie.** The constant ends in `+unknown` instead of a git SHA, so the pin cannot identify the build that served a request. A `build.rs` fixes it; until then this pin is not evidence | `ancre-gateway/src/lib.rs` | **debt** |
| E6 | **`telemetry.dropped` is never emitted.** Drops are counted and the window is recorded, but no code turns a `DropWindow` into an event. The gap is countable in a metric and invisible in the chain | `ancre-gateway/src/telemetry.rs` | M5 |
| E10 | **`changed_fields` is derived, never stored.** The frozen schema has no column for it and will not grow one, so `config.generation.applied` pins the before/after `config_hash` in `request_digest`/`response_digest` and the field-level diff is recomputed from the two snapshots. Nothing yet *does* the recomputation, and nothing yet retains snapshots by content hash — `GET /v1/snapshot` serves only the current one | `ancre-control/src/api.rs` | V1 |
| E11 | **`config.generation.applied` overloads two columns.** `latency_ms` carries `propagation_ms` and `error_code` carries the change class. Both are documented at the call site and neither is wrong, but a reader of the raw table needs the event type to interpret them. The alternative was new columns, which the frozen encoding forbids | `ancre-gateway/src/config_feed.rs` | by design |
| E8 | **`error_code` is only the HTTP status.** Provider error bodies are not parsed for a code, deliberately: guessing at a provider-specific shape would put a fabricated string in the evidence | `ancre-gateway/src/tap.rs` | by design |

## Cleared in M4

Kept briefly so the history is readable; delete at M5.

- ~~E1 checkpoints unsigned~~ — ed25519 signing, `seal_range`, `verify_checkpoint`
- ~~E2 no inclusion proofs~~ — RFC 6962 audit paths, `prove_inclusion` / `verify_inclusion`
- ~~E5 no heartbeat emission~~ — `ChainWriter::heartbeat`, deterministic per (chain, day)
- ~~E7 `config.generation.applied` never emitted~~ — `ConfigFeed::record`, one
  event per affected chain, with `propagation_ms` and the substantial
  candidate surfaced. What it cannot carry is now E10/E11
- ~~E9 checkpoints never issued~~ — `Checkpointer::tick`, N-events-or-T-minutes
  per chain, `KeyRing` rotation, `GET /v1/pubkeys`. Postgres storage is B5
- ~~D6 `#![allow(dead_code, unreachable_pub)]`~~ — gone from both the ingester
  and the control binary; nothing in the workspace suppresses either lint now

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
| D12 | **The control plane's poll backstop refuses to serve unpublished edits.** `current()` errors if the registry has changed since the last publish, rather than publishing on demand — safe, but it means an operator who edits the registry and forgets to publish sees 503s from `GET /v1/snapshot` with no hint that a publish is what is missing | `ancre-control/src/snapshot.rs` | An avoidable support call, and the error text is the only thing that prevents it |
| D13 | **Nothing runs the checkpointer or the publish loop.** `tick()` and `publish()` are correct and tested; the task that calls them on a timer lives in `main`, which is not written | `ancre-control/src/main.rs` | Both are inert until the binary exists |
| D7 | **No `cargo-deny` run locally.** It gates in CI, which has never run | — | A licence or advisory problem surfaces later than it should |
| D9 | **The chaos gate uses an in-memory store.** It proves the chaining and rollback logic survives an outage, not that the ClickHouse client does. The real check needs a live ClickHouse | `bench/examples/chaos-gate.rs` | A ClickHouse-specific failure (partial batch, connection reset mid-insert) is untested |
| D10 | **Dedupe is a bounded in-memory window** of 100 000 `event_id`s. The durable check has to be a unique index on `event_id` in ClickHouse, and that DDL is not written | `ancre-ingester/src/chain_writer.rs` | A redelivery older than the window doubles an event, and the chain still verifies |
| D11 | **One `ChainWriter` per chain, unbounded.** An ingester serving ten thousand systems holds ten thousand writers, each with a dedupe window. No eviction | `ancre-ingester/src/pipeline.rs` | Memory grows with tenant count rather than with traffic |
| D8 | **The provider-pinned-id heuristic is shape-based.** `looks_pinned` matches a trailing date. It errs toward flagging on purpose — a false `UnpinnedModel` costs a minute of review, a false "pinned" is evidence that lies | `ancre-provider/src/lib.rs` | A new provider id format reads as unpinned until the matcher learns it |

## Open questions carried from the PRD

Deferring these is the point; see PRD §16.

- Rego via regorus, or a smaller CEL policy layer (§16.2)
- Whether `Substantial` vs `Material` classification is defensible without
  legal review (§16.3) — assume no until reviewed
- Whether the OSS gateway cannibalises the Team tier (§16.4) — needs six
  months of conversion data
- Which sector first (§16.5)
