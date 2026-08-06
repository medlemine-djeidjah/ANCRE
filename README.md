# Ancre

An LLM gateway that produces regulator-grade evidence as a side effect of
serving traffic.

**Status: architectural scaffold.** The workspace compiles, `cargo fmt` and
`cargo clippy` are clean, and every function body is a `todo!()` tagged with
the milestone that fills it. Nothing runs yet.

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

## Where to start

M1, in order (mvp-plan §5):

1. `crates/ancre-canon` — deterministic encoding. Write **resolver spec test
   6 first**: two independently-built snapshots of the same logical config
   produce byte-identical `config_hash`. Non-deterministic canonical encoding
   is invisible until an auditor cannot verify a chain, and by then every chain
   ever written is suspect.
2. `crates/ancre-chain` — event hash and verification.
3. `crates/ancre-verify` — verifies a JSONL fixture, exit 0/1.

M1 is done when a deliberately corrupted event in a 100k-event fixture is
caught by the verifier and the report names its `seq`.

## Build

```sh
cargo check --workspace          # compiles; bodies are todo!()
cargo clippy --workspace --all-targets
cargo test --workspace           # no tests yet — that is M1's job
```

Both hash back-ends must keep building:

```sh
cargo check -p ancre-canon --no-default-features --features hash-sha256
```

## Language

We sell **evidence**, **traceability** and **readiness** — never "compliance"
or "conformity". No harmonised standards exist yet, so no product can deliver
conformity, and overclaiming loses deals at legal review. Say
**tamper-evident**, never tamper-proof (PRD §5).
