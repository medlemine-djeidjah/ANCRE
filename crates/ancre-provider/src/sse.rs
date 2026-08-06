//! SSE passthrough.
//!
//! **No buffering, anywhere.** The first byte is forwarded before the second is
//! read. The pin-capture pass observes frames as they fly past; it never gates
//! them. If capturing metadata ever delays a token, the design is wrong —
//! TTFT delta budget is < 2ms (PRD §8) and a platform engineer will measure it
//! before they will read the pitch.

/// A borrowed view over one SSE frame. Borrowed on purpose: the frame is on
/// its way to the client and copying it to inspect it is exactly the cost this
/// crate exists to avoid.
#[derive(Debug)]
pub struct Frame<'a> {
    pub event: Option<&'a str>,
    pub data: &'a [u8],
}

/// Incremental SSE splitter over a byte stream.
///
/// Frames can split across TCP reads mid-field, so this holds a small carry
/// buffer for the tail of an incomplete frame — bounded, and a frame that
/// exceeds the bound is a protocol error, not a reason to grow.
#[derive(Debug, Default)]
pub struct FrameScanner {
    _carry: (),
}

impl FrameScanner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk, yield complete frames. The caller forwards the chunk
    /// downstream **first**, then calls this.
    pub fn feed<'a>(&'a mut self, _chunk: &'a [u8]) -> impl Iterator<Item = Frame<'a>> {
        std::iter::empty::<Frame<'a>>()
    }
}

// M3 tests:
// - a frame split across three reads is reassembled
// - `[DONE]` terminates cleanly
// - a client disconnect mid-stream still emits an audit event with
//   `Outcome::Interrupted` and the tokens counted so far
