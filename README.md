# Ancre

An LLM gateway that produces regulator-grade evidence as a side effect of
serving traffic.

**Status: the MVP is complete. M1–M5 done.**

```sh
./deploy/compose/quickstart.sh
```

Six containers, no API key, and about ten minutes — most of which is compiling.
It starts a seeded gateway, sends traffic through it, builds an evidence pack,
verifies it in a container with no network interface, then edits one row
directly in ClickHouse and verifies again. The second verification fails and
names the event. That last step is the product; everything before it is setup.

Under it: the deterministic encoder, the hash chain, the offline verifier, the
in-path pin resolver, the request pipeline, the ingester's chaining, and the
control plane's snapshot build and checkpoint signing — 342 tests, both hash
back-ends, both latency gates passing. Thirty-four of those run against a real
ClickHouse, Postgres and NATS.

`docs/deferred.md` lists every remaining gap, with what it costs. Nothing in it
blocks an install.

## Documents

| File | What it is |
|---|---|
| `ancre-prd-and-architecture.md` | PRD, architecture, tech stack, GTM, kill criteria |
| `mvp-plan.md` | Plan to 31 Oct 2026, milestones M1–M5, frozen event schema |
| `version-pin-resolver-spec.md` | The resolver, in detail. The hard part |
| `docs/mapping-table.md` | AI Act requirement → schema column. Stub; also the lead magnet |
| `docs/deferred.md` | **Everything knowingly incomplete, and why.** Read before trusting anything here |

## Layout

```
crates/
  ancre-canon/      deterministic CBOR + hashing.  THE TRUST ROOT
  ancre-types/      pins, audit events, config snapshots
  ancre-chain/      event hashing, chain verify, checkpoint sign/verify
  ancre-resolver/   PinResolver, ArcSwap snapshot, staleness
  ancre-provider/   OpenAI + Anthropic wire adapters, SSE
  ancre-gateway/    [bin] hyper proxy, auth, telemetry fork
  ancre-ingester/   [bin] NATS → seq/chain → ClickHouse
  ancre-control/    [bin] axum, Postgres, snapshot build, checkpoint signer
  ancre-verify/     [bin] standalone offline verifier
bench/              criterion, gating CI from week 3
deploy/compose/     one-command self-host, and the quickstart
```

Three deployable binaries plus a verifier — PRD §10 caps production at three
containers, and that ceiling is a customer-adoption constraint, not a
preference.

## Try it

```sh
cargo run -p ancre-verify --example gen-fixture > chain.jsonl
cargo run -p ancre-verify -- --chain chain.jsonl
```

```
Chain verified: 50 000 events, seq 1–50000, no violations.
  range root: 709a200ee658750b4785d8c082c0593ff0e5de2804a2757df8958b5ea9d26b10
  chain head: 9eb965f6b8c0a2e59fcb561286c26e2ecd62c6128fd3c648e07f51fd0df885bc
```

Tamper with one event and it says which one:

```sh
cargo run -p ancre-verify --example gen-fixture -- tamper > tampered.jsonl
cargo run -p ancre-verify -- --chain tampered.jsonl; echo "exit=$?"
```

```
VERIFICATION FAILED: 50 000 events, seq 1–50000, 1 violation.
  - event 41207 was altered after it was sealed: it records hash 50e1d2… but
    its contents hash to b56c60…
exit=1
```

Exit codes are 0 clean, 1 violations, **2 cannot verify**. The third is not
decoration: "this build does not implement the rule set these events were
sealed under" is a different answer from "this chain is invalid", and merging
them would be dishonest in the direction that costs the most credibility.

## What M1 established

- **Deterministic CBOR** (`ancre-canon`): RFC 8949 canonical ordering, no
  floats, duplicate keys refused, and non-canonical input **rejected rather
  than normalised** — otherwise an attacker picks which of two byte sequences
  a verifier sees for the same event.
- **RFC 6962 Merkle tree** for range roots — the Certificate Transparency
  construction, so a subset can be proven without replaying the chain, and so
  `[a,b,c]` and `[a,b,c,c]` cannot share a root.
- **Length-prefixed chain rule**, so no value can be shifted across a field
  boundary without changing the digest.
