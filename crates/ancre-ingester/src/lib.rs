//! Ancre ingester. Bus → seq allocation → chain → store.
//!
//! **The ingester owns `seq` and the chain, not the gateway** (PRD §9). The
//! gateway emits unordered events with monotonic local timestamps and no
//! ordering claim. This keeps coordination entirely off the hot path, and it
//! means a gateway node dying mid-flight cannot leave a gap in a chain — there
//! is no chain position for it to die in the middle of.

pub mod chain_writer;
pub mod pipeline;
pub mod sink;
