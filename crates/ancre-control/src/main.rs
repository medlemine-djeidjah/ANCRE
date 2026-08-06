//! Ancre control plane.
//!
//! Registry CRUD, key management, snapshot build and publication, checkpoint
//! signing. Off the request path entirely — the gateway serves through a total
//! control-plane outage up to the staleness budget (PRD §8).

// Scaffold-only; remove as M4 lands. See the note in ancre-gateway's main.rs.
#![allow(dead_code, unreachable_pub)]

mod api;
mod checkpointer;
mod snapshot;

use std::process::ExitCode;

fn main() -> ExitCode {
    todo!("M4: tracing init, sqlx pool, NATS, axum serve, checkpointer task")
}