- **Explicit wire forms**: every enum has a frozen `as_str()`, and timestamps
  are integer microseconds. The canonical encoding depends on this repo's own
  code plus ciborium and blake3 — never on how `time` or `uuid` happen to
  serialize this year.

## What M2 established

The gate the whole thesis descends from (resolver spec §9):

```sh
cargo run -p ancre-bench --release --example resolve-gate
```

| Measurement                              | Target  | Actual |
|------------------------------------------|---------|--------|
| `resolve` p50, quiescent                 | < 2µs   | 138ns  |
| `resolve` p99, quiescent                 | < 5µs   | 152ns  |
| `resolve` p99, 10/s reload storm         | < 8µs   | 158ns  |
| **`resolve` p99, all cores + storm**     | < 8µs   | **2µs** |
| `resolve` p99, 10k systems / 50k routes  | < 5µs   | 194ns  |
| snapshot build, 10k systems / 50k routes | < 500ms | 110ms  |

*(20-core dev machine, 500k samples per case. Quote the saturated row — the
single-reader numbers are the uncontended floor and they flatter the design.)*

A bench that has never failed is not evidence, so there is a control:

```sh
cargo run -p ancre-bench --release --example swap-control
```

It runs the same load through `ArcSwap` and through `RwLock<Arc<T>>`. At full
core count the lock is **2.1× worse at the p99** — so the storm bench really
does have the power to catch the mistake it exists to catch. At 8 readers on a
20-core box the two are within 1.2× of each other, which is exactly why an
under-subscribed bench is a trap.

Design decisions worth knowing:

- **Staleness is measured on the local monotonic clock**, not against the
  control plane's `built_at`. Cross-machine skew must never feed a fail-closed
  decision, and "time since this node refreshed" is the honest reading of the
  bounded-staleness claim anyway.
- **`generation` is excluded from `config_hash`**, making it a true content
  identifier — so "generation bumped, nothing changed" is visible, which is
  the question substantial-modification review actually asks.
- **Cold start is an installed snapshot that refuses**, not an
  `Option<Snapshot>` — no hot-path branch, and no invitation for a future
  `unwrap_or_default()` to serve traffic with empty pins.
- **Snapshots are validated at build time**, so the hot path indexes
  `routes[default_route]` with no bounds check and no `Option`.

## What M3 established

```sh
cargo run -p ancre-bench --release --example overhead-gate
```

| Measurement                          | Target | Actual |
|--------------------------------------|--------|--------|
| p99 added overhead vs null baseline  | < 2ms  | ~0     |
| p99 added TTFT vs null baseline      | < 2ms  | ~0     |
| p99 total gateway cost, in-process   | < 1ms  | 3µs    |

The baseline is the **same binary with pinning compiled out**, not
direct-to-provider — that would fold network variance into the number and
produce something that falls apart the first time a prospect's own engineer
reproduces it. The delta isolates what pinning costs (~250ns at p50); the
total row is what a customer actually feels.

Design decisions worth knowing:

- **The response body forwards before it observes.** `TappedBody` hands each
  frame downstream in the same poll it reads it, then scans the bytes already
  in flight. Observation cannot delay a token.
- **`model_version` comes from the provider's response**, never the request.
  An id that does not name specific weights is recorded as
  `unresolved:<alias>` with `RiskFlag::UnpinnedModel` — the alias stays
  visible, because "we asked for gpt-4o and the provider would not say" is a
  more useful answer than `unknown`.
- **A client that hangs up mid-stream still produces an event**, via the
  body's `Drop`. No event at all would be indistinguishable from no request.
- **Untranslatable requests are refused, not rewritten.** Tool calls and
  multiple system messages have no faithful Anthropic equivalent, so they
  400. A wrong event is worse than a rejected request.
- **`emit()` returns nothing.** No caller on the request path may branch on
  telemetry success — a full channel drops, counts, and keeps serving.

## The whole thing, end to end

Three processes, three dependencies, one pipe — `quickstart.sh` runs all of
what follows. Traffic goes through the gateway; what comes out the other end is
a chain anybody can check.

