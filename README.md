# Ancre

An LLM gateway that produces regulator-grade evidence as a side effect of
serving traffic.

**Status: M1–M3 complete; M4–M5 scaffolded.**

The deterministic encoder, the hash chain, the offline verifier, the in-path
pin resolver and the request pipeline are real and tested — 176 tests, both
hash back-ends, both latency gates passing. What remains is persistence: the
ingester, the control plane, and checkpoint signing.

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
deploy/compose/     one-command self-host
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

## Next: M4, persistence

`ancre-ingester` and `ancre-control` — seq allocation, chain computation,
ClickHouse batch insert, snapshot publication, ed25519 checkpoint signing.

Done when ClickHouse can be killed for ten minutes under load: the gateway is
unaffected, the ingester catches up, and the chain verifies with no gap.

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
