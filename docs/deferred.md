# Deferred work

Everything knowingly left incomplete, and why. Kept in the repo rather than in
anyone's head, because the failure mode this guards against is shipping
something that *looks* finished.

Two categories, and the distinction matters:

- **Deferred by plan** — the plan says to do this later. Not debt.
- **Debt** — a shortcut taken to keep moving. Someone has to pay it.

Update this file in the same commit that creates or clears an entry.

Last updated: end of M5. The MVP is complete.

The seam has moved again, and this time it moved off the critical path. Every
milestone's done-when has been met: `docker compose up` brings up a seeded,
working gateway; `deploy/compose/quickstart.sh` takes a stranger from a clone
to a verified evidence pack without a question and without anybody's API key;
and the last step of that script edits a row in ClickHouse and watches both the
chain and the signature over it refuse it.

What is left is listed below, and none of it blocks an install. The largest
single item is that **CI has still never run** (D5) — there is no remote — so
every claim in this repository is a claim about a 20-core dev machine.

---

## Blocks a customer install

Nothing. The table that used to live here is empty for the first time.

## Evidence gaps

Things that affect what an auditor can be shown.

| # | Item | Where | Kind |
|---|---|---|---|
| E3 | **A pack is a directory, not an artefact.** `ancre-verify --pack` verifies one offline — chain, checkpoint signatures, and which events those signatures actually cover — and `quickstart.sh` assembles one from the three read endpoints. What the original spec asked for and this is not: a *single file* an auditor can be emailed, with the verifier binary inside it. Both halves are packaging, not trust: the trust story is complete, because a checkpoint is a signature over a tree root the verifier recomputes from the events themselves | `ancre-verify/src/pack.rs`, `deploy/compose/quickstart.sh` | V1 |
| E12 | **`pin.overridden` is declared and never emitted.** The frozen schema lists it as an event type and three doc comments say the gateway emits it "naming the fields". What actually happens is that `RiskFlag::PinOverridden` rides on the `llm.request` event whose pins were overridden — which is arguably the better evidence, since the flag is attached to the request that used the override rather than sitting in a separate row. What is genuinely missing is *which* field was overridden and what the registry would have said. `PinOverrides` has both at resolve time and neither survives into `Pins` | `ancre-resolver/src/lib.rs`, `ancre-gateway/src/tap.rs` | **debt** |
| E10 | **`changed_fields` is derived, never stored.** The frozen schema has no column for it and will not grow one, so `config.generation.applied` pins the before/after `config_hash` in `request_digest`/`response_digest` and the field-level diff is recomputed from the two snapshots. Nothing yet *does* the recomputation, and nothing yet retains snapshots by content hash — `GET /v1/snapshot` serves only the current one | `ancre-control/src/api.rs` | V1 |
| E11 | **Two event types overload a metrics column.** `config.generation.applied` carries `propagation_ms` in `latency_ms` and the change class in `error_code`; `telemetry.dropped` carries the number of lost events in `tokens_out`. Each is documented at the call site and none is wrong, but a reader of the raw table needs the event type to interpret them. The alternative was new columns, which the frozen encoding forbids | `ancre-gateway/src/config_feed.rs`, `ancre-gateway/src/telemetry.rs` | by design |
| E8 | **`error_code` is only the HTTP status.** Provider error bodies are not parsed for a code, deliberately: guessing at a provider-specific shape would put a fabricated string in the evidence | `ancre-gateway/src/tap.rs` | by design |
| E13 | **A pack's manifest is unsigned, and so is the `prev_hash` it may declare.** The verifier says so on both counts and neither is load-bearing — the checkpoints are what bind the pack, and an attacker who re-anchors a forged prefix still cannot produce a signature over it. It does mean a partial export's anchor should be confirmed out of band, which `--from` exists for | `ancre-verify/src/pack.rs` | by design |

## Cleared in M5

Kept briefly so the history is readable; delete at the start of M6.

- ~~B3 no Dockerfiles~~ — one `deploy/compose/Dockerfile` with a shared builder
  and five runtime stages, selected by `target:`. One Dockerfile and not three,
  so the three services cannot be built from three different commits; the
  verifier ships in the control image and in a `network_mode: none` container
  of its own, because an auditor is not required to own a Rust toolchain
- ~~no seed data~~ — `002_seed.sql` ships one tenant, one High-risk system, two
  routes and a demo key. `crates/ancre-gateway/tests/seed.rs` recomputes both
  of its hashes from the functions the gateway calls, so a drifted key hash
  fails a test instead of failing somebody's first evaluation with a 401
- ~~`ANCRE_SIGNING_KEY_PATH` pointed at `/run/secrets/`~~ — the control plane
  *mints* that key on first boot and a secrets mount is read-only, so nothing
  could ever have started. It is a named volume now, which also means a
  `down && up` keeps signing with the same key and the previous run's
  checkpoints keep verifying
- ~~no quickstart~~ — `deploy/compose/quickstart.sh`: six containers, traffic
  including a floating alias and an overridden pin, an evidence pack, an
  offline verification, then a row edited directly in ClickHouse and a second
  verification that fails with exit code 1
