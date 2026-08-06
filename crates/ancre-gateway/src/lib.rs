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
//!
//! A library as well as a binary, so the request pipeline can be tested
//! end-to-end against a fake upstream — the pin behaviour is what matters and
//! it should not need a network to check.

pub mod auth;
pub mod config_feed;
pub mod proxy;
pub mod serve;
pub mod tap;
pub mod telemetry;
pub mod upstream;

/// Build semver + git SHA. Stamped into every event as `gateway_version`, so
/// it comes from the build, never from a config file a human can edit.
pub const GATEWAY_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "+",
    // TODO(E4): set via build.rs from `git rev-parse --short HEAD`.
    "unknown"
);