```sh
curl -s localhost:8081/v1/chains/acme/hr-screening/events | ancre-verify --chain -
```

```
Chain verified: 30 events, seq 1–30, no violations.
  range root: fdfdd912838cfa4af13cd2a07283adb3e86e6692be9c4e5a1dd40f3a0c7fa9bc
  chain head: 47caa5f14360ee63113f3bad632a4edd03867ec5b0ae3f1f86b16b70cddec785
```

Then edit one row directly in ClickHouse — the database the events live in,
with full DDL rights:

```sh
clickhouse-client -q "ALTER TABLE ancre.audit_events UPDATE tokens_out = 999 WHERE seq = 17"
curl -s localhost:8081/v1/chains/acme/hr-screening/events | ancre-verify --chain -
```

```
VERIFICATION FAILED: 30 events, seq 1–30, 1 violation.
  - event 17 was altered after it was sealed: it records hash b118141240f89bc9…
    but its contents hash to 1395cac429dd87cc…
```

Append-only is enforced by the hash chain, not by the engine. That is the point
of the chain: the store is not trusted, and neither is the person who runs it.

The events are served as newline-delimited JSON, which is exactly what the
verifier reads — no new encoding to get wrong between the two. Signed
checkpoints come from `/v1/checkpoints/{tenant}/{system}` and the public keys
from `/v1/pubkeys`.

## The evidence pack

Those three responses plus a short manifest are an *evidence pack*, and
`ancre-verify --pack` checks the whole thing offline:

```sh
docker compose --profile tools run --rm verify --pack /evidence/acme-hr-screening
```

```
Pack: acme/hr-screening
  produced 2026-08-09T07:16:26Z by ancre quickstart, build a77de9bbc5cc

Chain verified: 8 events, seq 1–8, no violations.
  range root: 4c1e…
  chain head: 9d70…

Checkpoints: 1 of 1 verified.
  seq 1–8  root fde7ed1cb22ea8cb  signed by cp-ea3c03a599d0f1ce
Attested range: seq 1–8.

Signatures were checked against this pack's own keys. That proves the pack is
internally consistent and says nothing about who made it — compare these
fingerprints with the ones you were given separately, or re-run with --key:
  cp-ea3c03a599d0f1ce  ea3c03a599d0f1ce…  (active since 2026-08-09T07:15:55Z)
```

Two verdicts, not one, because the two properties fail independently: the chain
says the events are internally consistent, the checkpoints say a key signed
them. **Attested range** is the number that matters — it is the span an
auditor can rely on, and a hole between two signed ranges stays visible as a
hole rather than being averaged into a percentage.

Three things about that output are deliberate:

- The verifier recomputes each event's hash rather than trusting the one the
  file carries, so a checkpoint attests the events' *contents*. Feeding it the
  recorded hashes would have made the signature attest an attacker's own
  arithmetic — and in the tamper run above, that is why the signature check
  fails on its own terms and not merely as an echo of the chain's verdict.
- The circularity is printed, not hidden. A pack carries the keys that signed
  it, so verifying against them proves consistency and nothing about
  provenance. `--key <HEX>` pins a fingerprint obtained elsewhere, and then a
  forged pack fails.
- `network_mode: none` on that container. The claim is that verification needs
  no network; a container with no network interface is the version of that
  claim a sceptic cannot argue with.

## What M4 and M5 established

Persistence and the transports under it.

- **The ingester owns `seq`.** The gateway never assigns one, so a skewed
  clock on one node cannot reorder a chain. A batch the store refuses is
  rolled back across every chain it touched and left unacked — acking a subset
  would leave the store missing events the bus believes were consumed, and the
  chain would resume past a gap it can never fill.
- **Checkpoints are ed25519 over a tree root**, not over the last hash, so an
  auditor can verify one event without replaying the chain. They live in
  Postgres and not in ClickHouse: storing the attestation in the store it
  attests to hands anyone who can rewrite the events the ability to re-sign
  them.
- **Rotation never invalidates an old checkpoint.** Every key that was ever
  active stays exported with its window, and the windows abut exactly — a gap
  would leave checkpoints sealed inside it unattributable to any key an
  auditor holds.
