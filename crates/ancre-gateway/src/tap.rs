//! The observing body.
//!
//! Wraps the upstream response body and forwards every frame downstream
//! **before** looking at it. The pins are read from bytes already on their way
//! to the client, so observation cannot delay a token — that is the difference
//! between a gateway a platform engineer will install and one they will not.
//!
//! When the stream ends (cleanly, in error, or because the client hung up) the
//! tap emits exactly one audit event.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use ancre_provider::{Provider, ServedBy, StreamPins, sse};
use ancre_types::{EmittedEvent, EventType, Metrics, Outcome, RequestCtx, Timestamp};
use http_body::{Body, Frame};

use crate::telemetry::TelemetryFork;

/// Everything needed to write the audit event once the body finishes.
pub struct Tap {
    pub ctx: RequestCtx,
    pub tenant_id: std::sync::Arc<str>,
    pub node_id: std::sync::Arc<str>,
    pub provider: &'static dyn Provider,
    pub telemetry: TelemetryFork,
    pub http_status: u16,
    pub request_digest: ancre_canon::Hash32,
    pub streaming: bool,
}

/// A response body that forwards first and observes second.
pub struct TappedBody<B> {
    inner: B,
    tap: Option<Tap>,
    scanner: sse::FrameScanner,
    pins: StreamPins,
    /// Non-streaming responses are assembled here so the model id can be read
    /// from the whole JSON document. Streaming responses never touch it.
    buffered: Vec<u8>,
    response_hasher: ancre_canon::Hasher,
    started: Instant,
    ttft: Option<Instant>,
    outcome: Outcome,
}

impl<B> TappedBody<B> {
    pub fn new(inner: B, tap: Tap) -> Self {
        let started = tap.ctx.started;
        Self {
            inner,
            tap: Some(tap),
            scanner: sse::FrameScanner::new(),
            pins: StreamPins::default(),
            buffered: Vec::new(),
            response_hasher: ancre_canon::Hasher::new(),
            started,
            ttft: None,
            outcome: Outcome::Ok,
        }
    }

    /// Observe a chunk that has *already* been handed downstream.
    fn observe(&mut self, chunk: &[u8]) {
        if self.ttft.is_none() {
            self.ttft = Some(Instant::now());
        }
        self.response_hasher.update(chunk);

        let Some(tap) = self.tap.as_ref() else { return };

        if !tap.streaming {
            // Bounded by the upstream's own response size; a non-streaming
            // completion is not an unbounded stream.
            self.buffered.extend_from_slice(chunk);
            return;
        }

        // Once the model id is known there is nothing left in the frames that
        // changes the pin, only the trailing usage numbers — but those still
        // have to be read, so the scan continues either way. The early-exit
        // temptation here is a bug: skipping frames loses the token counts.
        let provider = tap.provider;
        let pins = &mut self.pins;
        // A frame that overflows the scanner's bound is a protocol error on
        // the upstream's side. The client already has the bytes; the pins for
        // this request are simply incomplete, which the event will show.
        let _ = self.scanner.feed(chunk, |frame| {
            if let Some(s) = provider.served_by_streaming(&frame) {
                pins.absorb(s);
            }
        });
    }

