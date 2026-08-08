# Deferred work

Everything knowingly left incomplete, and why. Kept in the repo rather than in
anyone's head, because the failure mode this guards against is shipping
something that *looks* finished.

Two categories, and the distinction matters:

- **Deferred by plan** — the plan says to do this later. Not debt.
- **Debt** — a shortcut taken to keep moving. Someone has to pay it.

Update this file in the same commit that creates or clears an entry.

Last updated: M5, after all three binaries run.

The seam has moved. Through M4 the honest summary was "the logic is written and
gated; none of the transports are". All four are now written and tested against
real servers — the ClickHouse `EventStore`, the NATS `EventSink` and consumer,
the Postgres `Registry`/`CheckpointStore`/`KeyDirectory`, the ClickHouse
`ChainSource` and the NATS `SnapshotBus` — and 32 integration tests behind
`ANCRE_TEST_*` gate them in CI's `transports` job.

All three binaries now run and talk to each other. Traffic through the gateway
produces a chained, pinned, checkpointed and signed record end to end, against
real ClickHouse, Postgres and NATS — which is the first time that sentence has
been true, and it immediately found a bug that no fake could have (see the
cleared list).

What remains is **packaging and export**: there are no Dockerfiles, and — the
sharper of the two — there is no way to get a chain *out* of the store and in
front of an auditor. B3 and B6.

---

## Blocks a customer install

Nothing here is optional before anyone can run this in front of real traffic.

| # | Item | Where | Kind |
|---|---|---|---|
| B3 | **No Dockerfiles.** `compose.yaml` references `Dockerfile.gateway`, `.control`, `.ingester`; none exist, so `docker compose up` fails immediately. No seed data either, so "working, seeded" is unmet in two ways. `ANCRE_SIGNING_KEY_PATH` also points at `/run/secrets/`, which the control plane cannot create a key in | `deploy/compose/` | M5 |
| B6 | **A chain cannot be exported.** `ancre-verify` reads JSONL and nothing produces JSONL from ClickHouse, so the events this system exists to produce are verifiable in principle and unreachable in practice. The demo ends at "trust this SQL query", which is the opposite of the pitch | `ancre-verify/`, `ancre-ingester/src/clickhouse.rs` | M5 |

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

## Cleared in M5

Kept briefly so the history is readable; delete at the end of M5.

- ~~B5 the control plane has no datastores~~ — `PgStore` behind `Registry`,
  `CheckpointStore` and `KeyDirectory`; `ClickHouseChains` behind
  `ChainSource`; `NatsSnapshotBus` behind `SnapshotBus`. `main` connects all
  three, installs the signing key, publishes once before binding the socket and
  serves the read API. 14 + 5 + 2 tests against real servers
- ~~D13 nothing runs the checkpointer or the publish loop~~ — both are tasks in
  `ancre-control/src/main.rs`, on 30s and 10s timers. A failing tick logs and
  the loop continues: a control plane that exits takes the fleet's config
  updates with it
- ~~the transport suites never run~~ — CI's `transports` job starts ClickHouse,
  Postgres and NATS and runs all five integration suites with their
  `ANCRE_TEST_*` variables set. They stay no-ops on a laptop without Docker
- ~~B1 the gateway serves nothing~~ / ~~B2 audit events are discarded~~ — `main`
  installs the first snapshot **before** binding the socket, then runs the bus
  watcher and the poll backstop together, and publishes to NATS through the
  real sink. Provider endpoints are overridable, so a demo needs nobody's API
  key
- ~~B4 the ingester's `main` is a stub~~ — one consumer task owning every
  `ChainWriter`, with the daily heartbeat driven from that same loop, because
  one writer per chain is a correctness requirement and a heartbeat is an
  append like any other
- ~~every completed request was recorded as `interrupted`~~ — found by running
  the three binaries together, not by any test. Hyper stops polling a body once
  its last byte is written, so `poll_frame` never returned `None` and the tap
  only ever finished from `Drop`, which is the client-hung-up path. Every 200
  carried an outcome that contradicted its own status code. Fixed by finishing
  on the inner body's end-of-stream, plus `ProxyBody` forwarding
  `is_end_stream`/`size_hint` instead of defaulting them. The regression test
  polls exactly as many frames as the body has and no more — the extra poll a
  `collect()` makes is what hid this