- **A generation is burned, never reused.** If the bus send fails after the
  number is allocated, the next publish moves past it. Two configurations
  sharing a `config_generation` would make every pin carrying it ambiguous
  forever, which is worse than a gap in a counter.
- **Round trips are hash-critical, so they are tested against real servers.**
  `AuditEvent` is nested and the table is flat; a conversion that changes one
  hashed byte leaves a chain that verifies inside the ingester and fails on the
  auditor's laptop. The first run against a real ClickHouse found four bugs no
  unit test could have.

```sh
docker run -d --name ancre-pg -p 15432:5432 \
  -e POSTGRES_USER=ancre -e POSTGRES_PASSWORD=ancre -e POSTGRES_DB=ancre \
  -v "$PWD/deploy/compose/init/postgres:/docker-entrypoint-initdb.d:ro" \
  postgres:16-alpine
ANCRE_TEST_POSTGRES=postgres://ancre:ancre@127.0.0.1:15432/ancre \
  cargo test -p ancre-control --test postgres
```

Each transport suite is a no-op without its `ANCRE_TEST_*` variable, so
`cargo test` stays green without Docker. CI's `transports` job always sets
them.

## What M5 established

Packaging, which is the milestone whose done-when is about a person rather than
a number: someone who has never seen the repo gets a verified chain without
asking a question.

- **One Dockerfile, not three.** A shared builder stage and five runtime stages
  selected by `target:`. Three files would mean three independent builds of the
  same workspace, and three chances for one service to be built from a
  different commit than the two it talks to.
- **The registry ships populated.** An empty registry publishes an empty
  snapshot; the gateway installs it and refuses every key, so the first thing a
  stranger would see is the product failing. Postgres is not reported healthy
  until the seed has landed, which closes the same race during startup.
- **The seeded hashes are checked by a test**, not typed and trusted. A key
  hash that drifts from `auth::key_hash` authenticates nothing, and the symptom
  would be a 401 ten minutes into somebody's first evaluation.
- **The demo shows the unhappy pins too.** One request comes back with a
  floating alias — recorded as `unresolved:gpt-4o-preview` with the
  `unpinned_model` flag — and one carries a caller's pin override. An evidence
  system that only ever demonstrates clean rows has not been demonstrated.
- **`gateway_version` names a commit.** `build.rs` stamps it, `.dirty` when the
  tree was not clean, and the gateway logs it at startup when its own build
  disagrees with the snapshot's — which happens legitimately mid-deploy, and
  should never happen silently.
- **Dropped telemetry is an event, per chain.** The batcher emits
  `telemetry.dropped` when the bus comes back, attributed to the chain that
  lost the events, because a node-wide count could only be reported into one
  arbitrary chain or into all of them as though each had lost everything.

The whole of it is a CI job, and it fails if the tampered pack *passes*.

### The chaos pass

```sh
./deploy/compose/chaos.sh
```

Three outages against the real containers, each with a claim that fails the
script if it is false: the bus dies and the gateway keeps serving while the
lost evidence is counted and later recorded; the store dies and the ingester
defers rather than acking, then catches up with no gap; the control plane dies
and the gateway serves from its installed snapshot until the staleness budget
expires and then **refuses** High-risk traffic.

The first run of this found a bug that no unit test could have. `poll_once`
skipped the staleness clock when the control plane reported an unchanged
generation, reasoning that the clock should measure the configuration's age. It
should measure neither the configuration's age nor the poll loop's: the budget
bounds how long a node may serve under a configuration that has been
*superseded*, and a control plane answering "still generation 41" is evidence
that it has not been. As written, every healthy fleet would have started
refusing all High-risk traffic thirty seconds after its last config edit.

That is the argument for killing real containers rather than fakes, and it is
why the third check is written to fail if the gateway keeps serving.

## Build

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
```

Both hash back-ends must keep passing:

```sh
cargo test -p ancre-canon --no-default-features --features hash-sha256
```

## Language

We sell **evidence**, **traceability** and **readiness** — never "compliance"
or "conformity". No harmonised standards exist yet, so no product can deliver
conformity, and overclaiming loses deals at legal review. Say
**tamper-evident**, never tamper-proof (PRD §5).
