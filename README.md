# Ancre

An LLM gateway that produces regulator-grade evidence as a side effect of
serving traffic.

You put it in your request path with a base-URL change. What comes out the
other end is a hash-chained, signed record of every inference decision — which
model version actually answered, under which prompt, which configuration, at
what risk classification — that anyone can verify on a laptop, offline, without
trusting you or the database it came from.

**Status: MVP complete (M1–M5), plus the dashboard (M6).** 373 tests, both hash back-ends, both latency
gates passing; 34 of those run against a real ClickHouse, Postgres and NATS.
`docs/deferred.md` lists every remaining gap, with what it costs. Nothing in it
blocks an install; several entries should change how you deploy it.

---

## See it work

```sh
./deploy/compose/quickstart.sh
```

No API key, no configuration, about ten minutes — most of that compiling. It
brings up six containers with a seeded customer, sends traffic through the
gateway, builds an evidence pack, verifies it in a container with **no network
interface**, then edits one row directly in ClickHouse and verifies again:

```
VERIFICATION FAILED: 9 events, seq 1–9, 1 violation.
  - event 3 was altered after it was sealed: it records hash 98d498eb… but
    its contents hash to 530580b7…

CHECKPOINT VERIFICATION FAILED: 1 of 1 did not verify.
  - the events for seq 1–9 do not produce the root this checkpoint signed:
    it attests fde7ed1cb22ea8cb but these events hash to d356a81e3b11693c
Attested range: none.
```

That is the product. Everything before it is setup. Append-only is enforced by
the hash chain, not by the storage engine — the store is not trusted, and
neither is whoever runs it.

The chain the demo produces is deliberately not all clean:

| seq | model asked for | recorded as | flags |
|---|---|---|---|
| 1–6 | `gpt-4o` | `gpt-4o-2024-08-06` | — |
| 7 | `claude-sonnet-4-5` | `claude-sonnet-4-5-20250929` | — |
| 8 | `gpt-4o-preview` | `unresolved:gpt-4o-preview` | `unpinned_model` |
| 9 | `gpt-4o` | `gpt-4o-2024-08-06` | `pin_overridden` |

Row 7 is the same client and the same OpenAI-wire request routed to Anthropic
instead. Row 8 is the provider refusing to say which weights ran. Row 9 is a
caller overriding a pin from a header. An evidence system that only ever
demonstrates clean rows has not been demonstrated.

---

## The dashboard

The control plane serves one at its own port — static assets compiled into the
binary, so there is no fourth container. Sign in with `ANCRE_ADMIN_TOKEN`; the
quickstart prints it.

It lists chains, shows each one's counted shape, and opens every event's full
pin set, digests and chain links. What it refuses to show is a green
**verified** tick. The server rendering that page is the server that stores the
events, so a claim it makes about their integrity is worth nothing to an
auditor — instead it reports which sequence ranges carry a signature, and hands
over an evidence pack to check somewhere the server cannot reach:

```sh
ancre-verify --pack ./acme-hr-screening --key <fingerprint you got elsewhere>
```

Access is split on that same reasoning. `/v1/checkpoints/…` and `/v1/pubkeys`
stay open — signatures over hashes reveal nothing, and an auditor who needs a
credential before checking one verifies fewer of them. The endpoints that
describe how a customer runs their AI need the token.

## Use it

| I want to… | Read |
|---|---|
| Point my application at it | [`docs/integrate.md`](docs/integrate.md) — base-URL swap, SDK examples, what the pins mean, failure modes |
| Run it in front of real traffic | [`docs/deploy.md`](docs/deploy.md) — topology, every environment variable, onboarding a system, access control, key custody, what to alert on |
| Know what is not finished | [`docs/deferred.md`](docs/deferred.md) — **read this before trusting anything here** |
| Understand why it is built this way | [`ancre-prd-and-architecture.md`](ancre-prd-and-architecture.md), [`mvp-plan.md`](mvp-plan.md), [`version-pin-resolver-spec.md`](version-pin-resolver-spec.md) |

The short version of integration:

```python
client = OpenAI(
    base_url="http://ancre-gateway.internal:8080/v1",
    api_key="<your Ancre virtual key>",   # not your OpenAI key
)
```

Nothing else changes. Not the SDK, not the request shape, not the response.
The moment integration needs a code change, the platform engineer who has to
approve it starts asking what else is being bolted onto their inference path.

---

## Test it

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
cargo fmt --all -- --check
```

Both hash back-ends must keep passing — the SHA-256 path is not decoration, it
is what an enterprise crypto policy will demand:

```sh
cargo test -p ancre-canon --no-default-features --features hash-sha256
cargo test -p ancre-chain --features fixtures,ancre-canon/hash-sha256
```

### Against real infrastructure

Each transport suite is a no-op without its `ANCRE_TEST_*` variable, so
`cargo test` stays green on a laptop with no Docker. CI's `transports` job
always sets them.

```sh
docker run -d --name ancre-pg -p 15432:5432 \
  -e POSTGRES_USER=ancre -e POSTGRES_PASSWORD=ancre -e POSTGRES_DB=ancre \
  -v "$PWD/deploy/compose/init/postgres:/docker-entrypoint-initdb.d:ro" \
  postgres:16-alpine