- ~~E6 `telemetry.dropped` is never emitted~~ — the batcher emits it, because
  the batcher is the only thing that knows when the bus came back. Drops are
  counted **per chain**, keyed on the event handed back by the full channel: a
  node-wide count could only be reported into one arbitrary chain or repeated
  into every chain as though each had lost all of them, and both are wrong in a
  way an auditor would act on. A failed recovery publish restores the windows
  rather than destroying the record it was trying to save
- ~~E4 `gateway_version` is a lie~~ — `build.rs` in both the gateway and the
  control plane stamps the commit, from `ANCRE_BUILD_SHA` in a container build
  and from `git rev-parse` in a checkout, with `unknown` as the honest fallback
  and `.dirty` when the tree was not clean. The gateway compares its own build
  against the snapshot's at startup and logs the disagreement, because during a
  rolling deploy the pin legitimately names the control plane's build
- ~~the M5 checklist's `ancre verify --pack`~~ — see E3 for the half that is
  packaging rather than verification
- ~~D9 the chaos gate only ever ran against fakes~~ — `deploy/compose/chaos.sh`
  kills the real NATS, the real ClickHouse and the real control plane under
  load, and fails if the gateway stops serving where it should not, if the
  chain gaps, if the drops go unrecorded, or if the gateway *keeps* serving
  where it must fail closed. `bench`'s in-memory gate stays: it is fast, it
  runs on a laptop with no Docker, and the two check different things
