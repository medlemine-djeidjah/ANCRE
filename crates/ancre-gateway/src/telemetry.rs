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
        // The rejected event comes back, which is what makes the drop
        // attributable: `telemetry.dropped` has to land in the chain that lost
        // the event, or an auditor reading one system's history sees a clean
        // record of a period during which that system's events were being
        // thrown away.
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(e) | mpsc::error::TrySendError::Closed(e)) => {
                self.drops.record(&e.tenant_id, &e.system_id);
            }
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

impl Receiver {
    /// Everything queued right now, without waiting.
    ///
    /// For a caller that owns the receiver directly — tests, mostly. The
    /// production drain is `Batcher::run`, which batches, publishes, and
    /// counts what it could not.
    pub fn drain_now(&mut self) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            out.push(event);
        }
        out
    }
}

/// How many chains a single node will track drop windows for.
///
/// Past this, drops are recorded against a single overflow window whose chain
/// is `unknown/unknown`. That is a worse answer than a per-chain one and a much
/// better answer than either of the alternatives: an unbounded map that grows
/// with tenant count on the exact code path that runs when the node is already
/// in trouble, or a silent drop that leaves no trace at all.
const MAX_TRACKED_CHAINS: usize = 4096;

/// Counted per chain, then emitted as `telemetry.dropped` once the bus is back.
///
/// Per chain, because a chain is `(tenant_id, system_id)` and an event about a
/// hole has to land in the chain that has the hole. A node-wide counter could
/// only produce one of two dishonest things: an event in one arbitrary chain,
/// or the same node-wide count repeated into every chain as though each had
/// lost that many.
///
/// The map is behind a `Mutex` and that is deliberate. This path runs only when
/// an event has already been thrown away — never on a successful emit — so it
/// is not on the latency budget, and a lock held for the length of a hash
/// lookup is the cheapest correct thing available.
#[derive(Debug, Default)]
pub struct DropCounter {
    total: AtomicU64,
    windows: std::sync::Mutex<ChainSpans>,
}

/// Drop spans by chain — `(tenant_id, system_id)`, the same key everything
/// else in this system calls a chain.
type ChainSpans = std::collections::HashMap<(Arc<str>, Arc<str>), Span>;

/// Microsecond bounds and a count, accumulating until taken. An auditor needs
/// to know *when* the hole is, not only that there is one.
#[derive(Debug, Clone, Copy)]
struct Span {
    count: u64,
    first: i64,
    last: i64,
}

impl DropCounter {
    /// Record one dropped event, against the chain that lost it.
    pub fn record(&self, tenant_id: &Arc<str>, system_id: &Arc<str>) {
        let now = Timestamp::now().as_micros();
        self.total.fetch_add(1, Ordering::Relaxed);

        // A poisoned lock here would mean a panic inside this function, which
        // holds no invariants worth abandoning the count over — the drop
        // already happened, and losing the record of it is the failure this
        // whole type exists to prevent.
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let key = if windows.len() >= MAX_TRACKED_CHAINS
            && !windows.contains_key(&(Arc::clone(tenant_id), Arc::clone(system_id)))
        {
            (
                Arc::from(ancre_types::UNKNOWN),
                Arc::from(ancre_types::UNKNOWN),
            )
        } else {
            (Arc::clone(tenant_id), Arc::clone(system_id))
        };

        windows
            .entry(key)
            .and_modify(|s| {
                s.count += 1;
                s.last = now;
            })
            .or_insert(Span {
                count: 1,
                first: now,
                last: now,
            });
    }

    /// Every drop this node has recorded, across every chain.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Take and reset. Each returned window becomes one `telemetry.dropped`
    /// event covering exactly the period its chain's drops happened in.
    #[must_use]
    pub fn take(&self) -> Vec<DropWindow> {
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::mem::take(&mut *windows)
            .into_iter()
            .map(|((tenant_id, system_id), s)| DropWindow {
                tenant_id,
                system_id,
                count: s.count,
                from: Timestamp::from_micros(s.first),
                to: Timestamp::from_micros(s.last),
            })
            .collect()
    }

