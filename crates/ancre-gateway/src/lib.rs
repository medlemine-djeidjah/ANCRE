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
pub mod bus;
pub mod config_feed;
pub mod control;
pub mod proxy;
pub mod serve;
pub mod tap;
pub mod telemetry;
pub mod upstream;

/// Build semver + git SHA, stamped by `build.rs` at compile time so it comes
/// from the build and never from a config file a human can edit.
///
/// Note where this is *not* used: the `gateway_version` an event carries comes
/// from the installed snapshot, because every pin in an event has to be a
/// value the control plane hashed into `config_hash`. So this constant is what
/// this binary believes about itself, and the snapshot's is what the fleet was
/// told — `main` compares them at startup and says so when they differ. A
/// mismatch is a half-finished deploy, and the cost of not noticing is a pin
/// that names the wrong build.
pub const GATEWAY_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("ANCRE_BUILD_SHA"));