- ~~a stable configuration went stale and failed closed~~ — **found by the
  first chaos run against real containers, forty seconds into a deployment
  with nothing wrong with it.** `poll_once` skipped the staleness clock when
  the generation had not changed, on the reasoning that the clock should
  measure the config's age rather than the poll loop's. It should measure
  neither: the budget bounds how long a node may serve under a configuration
  that has been *superseded*, and a control plane answering "still generation
  41" is positive evidence that it has not been. As written, every healthy
  fleet would have begun refusing all High-risk traffic one budget after its
  last configuration edit. `PinResolver::confirm_fresh` now restarts the clock
  on a successful poll that changed nothing — and, as the second test insists,
  on nothing else

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
| D5 | **CI has never run.** No git remote, no GitHub repo. Every command in the workflow passes locally, including the `transports` job run container-for-container as written — but the `latency-gate` job on a shared runner will be noisier than a 20-core dev box, and no job builds the container images at all, so the Dockerfile is verified only by the fact that a human ran the quickstart | `.github/workflows/ci.yml` | The first push is a surprise; expect to tune thresholds or mark the gate advisory |
| D18 | **A stale-artefact class of build bug is fixed by `touch`, not by design.** `COPY` preserves mtimes and cargo decides freshness by mtime, so a persistent `/build/target` cache mount can serve artefacts compiled from an earlier version of a file that was edited before the previous build. It happened during M5 and produced a compile error several crates away from the cause. The workaround touches every workspace source before building — correct, and it recompiles nine crates every image build | `deploy/compose/Dockerfile` | A future change to the caching strategy reintroduces images whose binaries do not match their source |
| D1 | **`verify_range` holds every leaf hash resident** to compute the range root — 32 bytes per event, so ~320MB for a 10M-event range. `ChainSource::leaves` has the same shape on the writing side, and `--pack` now adds a third: it reads the whole `events.jsonl` into memory before verifying, where `--chain` streams | `ancre-chain/src/verify.rs`, `ancre-control/src/clickhouse.rs`, `ancre-verify/src/pack.rs` | An auditor's laptop OOMs on a large pack; a catch-up tick OOMs the control plane |
| D2 | **`PromptRef::Inline` is never constructed.** `ConfigSnapshot::build` always produces `Lazy`, and the gateway never fetches a body, so no prompt body is ever resident on the hot path. Pins are unaffected — they only need the hash. The control plane now *stores* bodies and serves them from `GET /v1/prompts/{hash}`, verified against their own key, so the missing half is the fetch and the resolver's LRU | `ancre-types`, `ancre-resolver` | Prompt bodies cannot be shown next to the events that used them |
| D3 | **Multi-line SSE `data:` fields take the first line.** Legal in SSE, emitted by no LLM provider, and joining fragments would allocate on the hot path | `ancre-provider/src/sse.rs` | A future provider's pins are silently truncated |
| D4 | **`bench-drift` does not gate.** Criterion alone cannot fail a build; it needs `critcmp` against a stored baseline | `.github/workflows/ci.yml` | Slow drift between the absolute gates goes unnoticed |
| D17 | **The export endpoint is unauthenticated,** like the checkpoints it sits beside — but unlike a checkpoint it serves *content*. Digests rather than prompts and completions, so no payload leaks, yet `system_id`, timings, token counts and model versions are a competitive picture of how a customer runs their AI. Authz for the read API is V1; until then a deployment that cares has to put its own gateway in front | `ancre-control/src/api.rs` | A customer's AI usage profile is readable by anyone who can reach the control plane |
| D15 | **Heartbeats only cover chains this process has already seen.** `Ingester::heartbeats` walks its live `ChainWriter`s, and a restart starts with none — so a system that goes quiet across a restart stops emitting the daily heartbeat that makes its silence countable. Absence of evidence and absence of a system look identical again, which is the exact thing the heartbeat exists to prevent (mvp-plan §8.4). The fix is seeding writers from the store's chain list at startup | `ancre-ingester/src/pipeline.rs` | A silent system is indistinguishable from a decommissioned one, after any restart |
| D16 | **The gateway trusts the first snapshot it is handed.** Cold start retries for 60s and then exits, which is right, but there is no lower bound on what it will accept — an empty registry publishes an empty snapshot, and the gateway installs it and serves 503s for every key. The compose stack now closes the *timing* half of this by making Postgres unhealthy until the registry has at least one key, so the control plane cannot publish an empty snapshot during init; the gateway itself still has no floor | `ancre-gateway/src/main.rs` | A registry pointed at the wrong database looks like a working gateway with no customers |
| D14 | **Generation allocation is serialised; publication is not.** `allocate_generation` holds an advisory lock, so two control-plane replicas never mint the same number. The bus send happens after the lock is released, so replica A can allocate 42, replica B allocate 43 and publish first, and A's 42 lands after it. `PinResolver::reload` installs whatever it is given, so a gateway would go backwards a generation until the next publish. One replica is the MVP deployment and the fix is a monotonicity check in `reload`, which is cheap — it is listed rather than done because the check needs a decision about what a gateway should do when it *legitimately* sees a lower generation after a control-plane rollback | `ancre-control/src/postgres.rs`, `ancre-resolver/src/lib.rs` | Two replicas can flip a fleet between two configurations |
| D12 | **The control plane's poll backstop refuses to serve unpublished edits.** `current()` errors if the registry has changed since the last publish, rather than publishing on demand. Now that `main` runs a 10s publish loop the window is ten seconds rather than forever, so this has gone from a support call to a confusing 503 during a deploy — still worth a better error than the one it has | `ancre-control/src/snapshot.rs` | A 503 from `GET /v1/snapshot` that reads as an outage and is actually a race with the publish loop |
| D7 | **No `cargo-deny` run locally.** It gates in CI, which has never run | — | A licence or advisory problem surfaces later than it should |
| D20 | **The chaos pass is not in CI.** `chaos.sh` takes minutes of wall clock, most of it waiting out a staleness budget, and it is the only check in this repository that a human has to remember to run. It caught a bug that would have made the product unusable in production, which is precisely the argument for automating it | `deploy/compose/chaos.sh` | The next regression of the same class is found by a customer |
| D10 | **Dedupe is a bounded in-memory window** of 100 000 `event_id`s. The durable check has to be a unique index on `event_id` in ClickHouse, and that DDL is not written | `ancre-ingester/src/chain_writer.rs` | A redelivery older than the window doubles an event, the chain still verifies, and the checkpointer then refuses the range for good — `tick` compares the leaf count against the range and will not sign a root over a set it cannot account for, so that chain stops being attested |
| D11 | **One `ChainWriter` per chain, unbounded.** An ingester serving ten thousand systems holds ten thousand writers, each with a dedupe window. No eviction. The gateway's drop counter has the same shape and *is* bounded — 4 096 chains, then an `unknown/unknown` overflow bucket — which is the pattern to copy here | `ancre-ingester/src/pipeline.rs` | Memory grows with tenant count rather than with traffic |
| D19 | **The demo lowers the checkpoint policy and the quickstart says so.** `quickstart.sh` and `chaos.sh` set 20 events / 30 seconds against a shipped default of 10 000 / 5 minutes, so that the signed half of each lands inside somebody's attention span. It is a real configuration knob rather than a special case in the code, but it does mean neither script demonstrates the default cadence | `deploy/compose/quickstart.sh` | Someone infers the default checkpoint interval from the demo and is surprised in production |
| D21 | **A compose environment variable only exists if `compose.yaml` names it.** The quickstart exported the checkpoint policy for several runs while the control plane quietly used its defaults, and the demo still worked, which is why nobody noticed. The three are declared with defaults now. Nothing checks that a *future* setting does not repeat this, and the failure is silent by construction | `deploy/compose/compose.yaml` | A knob that appears to be set is not, and the behaviour attributed to it is coming from somewhere else |
| D8 | **The provider-pinned-id heuristic is shape-based.** `looks_pinned` matches a trailing date. It errs toward flagging on purpose — a false `UnpinnedModel` costs a minute of review, a false "pinned" is evidence that lies | `ancre-provider/src/lib.rs` | A new provider id format reads as unpinned until the matcher learns it |

## Open questions carried from the PRD

Deferring these is the point; see PRD §16.

- Rego via regorus, or a smaller CEL policy layer (§16.2)
- Whether `Substantial` vs `Material` classification is defensible without
  legal review (§16.3) — assume no until reviewed
- Whether the OSS gateway cannibalises the Team tier (§16.4) — needs six
  months of conversion data
- Which sector first (§16.5)