    /// Put windows back, unchanged.
    ///
    /// Called when the publish that was carrying them failed. Without this the
    /// recovery attempt itself would destroy the record it was trying to save,
    /// and a bus that comes back one interval too late would leave no evidence
    /// that anything had been lost.
    pub fn restore(&self, taken: Vec<DropWindow>) {
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        for w in taken {
            windows
                .entry((w.tenant_id, w.system_id))
                .and_modify(|s| {
                    s.count += w.count;
                    s.first = s.first.min(w.from.as_micros());
                    s.last = s.last.max(w.to.as_micros());
                })
                .or_insert(Span {
                    count: w.count,
                    first: w.from.as_micros(),
                    last: w.to.as_micros(),
                });
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropWindow {
    pub tenant_id: Arc<str>,
    pub system_id: Arc<str>,
    pub count: u64,
    pub from: Timestamp,
    pub to: Timestamp,
}

impl DropWindow {
    /// The hole, as an event.
    ///
    /// Everything here is chosen so that a reader of the raw table is not
    /// misled about which columns carry a measurement:
    ///
    /// - `occurred_at` is when the hole **starts** and `latency_ms` is how long
    ///   it lasted, so the window is expressed in two columns that already mean
    ///   "a moment" and "a duration". No new column, and no reinterpretation of
    ///   an existing one.
    /// - `tokens_out` carries the number of events lost. This one *is* an
    ///   overload — the frozen schema has no count column and will not grow one
    ///   — and it is the only place in this event where a raw-table reader
    ///   needs the event type to interpret a value. Documented here and in
    ///   `docs/deferred.md` (E11 is the same trade).
    /// - every pin is `unknown`, because the events this accounts for are gone
    ///   and their pins went with them. `has_gap()` is true, which is the
    ///   point: this event is *supposed* to show up when you count what is
    ///   missing.
    #[must_use]
    pub fn into_event(self, node_id: Arc<str>) -> EmittedEvent {
        let duration_ms =
            u32::try_from((self.to.as_micros() - self.from.as_micros()).max(0) / 1_000)
                .unwrap_or(u32::MAX);

        EmittedEvent {
            tenant_id: self.tenant_id,
            event_id: uuid::Uuid::new_v4(),
            trace_id: Arc::from(""),
            attempt_seq: 0,
            occurred_at: self.from,
            node_id,
            event_type: ancre_types::EventType::TelemetryDropped,
            // Not `Ok`. The gateway served those requests correctly and failed
            // to record them, and an evidence system that reports its own
            // failures as successes has no claim on anyone's trust.
            outcome: ancre_types::Outcome::Error,
            pins: ancre_types::Pins::gap(
                Arc::clone(&self.system_id),
                ancre_types::RiskFlag::TelemetryDropped,
            ),
            system_id: self.system_id,
            request_digest: ancre_canon::GENESIS,
            response_digest: ancre_canon::GENESIS,
            metrics: ancre_types::Metrics {
                provider: Arc::from(""),
                http_status: 0,
                latency_ms: duration_ms,
                ttft_ms: 0,
                tokens_in: 0,
                tokens_out: u32::try_from(self.count).unwrap_or(u32::MAX),
                error_code: Arc::from(ancre_types::RiskFlag::TelemetryDropped.as_str()),
            },
        }
    }
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
///
/// It is also the only thing in the process that knows when the bus is back —
/// a publish that succeeds is the definition — so it is where the accumulated
/// drop windows get turned into events and sent.
#[derive(Debug)]
pub struct Batcher<S: EventSink> {
    rx: Receiver,
    sink: S,
    config: BatchConfig,
    node_id: Arc<str>,
}

impl<S: EventSink> Batcher<S> {
    #[must_use]
    pub fn new(rx: Receiver, sink: S, config: BatchConfig) -> Self {
        Self {
            rx,
            sink,
            config,
            node_id: Arc::from(ancre_types::UNKNOWN),
        }
    }

    /// Which node the `telemetry.dropped` events will name.
    ///
    /// Defaulted rather than required, because the tests that care about
    /// batching do not care about identity — but a production batcher without
    /// one emits drop events attributed to `unknown`, and "which node lost
    /// these" is the first question an incident asks.
    #[must_use]
    pub fn with_node_id(mut self, node_id: impl Into<Arc<str>>) -> Self {
        self.node_id = node_id.into();
        self
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
        if !batch.is_empty() {
            let n = batch.len() as u64;
            // Which chains this batch belongs to, kept in case the publish
            // fails. Arc clones off the request path — the alternative is a
            // sink trait that hands the batch back on error, which would put
            // the ownership dance in every implementation instead of here.
            let chains: Vec<(Arc<str>, Arc<str>)> = batch
                .iter()
                .map(|e| (Arc::clone(&e.tenant_id), Arc::clone(&e.system_id)))
                .collect();

            if self.sink.publish(std::mem::take(batch)).await.is_ok() {
                report.published += n;
                report.batches += 1;
            } else {
                // The batch is gone. Count it the same way a full channel is
                // counted, so the gap stays visible either way.
                report.failed += n;
                for (tenant_id, system_id) in &chains {
                    self.rx.drops.record(tenant_id, system_id);
                }
            }
            batch.clear();
        }

        self.recover(report).await;
    }

    /// Turn whatever has been dropped into events, and publish them.
    ///
    /// Attempted on every flush, including the idle ones — a node that stops
    /// receiving traffic during an outage must still report the hole, and
    /// waiting for the next real request to carry it would mean the quietest
    /// systems get the worst evidence.
    ///
    /// If this publish fails too, the windows go back exactly as they were.
    /// The recovery attempt must never be the thing that destroys the record
    /// it exists to save.
    async fn recover(&self, report: &mut DrainReport) {
        let windows = self.rx.drops.take();
        if windows.is_empty() {
            return;
        }

        let lost: u64 = windows.iter().map(|w| w.count).sum();
        let events: Vec<EmittedEvent> = windows
            .iter()
            .cloned()
            .map(|w| w.into_event(Arc::clone(&self.node_id)))
            .collect();

        if self.sink.publish(events).await.is_ok() {
            report.drop_events += windows.len() as u64;
            tracing::warn!(
                events_lost = lost,
                chains = windows.len(),
                "telemetry.dropped emitted: the chain is complete, the record is not"
            );
        } else {
            self.rx.drops.restore(windows);
        }
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
    /// `telemetry.dropped` events successfully published. One per chain per
    /// recovery, not one per lost event — the events themselves are gone.
    pub drop_events: u64,
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

    /// Keeps what it published, and can be made to fail and recover mid-test —
    /// which is the only way to exercise the bus-came-back path.
    #[derive(Debug, Default)]
    struct FlakySink {
        fail: std::sync::atomic::AtomicBool,
        published: Mutex<Vec<EmittedEvent>>,
    }

    impl EventSink for Arc<FlakySink> {
        async fn publish(&self, batch: Vec<EmittedEvent>) -> Result<(), SinkError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(SinkError::Unavailable("test".into()));
            }
            self.published.lock().unwrap().extend(batch);
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
        assert!(drops.take().is_empty(), "no drops, no event");

        let (tenant, system) = (Arc::from("acme"), Arc::from("hr-screening"));
        drops.record(&tenant, &system);
        drops.record(&tenant, &system);

        let taken = drops.take();
        assert_eq!(taken.len(), 1, "one chain, one window");
        let w = &taken[0];
        assert_eq!(w.count, 2);
        assert!(w.from.as_micros() > 0);
        assert!(w.to >= w.from);

        assert!(drops.take().is_empty(), "taking must reset the counter");
    }

    /// The property that makes the event useful. A node-wide count could only
    /// be reported into one arbitrary chain, or into every chain as though each
    /// had lost all of them — both are wrong in a way an auditor would act on.
    #[tokio::test]
    async fn drops_are_attributed_to_the_chain_that_lost_them() {
        let drops = DropCounter::default();
        let hr: Arc<str> = Arc::from("hr-screening");
        let credit: Arc<str> = Arc::from("credit-scoring");
        let acme: Arc<str> = Arc::from("acme");

        drops.record(&acme, &hr);
        drops.record(&acme, &hr);
        drops.record(&acme, &credit);

        let mut taken = drops.take();
        taken.sort_by(|a, b| a.system_id.cmp(&b.system_id));

        assert_eq!(taken.len(), 2);
        assert_eq!(
            (&*taken[0].system_id, taken[0].count),
            ("credit-scoring", 1)
        );
        assert_eq!((&*taken[1].system_id, taken[1].count), ("hr-screening", 2));
        assert_eq!(
            drops.count(),
            3,
            "the node-wide total still counts them all"
        );
    }

    /// A failed recovery publish must not be the thing that destroys the
    /// record. Restoring merges rather than replaces, so drops that happened
    /// while the recovery was in flight are not lost either.
    #[tokio::test]
    async fn restoring_a_failed_recovery_keeps_every_drop() {
        let drops = DropCounter::default();
        let (tenant, system) = (Arc::from("acme"), Arc::from("hr-screening"));
        drops.record(&tenant, &system);
        drops.record(&tenant, &system);

        let taken = drops.take();
        // A drop landing between the take and the restore: the window is
        // reopened by a concurrent request, and the merge has to keep both.
        drops.record(&tenant, &system);
        drops.restore(taken);

        let after = drops.take();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].count, 3);
    }

    /// The window becomes an event that says what it does not know.
    #[tokio::test]
    async fn a_drop_window_becomes_an_event_with_every_pin_unknown() {
        let drops = DropCounter::default();
        let (tenant, system) = (Arc::from("acme"), Arc::from("hr-screening"));
        drops.record(&tenant, &system);

        let event = drops.take().remove(0).into_event(Arc::from("gw-1"));

        assert_eq!(event.event_type, ancre_types::EventType::TelemetryDropped);
        assert_eq!(event.outcome, ancre_types::Outcome::Error);
        assert_eq!(&*event.tenant_id, "acme");
        assert_eq!(&*event.system_id, "hr-screening");
        assert_eq!(&*event.node_id, "gw-1");
        assert_eq!(event.metrics.tokens_out, 1, "the number of events lost");
        assert!(
            event.pins.has_gap(),
            "a telemetry.dropped event must count as a gap; it is the gap"
        );
        assert_eq!(
            event.pins.risk_flags.as_slice(),
            [ancre_types::RiskFlag::TelemetryDropped]
        );
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
        assert_eq!(
            report.drop_events, 0,
            "the recovery publish fails too, on a sink that fails everything"
        );

        let taken = drops.take();
        assert_eq!(taken.len(), 1, "ten events from one chain, one window");
        assert_eq!(
            taken[0].count, 10,
            "the record survives the failed recovery attempt"
        );
    }

    /// The whole point of counting drops: once the bus is back, the hole shows
    /// up in the chain that has it, without anyone having to look at a metric.
    #[tokio::test]
    async fn the_hole_is_published_as_an_event_once_the_bus_is_back() {
        let sink = Arc::new(FlakySink::default());
        let (fork, rx) = TelemetryFork::new(4);
        let drainer = Batcher::new(rx, Arc::clone(&sink), BatchConfig::default())
            .with_node_id("gw-under-test");

        // Fill the channel and then some, with nothing draining it yet.
        sink.fail.store(true, Ordering::SeqCst);
        for i in 1..=20 {
            fork.emit(event(i));
        }
        assert!(fork.dropped() >= 16, "a channel of 4 cannot hold 20");

        let handle = tokio::spawn(drainer.run());
        tokio::time::sleep(Duration::from_millis(50)).await;
        sink.fail.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;

        drop(fork);
        let report = handle.await.unwrap();
        assert!(report.drop_events > 0, "the drops must reach the bus");

        let published = sink.published.lock().unwrap();
        let dropped: Vec<_> = published
            .iter()
            .filter(|e| e.event_type == ancre_types::EventType::TelemetryDropped)
            .collect();
        assert_eq!(dropped.len(), 1, "one chain, one telemetry.dropped event");
        assert!(
            dropped[0].metrics.tokens_out >= 16,
            "the event has to carry how many were lost, not merely that some were"
        );
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
