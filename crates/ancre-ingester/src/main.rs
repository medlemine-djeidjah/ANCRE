//! Ancre ingester. NATS JetStream → seq allocation → chain → ClickHouse.
//!
//! **The ingester owns `seq` and the chain, not the gateway** (PRD §9). The
//! gateway emits unordered events with monotonic local timestamps and no
//! ordering claim. This keeps coordination entirely off the hot path, and it
//! means a gateway node dying mid-flight cannot leave a gap in a chain — there
//! is no chain position for it to die in the middle of.

// Scaffold-only; remove as M4 lands. See the note in ancre-gateway's main.rs.
#![allow(dead_code, unreachable_pub)]

mod chain_writer;
mod sink;

use std::process::ExitCode;

fn main() -> ExitCode {
    todo!("M4: tracing init, NATS consumer, ClickHouse client, run loop")
}