ANCRE_TEST_POSTGRES=postgres://ancre:ancre@127.0.0.1:15432/ancre \
  cargo test -p ancre-control --test postgres
```

`ANCRE_TEST_CLICKHOUSE` and `ANCRE_TEST_NATS` work the same way. Round trips
are hash-critical, which is why they are tested against real servers: a
conversion that changes one hashed byte leaves a chain that verifies inside the
ingester and fails on the auditor's laptop. The first run against a real
ClickHouse found four bugs no unit test could have.

### The gates

Both exit non-zero on a miss, which is what makes them gates rather than demos.

```sh
./deploy/compose/quickstart.sh   # packaging works, and tampering is caught
./deploy/compose/chaos.sh        # three real outages behave as documented
```

```sh
cargo run -p ancre-bench --release --example resolve-gate    # the latency claim
cargo run -p ancre-bench --release --example overhead-gate   # end-to-end overhead
cargo run -p ancre-bench --release --example chaos-gate      # chaining under outage
cargo run -p ancre-bench --release --example config-gate     # snapshot build
cargo run -p ancre-bench --release --example swap-control    # the bench's own control
```

`chaos.sh` wipes its volumes on purpose: `quickstart.sh` ends by corrupting a
row, and a chaos run against what it leaves behind reports a broken chain that
has nothing to do with the outage being tested.

---

## The two claims everything rests on

### 1. In-path version pinning costs under 1ms at p99

```sh
cargo run -p ancre-bench --release --example resolve-gate
cargo run -p ancre-bench --release --example overhead-gate
```

| Measurement | Target | Actual |
|---|---|---|
| `resolve` p50, quiescent | < 2µs | 138ns |
| `resolve` p99, quiescent | < 5µs | 152ns |
| **`resolve` p99, all cores + 10/s reload storm** | < 8µs | **2µs** |
| `resolve` p99, 10k systems / 50k routes | < 5µs | 194ns |
| snapshot build, 10k systems / 50k routes | < 500ms | 110ms |
| p99 added overhead vs null baseline | < 2ms | ~0 |
| p99 added TTFT vs null baseline | < 2ms | ~0 |
| p99 total gateway cost, in-process | < 1ms | 3µs |

*(20-core dev machine, 500k samples per case. Quote the saturated row — the
single-reader numbers are the uncontended floor and they flatter the design.)*

The baseline is the **same binary with pinning compiled out**, not
direct-to-provider. Measuring against the provider would fold network variance
into the number and produce something that falls apart the first time a
prospect's own engineer reproduces it.

A bench that has never failed is not evidence, so there is a control:
`swap-control` runs identical load through `ArcSwap` and through
`RwLock<Arc<T>>`. At full core count the lock is **2.1× worse at the p99** — so
the storm bench really does have the power to catch the mistake it exists to
catch. At 8 readers on a 20-core box the two are within 1.2× of each other,
which is exactly why an under-subscribed bench is a trap.

### 2. A chain verifies byte-identically on an auditor's laptop

```sh
cargo run -p ancre-verify --example gen-fixture > chain.jsonl
cargo run -p ancre-verify -- --chain chain.jsonl
```

```
Chain verified: 50 000 events, seq 1–50000, no violations.
  range root: 709a200ee658750b4785d8c082c0593ff0e5de2804a2757df8958b5ea9d26b10
  chain head: 9eb965f6b8c0a2e59fcb561286c26e2ecd62c6128fd3c648e07f51fd0df885bc
```

Exit codes are 0 clean, 1 violations, **2 cannot verify**. The third is not
decoration: "this build does not implement the rule set these events were
sealed under" is a different answer from "this chain is invalid", and merging
them would be dishonest in the direction that costs the most credibility.

---

## The evidence pack

Four files — the events, the signed checkpoints, the public keys, and a short
manifest — verified in one command with no network:

```sh
docker compose --profile tools run --rm verify --pack /evidence/acme-hr-screening
```

```
Pack: acme/hr-screening
  produced 2026-08-09T07:16:26Z by ancre quickstart, build a77de9bbc5cc

Chain verified: 9 events, seq 1–9, no violations.
  range root: 4c1e…
  chain head: 9d70…

Checkpoints: 1 of 1 verified.
  seq 1–9  root fde7ed1cb22ea8cb  signed by cp-ea3c03a599d0f1ce
Attested range: seq 1–9.

Signatures were checked against this pack's own keys. That proves the pack is
internally consistent and says nothing about who made it — compare these
fingerprints with the ones you were given separately, or re-run with --key:
  cp-ea3c03a599d0f1ce  ea3c03a599d0f1ce…  (active since 2026-08-09T07:15:55Z)