    /// Emit exactly one audit event. Idempotent: the tap is taken.
    fn finish(&mut self) {
        let Some(tap) = self.tap.take() else { return };

        let served = if tap.streaming {
            let mut pins = std::mem::take(&mut self.pins);
            let provider = tap.provider;
            self.scanner.finish(|frame| {
                if let Some(s) = provider.served_by_streaming(&frame) {
                    pins.absorb(s);
                }
            });
            pins.finish()
        } else {
            // A body that will not parse is `unknown` plus a flag, never a
            // guess taken from the request. Recording what we asked for as if
            // it were what ran is the exact failure this product exists to
            // prevent.
            tap.provider
                .served_by(&self.buffered)
                .unwrap_or_else(|_| ServedBy::unknown())
        };

        let mut pins = tap.ctx.pins;
        // The provider's answer overrides the route's declared version. The
        // route says what we asked for; only the response says what ran.
        pins.model_version = served.model_version;
        if let Some(flag) = served.flag {
            if !pins.risk_flags.contains(&flag) {
                pins.risk_flags.push(flag);
            }
        }

        let now = Instant::now();
        let event = EmittedEvent {
            tenant_id: tap.tenant_id,
            system_id: pins.system_id.clone(),
            event_id: uuid::Uuid::new_v4(),
            trace_id: tap.ctx.trace_id,
            attempt_seq: tap.ctx.attempt_seq,
            occurred_at: Timestamp::now(),
            node_id: tap.node_id,
            event_type: EventType::LlmRequest,
            outcome: self.outcome,
            pins,
            request_digest: tap.request_digest,
            response_digest: self.response_hasher.finalize(),
            metrics: Metrics {
                provider: std::sync::Arc::from(tap.provider.kind().as_str()),
                http_status: tap.http_status,
                latency_ms: millis_since(self.started, now),
                ttft_ms: self.ttft.map_or(0, |t| millis_since(self.started, t)),
                tokens_in: served.tokens_in,
                tokens_out: served.tokens_out,
                // Empty on success. The upstream's own error body is not
                // parsed for a code here — that is a provider-specific shape
                // and guessing at it would put a fabricated string in the
                // evidence. The status is recorded and is the fact we have.
                error_code: if tap.http_status >= 400 {
                    std::sync::Arc::from(format!("http_{}", tap.http_status))
                } else {
                    std::sync::Arc::from("")
                },
            },
        };

        // The only place the request path touches telemetry, and it does not
        // await: a full channel drops and counts (PRD §9).
        tap.telemetry.emit(event);
    }
}

// `dyn Provider` and the upstream body have no meaningful `Debug`; these exist
// so the types can appear inside other debug output without forcing a bound.
impl std::fmt::Debug for Tap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tap")
            .field("trace_id", &self.ctx.trace_id)
            .field("http_status", &self.http_status)
            .field("streaming", &self.streaming)
            .finish_non_exhaustive()
    }
}

impl<B> std::fmt::Debug for TappedBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TappedBody")
            .field("finished", &self.tap.is_none())
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

fn millis_since(from: Instant, to: Instant) -> u32 {
    u32::try_from(to.saturating_duration_since(from).as_millis()).unwrap_or(u32::MAX)
}

impl<B> Body for TappedBody<B>
where
    B: Body<Data = bytes::Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    type Data = bytes::Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let me = &mut *self;
        match Pin::new(&mut me.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                // Observation happens on the way past. The frame is returned
                // to the caller in the same poll, so nothing is held back.
                if let Some(data) = frame.data_ref() {
                    me.observe(data);
                }
                // End of stream can arrive as a flag on the last frame rather
                // than as a following `None`, and hyper takes that shortcut:
                // once a Content-Length body has yielded its last byte, the
                // server writes the response and never polls again. Without
                // this, every non-streaming request would be recorded as
                // `interrupted` — the body would only ever `finish` from
                // `Drop`, which is the "client hung up" path. A 200 logged as
                // an interruption is evidence that contradicts itself.
                if me.inner.is_end_stream() {
                    me.finish();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                me.outcome = Outcome::Error;
                me.finish();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                me.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// A client that hangs up mid-stream still produces an event.
///
/// Dropping the body is how that arrives: hyper drops it when the connection
/// goes away. Without this, a cancelled 90-second completion would leave no
/// record at all — and "the client disconnected" is exactly the kind of thing
/// an oversight review asks about.
impl<B> Drop for TappedBody<B> {
    fn drop(&mut self) {
        if self.tap.is_some() {
            self.outcome = Outcome::Interrupted;
            self.finish();
        }
    }
}
