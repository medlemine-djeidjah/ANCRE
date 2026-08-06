# Ancre

An LLM gateway that produces regulator-grade evidence as a side effect of
serving traffic.

**Status: M1 (trust root) complete; M2–M5 scaffolded.**

The deterministic encoder, the hash chain and the offline verifier are real
and tested — 57 tests, both hash back-ends. Everything downstream of them is a
`todo!()` tagged with the milestone that fills it.

## Documents

| File | What it is |
|---|---|
| `ancre-prd-and-architecture.md` | PRD, architecture, tech stack, GTM, kill criteria |
| `mvp-plan.md` | Plan to 31 Oct 2026, milestones M1–M5, frozen event schema |
| `version-pin-resolver-spec.md` | The resolver, in detail. The hard part |
| `docs/mapping-table.md` | AI Act requirement → schema column. Stub; also the lead magnet |

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

## Next: M2, the resolver

`crates/ancre-resolver`, per `version-pin-resolver-spec.md`. **Run the
reload-storm bench on day one** — it is the one that catches an `RwLock` where
`ArcSwap` belongs, and it is cheap before there is code worth defending.

Gate: if `resolve` p99 cannot get under 5µs, stop and re-plan. The latency
claim descends from that number.

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
