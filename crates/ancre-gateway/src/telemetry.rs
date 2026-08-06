//! The telemetry fork.
//!
//! Bounded channel → batcher → bus. **Never awaited by the request.** If the
//! bus is down the gateway keeps serving; the channel fills, events drop, the
//! drops are counted, and the count is itself emitted as an event once the bus
//! is back.
//!
//! That last part is the whole design. A silent drop is an invisible hole in
//! the evidence; a counted drop is a countable gap, and countable gaps are
//! invoices (PRD §6.3).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ancre_types::{EmittedEvent, Timestamp};
use tokio::sync::mpsc;

/// Where a drained batch goes. A trait so the batcher can be tested without a
/// broker, and so the NATS client stays out of the hot-path crate's tests.
pub trait EventSink: Send + Sync + 'static {
    fn publish(
        &self,
        batch: Vec<EmittedEvent>,
    ) -> impl std::future::Future<Output = Result<(), SinkError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("bus unavailable: {0}")]
    Unavailable(String),
}

/// The producer half. Cloned into every request task.
#[derive(Debug, Clone)]
pub struct TelemetryFork {
    tx: mpsc::Sender<EmittedEvent>,
    drops: Arc<DropCounter>,
}

impl TelemetryFork {
    #[must_use]
    pub fn new(capacity: usize) -> (Self, Receiver) {
        let (tx, rx) = mpsc::channel(capacity);
        let drops = Arc::new(DropCounter::default());
        (
            Self {
                tx,
                drops: Arc::clone(&drops),
            },
            Receiver { rx, drops },
        )
    }

    /// Non-blocking, infallible, never awaited.
    ///
    /// There is no `Result` on purpose. No caller on the request path is
    /// allowed to branch on telemetry success, because the only correct
    /// response to a full channel is to keep serving — and a `Result` here is
    /// an invitation for someone to `?` it into the request path six months
    /// from now.
    pub fn emit(&self, event: EmittedEvent) {
        if self.tx.try_send(event).is_err() {
            // Full, or the batcher is gone. Either way: count it and return.
            self.drops.record();
        }
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.drops.count()
    }

    /// A handle on the shared counter, so the drop window survives both this
    /// fork and the batcher being dropped. The `telemetry.dropped` event is
    /// emitted from whatever is still alive at recovery.
    #[must_use]
    pub fn drop_counter(&self) -> Arc<DropCounter> {
        Arc::clone(&self.drops)
    }
}

/// The consumer half.
#[derive(Debug)]
pub struct Receiver {
    rx: mpsc::Receiver<EmittedEvent>,
    drops: Arc<DropCounter>,
}

/// Counted, then emitted as a `telemetry.dropped` event once the bus is back.
#[derive(Debug, Default)]
pub struct DropCounter {
    count: AtomicU64,
    /// Microsecond timestamps bounding the window the drops fall in. An
    /// auditor needs to know *when* the hole is, not only that there is one.
    first: AtomicU64,
    last: AtomicU64,
}