```

Two verdicts, not one, because the properties fail independently: the chain
says the events are internally consistent, the checkpoints say a key signed
them. **Attested range** is the number that matters — the span an auditor can
rely on — and a hole between two signed ranges stays visible as a hole rather
than being averaged into a percentage.

Three choices worth knowing:

- The verifier **recomputes** each event's hash rather than trusting the one in
  the file, so a checkpoint attests the events' *contents*. Feeding it the
  recorded hashes would let a signature attest an attacker's own arithmetic.
- The circularity is printed, not hidden. A pack carries the keys that signed
  it, so verifying against them proves consistency and nothing about
  provenance. `--key <HEX>` pins a fingerprint obtained out of band, and then a
  forged pack fails.
- `network_mode: none` on that container. "Verification needs no network" is a
  claim; a container with no network interface is the version of it a sceptic
  cannot argue with.

---

## How it fits together

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
bench/              criterion, gating CI
crates/ancre-control/ui/   the dashboard. React + Vite + Tailwind, embedded
deploy/compose/     one-command self-host, quickstart, chaos pass
docs/               deploy, integrate, deferred work, the AI Act mapping table
```

Three deployable binaries plus a verifier. PRD §10 caps production at three
containers, and that ceiling is a customer-adoption constraint, not a
preference.

`ancre-canon` is a separate crate on purpose: it is the smallest, most audited,
least-changing thing in the system, and every other crate's correctness reduces
to it. `ancre-verify` may depend on nothing beyond it and `ancre-chain` — no
HTTP client, no database driver, no network capability of any kind. An auditor
should be able to satisfy themselves it cannot phone home by reading one
`Cargo.toml`.

### Design decisions worth knowing

**The trust root.** RFC 8949 canonical CBOR with non-canonical input *rejected
rather than normalised* — otherwise an attacker picks which of two byte
sequences a verifier sees for the same event. RFC 6962 Merkle trees for range
roots, so a subset can be proven without replaying the chain and `[a,b,c]` and
`[a,b,c,c]` cannot share a root. Length-prefixed chain rule, so no value can be
shifted across a field boundary without changing the digest. Every enum has a
frozen `as_str()` and timestamps are integer microseconds: the canonical
encoding depends on this repo's own code, never on how `time` or `uuid` happen
to serialize this year.

**Resolution happens exactly once.** Re-resolving before writing the audit
event is the bug that eats the whole design — a streaming completion can run
for ninety seconds, and a reload in that window would make the event report a
configuration the request never used. `generation` is excluded from
`config_hash`, making it a true content identifier, so "generation bumped,
nothing changed" is visible — which is the question substantial-modification
review actually asks.

**Staleness is measured on the local monotonic clock**, from the last time this
node could *confirm* its configuration with the control plane. Not from the
control plane's `built_at`, because cross-machine skew must never feed a
fail-closed decision; and not from the configuration's own age, because a
config that has not changed in a year is stable, not stale. Getting that second
distinction wrong is what the chaos pass caught.

**The response body forwards before it observes.** Each frame goes downstream
in the same poll it is read, then the bytes already in flight are scanned.
Observation cannot delay a token. `emit()` returns nothing at all, so no caller
on the request path can branch on telemetry success — a full channel drops,
counts, and keeps serving.

**`model_version` comes from the provider's response**, never the request. An
id that does not name specific weights is recorded as `unresolved:<alias>` with
a risk flag, keeping the alias visible: "we asked for gpt-4o and the provider
would not say" is more useful than `unknown`.

**The ingester owns `seq`.** The gateway never assigns one, so a skewed clock
on one node cannot reorder a chain, and a node dying mid-flight cannot create a
gap. A batch the store refuses is rolled back across every chain it touched and
left unacked — acking a subset would leave the store missing events the bus
believes were consumed.

**Checkpoints live in Postgres, not ClickHouse.** Storing the attestation in
the store it attests to hands anyone who can rewrite the events the ability to
re-sign them. Rotation never invalidates an old checkpoint: every key that was
ever active stays exported with its window, and the windows abut exactly.

**A generation is burned, never reused.** If the bus send fails after the
number is allocated, the next publish moves past it. Two configurations sharing
a `config_generation` would make every pin carrying it ambiguous forever, which
is worse than a gap in a counter.

**Unknown is a value, not a null.** `unknown` shows up in a `GROUP BY` and
turns into a line item; NULL just hides. Dropped telemetry becomes a
`telemetry.dropped` event attributed to the chain that lost the events, so a
hole in the record is countable rather than invisible.

---

## What is deliberately not here

No semantic cache, no guardrails, no PII detection, no agent framework. PRD §7
non-goals hold absolutely. Also out of the MVP by plan: provider failover
logic, Azure OpenAI and vLLM adapters, budgets and hard cutoff, a policy engine
(`policy_id` pins to `none`, which is a fact and not a gap), RFC 3161
timestamping, and a trace viewer.

The largest single caveat: **CI has never run.** There is no remote, so every
number above was measured on one developer's machine, and the first push should
be expected to need threshold tuning.

---

## Language

We sell **evidence**, **traceability** and **readiness** — never "compliance"
or "conformity". No harmonised standards exist yet, so no product can deliver
conformity, and overclaiming loses deals at legal review. Say
**tamper-evident**, never tamper-proof (PRD §5).