The M4 list is deleted; it is in the git history and the entries it cleared
have not come back.

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
| D1 | **`verify_range` holds every leaf hash resident** to compute the range root — 32 bytes per event, so ~320MB for a 10M-event range. `ChainSource::leaves` has the same shape on the writing side: a checkpointer catching up over a long outage reads the whole range into memory. Fine for a pack today | `ancre-chain/src/verify.rs`, `ancre-control/src/clickhouse.rs` | An auditor's laptop OOMs on a large range; a catch-up tick OOMs the control plane |
| D2 | **`PromptRef::Inline` is never constructed.** `ConfigSnapshot::build` always produces `Lazy`, and the gateway never fetches a body, so no prompt body is ever resident on the hot path. Pins are unaffected — they only need the hash. The control plane now *stores* bodies and serves them from `GET /v1/prompts/{hash}`, verified against their own key, so the missing half is the fetch and the resolver's LRU | `ancre-types`, `ancre-resolver` | Prompt bodies cannot be shown next to the events that used them |
| D3 | **Multi-line SSE `data:` fields take the first line.** Legal in SSE, emitted by no LLM provider, and joining fragments would allocate on the hot path | `ancre-provider/src/sse.rs` | A future provider's pins are silently truncated |
| D4 | **`bench-drift` does not gate.** Criterion alone cannot fail a build; it needs `critcmp` against a stored baseline | `.github/workflows/ci.yml` | Slow drift between the absolute gates goes unnoticed |
| D5 | **CI has never run.** No git remote, no GitHub repo. Every command in the workflow passes locally — including the new `transports` job, run container-for-container as written — but the `latency-gate` job on a shared runner will be noisier than a 20-core dev box | `.github/workflows/ci.yml` | The first push is a surprise; expect to tune thresholds or mark the gate advisory |
| D15 | **Heartbeats only cover chains this process has already seen.** `Ingester::heartbeats` walks its live `ChainWriter`s, and a restart starts with none — so a system that goes quiet across a restart stops emitting the daily heartbeat that makes its silence countable. Absence of evidence and absence of a system look identical again, which is the exact thing the heartbeat exists to prevent (mvp-plan §8.4). The fix is seeding writers from the store's chain list at startup | `ancre-ingester/src/pipeline.rs` | A silent system is indistinguishable from a decommissioned one, after any restart |
| D16 | **The gateway trusts the first snapshot it is handed.** Cold start retries for 60s and then exits, which is right, but there is no lower bound on what it will accept — an empty registry publishes an empty snapshot, and the gateway installs it and serves 503s for every key. Correct behaviour for an empty registry; indistinguishable from a misconfigured one | `ancre-gateway/src/main.rs` | A registry pointed at the wrong database looks like a working gateway with no customers |
| D14 | **Generation allocation is serialised; publication is not.** `allocate_generation` holds an advisory lock, so two control-plane replicas never mint the same number. The bus send happens after the lock is released, so replica A can allocate 42, replica B allocate 43 and publish first, and A's 42 lands after it. `PinResolver::reload` installs whatever it is given, so a gateway would go backwards a generation until the next publish. One replica is the MVP deployment and the fix is a monotonicity check in `reload`, which is cheap — it is listed rather than done because the check needs a decision about what a gateway should do when it *legitimately* sees a lower generation after a control-plane rollback | `ancre-control/src/postgres.rs`, `ancre-resolver/src/lib.rs` | Two replicas can flip a fleet between two configurations |
| D12 | **The control plane's poll backstop refuses to serve unpublished edits.** `current()` errors if the registry has changed since the last publish, rather than publishing on demand. Now that `main` runs a 10s publish loop the window is ten seconds rather than forever, so this has gone from a support call to a confusing 503 during a deploy — still worth a better error than the one it has | `ancre-control/src/snapshot.rs` | A 503 from `GET /v1/snapshot` that reads as an outage and is actually a race with the publish loop |
| D7 | **No `cargo-deny` run locally.** It gates in CI, which has never run | — | A licence or advisory problem surfaces later than it should |
| D9 | **The chaos gate uses an in-memory store.** It proves the chaining and rollback logic survives an outage, not that the ClickHouse client does. The real check needs a live ClickHouse | `bench/examples/chaos-gate.rs` | A ClickHouse-specific failure (partial batch, connection reset mid-insert) is untested |
| D10 | **Dedupe is a bounded in-memory window** of 100 000 `event_id`s. The durable check has to be a unique index on `event_id` in ClickHouse, and that DDL is not written | `ancre-ingester/src/chain_writer.rs` | A redelivery older than the window doubles an event, the chain still verifies, and the checkpointer then refuses the range for good — `tick` compares the leaf count against the range and will not sign a root over a set it cannot account for, so that chain stops being attested |
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