impl DropCounter {
    pub fn record(&self) {
        let now = Timestamp::now().as_micros().unsigned_abs();
        self.count.fetch_add(1, Ordering::Relaxed);
        // Only the first drop in a window sets `first`.
        let _ = self
            .first
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
        self.last.store(now, Ordering::Relaxed);
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Take and reset. The returned window becomes one `telemetry.dropped`
    /// event covering exactly the period the drops happened in.
    #[must_use]
    pub fn take(&self) -> Option<DropWindow> {
        let count = self.count.swap(0, Ordering::Relaxed);
        if count == 0 {
            return None;
        }
        let first = self.first.swap(0, Ordering::Relaxed);
        let last = self.last.swap(0, Ordering::Relaxed);
        Some(DropWindow {
            count,
            from: Timestamp::from_micros(i64::try_from(first).unwrap_or(i64::MAX)),
            to: Timestamp::from_micros(i64::try_from(last).unwrap_or(i64::MAX)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropWindow {
    pub count: u64,
    pub from: Timestamp,
    pub to: Timestamp,
}

#[derive(Debug, Clone, Copy)]
pub struct BatchConfig {
    pub max_events: usize,
    pub max_wait: Duration,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_events: 512,
            // Bounds how long an event can sit unpublished on a quiet node.
            // Under load the size trigger fires first and this never matters.
            max_wait: Duration::from_millis(200),
        }
    }
}

/// Drains the channel, batches, publishes. Its own task.
#[derive(Debug)]
pub struct Batcher<S: EventSink> {
    rx: Receiver,
    sink: S,
    config: BatchConfig,
}

impl<S: EventSink> Batcher<S> {
    #[must_use]
    pub fn new(rx: Receiver, sink: S, config: BatchConfig) -> Self {
        Self { rx, sink, config }
    }

    /// Runs until the last `TelemetryFork` is dropped and the channel drains.
    ///
    /// On a clean shutdown it drains; on a crash it does not — which is
    /// exactly why the ingester is idempotent on `event_id` (mvp-plan §5, M4).
    pub async fn run(mut self) -> DrainReport {
        let mut report = DrainReport::default();
        let mut batch: Vec<EmittedEvent> = Vec::with_capacity(self.config.max_events);

        loop {
            let got = tokio::time::timeout(self.config.max_wait, self.rx.rx.recv()).await;

            match got {
                Ok(Some(event)) => {
                    batch.push(event);
                    // Opportunistically take whatever else is queued; one
                    // publish for many events is the difference between the
                    // bus keeping up and not.
                    while batch.len() < self.config.max_events {
                        match self.rx.rx.try_recv() {
                            Ok(e) => batch.push(e),
                            Err(_) => break,
                        }
                    }
                    if batch.len() >= self.config.max_events {
                        self.flush(&mut batch, &mut report).await;
                    }
                }
                // Channel closed: every producer is gone. Drain and stop.
                Ok(None) => {
                    self.flush(&mut batch, &mut report).await;
                    return report;
                }
                // Idle past max_wait: publish whatever is waiting.
                Err(_) => self.flush(&mut batch, &mut report).await,
            }
        }
    }

    async fn flush(&self, batch: &mut Vec<EmittedEvent>, report: &mut DrainReport) {
        if batch.is_empty() {
            return;
        }
        let n = batch.len() as u64;
        if self.sink.publish(std::mem::take(batch)).await.is_ok() {
            report.published += n;
            report.batches += 1;
        } else {
            // The batch is gone. Count it the same way a full channel is
            // counted, so the gap stays visible either way.
            report.failed += n;
            for _ in 0..n {
                self.rx.drops.record();
            }
        }
        batch.clear();
    }

    /// A handle on the shared counter. Taken before `run` consumes the
    /// batcher, since the drop window has to outlive the drain.
    #[must_use]
    pub fn drop_counter(&self) -> Arc<DropCounter> {
        Arc::clone(&self.rx.drops)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub published: u64,
    pub batches: u64,
    pub failed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct Recorder {
        batches: Arc<Mutex<Vec<usize>>>,
        fail: bool,
    }

    impl EventSink for Recorder {
        async fn publish(&self, batch: Vec<EmittedEvent>) -> Result<(), SinkError> {
            if self.fail {
                return Err(SinkError::Unavailable("test".into()));
            }
            self.batches.lock().unwrap().push(batch.len());
            Ok(())
        }
    }

    fn event(seq: u64) -> EmittedEvent {
        ancre_types::fixtures::event(seq, ancre_canon::GENESIS).emitted
    }

    #[tokio::test]
    async fn events_reach_the_sink_in_batches() {
        let (fork, rx) = TelemetryFork::new(1024);
        let batches = Arc::new(Mutex::new(Vec::new()));
        let drainer = Batcher::new(
            rx,
            Recorder {
                batches: Arc::clone(&batches),
                fail: false,
            },
            BatchConfig::default(),
        );

        for i in 1..=100 {
            fork.emit(event(i));
        }
        drop(fork);

        let report = drainer.run().await;
        assert_eq!(report.published, 100);
        assert_eq!(fork_drops(&batches), 100);
    }

    fn fork_drops(batches: &Arc<Mutex<Vec<usize>>>) -> usize {
        batches.lock().unwrap().iter().sum()
    }

    /// The property the whole design rests on: a full channel must not block,
    /// fail, or slow the caller down. It must drop and count.
    #[tokio::test]
    async fn a_full_channel_drops_and_counts_rather_than_blocking() {
        // No batcher running, so nothing drains: the channel fills at 8.
        let (fork, _rx) = TelemetryFork::new(8);

        for i in 1..=1_000 {
            fork.emit(event(i));
        }

        assert_eq!(fork.dropped(), 992, "8 queued, the rest counted as dropped");
    }

    #[tokio::test]
    async fn emitting_after_the_batcher_is_gone_is_counted_not_fatal() {
        let (fork, rx) = TelemetryFork::new(8);
        drop(rx);

        fork.emit(event(1));
        assert_eq!(fork.dropped(), 1);
    }

    #[tokio::test]
    async fn the_drop_window_records_when_the_hole_is() {
        let drops = DropCounter::default();
        assert!(drops.take().is_none(), "no drops, no event");

        drops.record();
        drops.record();

        let w = drops.take().expect("two drops must produce a window");
        assert_eq!(w.count, 2);
        assert!(w.from.as_micros() > 0);
        assert!(w.to >= w.from);

        assert!(drops.take().is_none(), "taking must reset the counter");
    }

    /// A failed publish is a hole in the record exactly like a full channel,
    /// and has to be counted the same way — otherwise a bus outage looks like
    /// clean traffic.
    #[tokio::test]
    async fn a_failed_publish_is_counted_as_a_drop() {
        let (fork, rx) = TelemetryFork::new(1024);
        let drainer = Batcher::new(
            rx,
            Recorder {
                batches: Arc::new(Mutex::new(Vec::new())),
                fail: true,
            },
            BatchConfig::default(),
        );

        let drops = drainer.drop_counter();
        for i in 1..=10 {
            fork.emit(event(i));
        }
        drop(fork);

        let report = drainer.run().await;
        assert_eq!(report.published, 0);
        assert_eq!(report.failed, 10);

        let w = drops.take().expect("failed publishes are drops");
        assert_eq!(w.count, 10);
    }

    #[tokio::test]
    async fn a_quiet_node_still_publishes_within_the_wait_budget() {
        let (fork, rx) = TelemetryFork::new(1024);
        let batches = Arc::new(Mutex::new(Vec::new()));
        let drainer = Batcher::new(
            rx,
            Recorder {
                batches: Arc::clone(&batches),
                fail: false,
            },
            BatchConfig {
                max_events: 512,
                max_wait: Duration::from_millis(20),
            },
        );

        let handle = tokio::spawn(drainer.run());
        fork.emit(event(1));

        // One event, far below the size trigger: the timer must publish it.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(fork_drops(&batches), 1, "the wait budget must flush");

        drop(fork);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_clean_shutdown_drains_everything_queued() {
        let (fork, rx) = TelemetryFork::new(4096);
        let batches = Arc::new(Mutex::new(Vec::new()));
        let drainer = Batcher::new(
            rx,
            Recorder {
                batches: Arc::clone(&batches),
                fail: false,
            },
            BatchConfig::default(),
        );

        for i in 1..=2_000 {
            fork.emit(event(i));
        }
        assert_eq!(fork.dropped(), 0);
        drop(fork);

        let report = drainer.run().await;
        assert_eq!(report.published, 2_000);
    }
}
