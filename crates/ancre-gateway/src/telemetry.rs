//! The telemetry fork.
//!
//! Bounded channel → batcher → NATS JetStream. **Never awaited by the
//! request.** If the bus is down the gateway keeps serving; the channel fills,
//! events drop, `dropped_events` increments, and the drop is itself emitted as
//! an event at recovery.
//!
//! That last part is the whole design. A silent drop is an invisible hole in
//! the evidence; a counted drop is a countable gap, and countable gaps are
//! invoices (PRD §6.3).

use ancre_types::EmittedEvent;

#[derive(Debug)]
pub struct TelemetryFork {
    _tx: (),
}

impl TelemetryFork {
    #[must_use]
    pub fn new(_capacity: usize) -> (Self, Batcher) {
        todo!("M3: tokio::sync::mpsc bounded channel")
    }

    /// Non-blocking, infallible, never awaited.
    ///
    /// Returns immediately whether or not the event was accepted. There is no
    /// `Result` on purpose: no caller on the hot path is allowed to branch on
    /// telemetry success, because the only correct response is to keep serving.
    pub fn emit(&self, _event: EmittedEvent) {
        todo!("M3: try_send; on Err(Full) bump dropped_events and return")
    }
}

/// Drains the channel, batches, publishes to NATS. Its own task.
#[derive(Debug)]
pub struct Batcher {
    _rx: (),
}

impl Batcher {
    /// Runs until shutdown. On a clean shutdown it drains — on a crash it does
    /// not, which is exactly why the ingester is idempotent on `event_id`.
    pub async fn run(self) {
        todo!("M3: batch by size or interval, publish, ack")
    }
}

/// Counted, then emitted as a `telemetry.dropped` event once the bus is back.
#[derive(Debug, Default)]
pub struct DropCounter {
    _n: (),
}

impl DropCounter {
    /// Take and reset. The returned count becomes one `telemetry.dropped`
    /// event carrying the window it covers — an auditor needs to know *when*
    /// the hole is, not only that there is one.
    pub fn take(&self) -> u64 {
        todo!("M3: atomic swap")
    }
}
