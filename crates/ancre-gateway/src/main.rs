//! Ancre gateway.
//!
//! **The request path is sacred.** Nothing that produces evidence may add
//! meaningful latency, and every feature here is measured against the p99
//! budget before it ships. The latency budget is a *sales* requirement: the
//! blocker on every deal is a platform engineer who thinks you are bolting
//! overhead onto their inference path (PRD §4, §6.1).
//!
//! Flow (PRD §9):
//!   ingress → TLS → key hash → **pins resolved once** into `RequestCtx`
//!   → policy → provider call, streamed straight through
//!   → telemetry fork: bounded channel → batcher → NATS. **Never awaited.**

// Scaffold-only. `main` is `todo!()`, so everything below it reads as dead and
// every `pub` in a binary reads as unreachable. Delete both of these as M3
// lands — leaving them past that point hides real dead code.
#![allow(dead_code, unreachable_pub)]

mod auth;
mod proxy;
mod telemetry;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Cold start fails closed. No traffic is served with unknown pins on a
    // high-risk system, so the bind happens *after* the first snapshot lands
    // or after the fail-closed resolver is installed — never before.
    todo!("M3: tracing init, config load, resolver cold(), snapshot subscribe, hyper serve")
}

/// Build semver + git SHA. Stamped into every event as `gateway_version`, so
/// it comes from the build, never from a config file a human can edit.
pub const GATEWAY_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "+",
    // TODO(M1): set via build.rs from `git rev-parse --short HEAD`.
    "unknown"
);
