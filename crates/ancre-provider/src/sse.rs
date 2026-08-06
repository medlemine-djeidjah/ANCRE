//! SSE frame scanning.
//!
//! **No buffering, anywhere.** The first byte is forwarded before the second
//! is read. This scanner is a side observer: the gateway forwards the chunk
//! downstream *first*, then hands the same bytes here to pick the pins out.
//! It never gates a byte, and if capturing metadata ever delays a token the
//! design is wrong — the TTFT delta budget is < 2ms (PRD §8), and a platform
//! engineer will measure it before they read the pitch.
//!
//! Zero-copy on the common path: complete frames inside a chunk are yielded
//! as borrows of that chunk. Only a frame split across a read boundary is
//! copied, into a bounded carry buffer.

/// A borrowed view over one SSE frame.
///
/// Borrowed on purpose: the frame is on its way to the client, and copying it
/// to inspect it is exactly the cost this module exists to avoid.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub event: Option<&'a str>,
    pub data: &'a [u8],
}

impl Frame<'_> {
    /// OpenAI's stream terminator.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.data == b"[DONE]"
    }

    #[must_use]
    pub fn data_str(&self) -> Option<&str> {
        std::str::from_utf8(self.data).ok()
    }
}

/// A frame larger than this is a protocol error, not a reason to grow.
///
/// Without a ceiling, a hostile or broken upstream that never sends a frame
/// separator would grow this buffer until the gateway is OOM-killed — taking
/// down every tenant on the node, not just the one stream.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SseError {
    #[error("SSE frame exceeded {MAX_FRAME_BYTES} bytes without a separator")]
    FrameTooLarge,
}

/// Incremental SSE splitter over a byte stream.
#[derive(Debug, Default)]
pub struct FrameScanner {
    /// Tail of an incomplete frame, carried to the next chunk. Empty on the
    /// common path, which is what keeps the scan zero-copy.
    carry: Vec<u8>,
}

impl FrameScanner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk and yield every complete frame in it.
    ///
    /// The caller forwards the chunk downstream **first**, then calls this.
    ///
    /// A callback rather than an iterator: frames may borrow either the carry
    /// buffer or the chunk, and an iterator holding `&mut self` and `&chunk`
    /// at once does not typecheck without copying every byte. The callback
    /// keeps both borrows short and the fast path allocation-free.
    pub fn feed<F>(&mut self, chunk: &[u8], mut on_frame: F) -> Result<(), SseError>
    where
        F: FnMut(Frame<'_>),
    {
        // Fast path: nothing carried, so complete frames are borrows of the
        // chunk itself and only the trailing partial is copied.
        if self.carry.is_empty() {
            let mut rest = chunk;
            while let Some((at, sep_len)) = find_separator(rest) {
                if let Some(frame) = parse(&rest[..at]) {
                    on_frame(frame);
                }
                rest = &rest[at + sep_len..];
            }
            return self.push_carry(rest);
        }

        // Slow path: a frame straddles the read boundary.
        //
        // The separator itself can straddle it too — `"…one\n"` then
        // `"\ndata: two…"` — so the join has to be scanned as one buffer.
        // Searching only within the new chunk misses that split and silently
        // merges two frames into one, which loses whichever pin the second
        // one carried.
        self.carry.extend_from_slice(chunk);

        let mut consumed = 0;
        while let Some((at, sep_len)) = find_separator(&self.carry[consumed..]) {
            if let Some(frame) = parse(&self.carry[consumed..consumed + at]) {
                on_frame(frame);
            }
            consumed += at + sep_len;
        }
        self.carry.drain(..consumed);

        // Bound checked after draining: a chunk carrying many complete frames
        // is normal and must not trip the limit. Only an unterminated *frame*
        // is a protocol error.
        if self.carry.len() > MAX_FRAME_BYTES {
            self.carry.clear();
            return Err(SseError::FrameTooLarge);
        }
        Ok(())
    }

    /// Flush at end of stream.
    ///
    /// An upstream that closes without a trailing separator still leaves one
    /// real frame in the buffer — and for a short response that frame can be
    /// the only one, carrying the usage numbers the audit event needs.
    pub fn finish<F>(&mut self, mut on_frame: F)
    where
        F: FnMut(Frame<'_>),
    {
        let carried = std::mem::take(&mut self.carry);
        if let Some(frame) = parse(&carried) {
            on_frame(frame);
        }
    }

    fn push_carry(&mut self, bytes: &[u8]) -> Result<(), SseError> {
        if self.carry.len() + bytes.len() > MAX_FRAME_BYTES {
            self.carry.clear();
            return Err(SseError::FrameTooLarge);
        }
        self.carry.extend_from_slice(bytes);
        Ok(())
    }

    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.carry.len()
    }
}

/// Offset and length of the next frame separator: `\n\n` or `\r\n\r\n`.
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            b'\n' if buf.get(i + 1) == Some(&b'\n') => return Some((i, 2)),
            b'\r' if buf.get(i + 1..i + 4) == Some(b"\n\r\n") => return Some((i, 4)),
            _ => i += 1,
        }
    }
    None
}

/// Parse one frame body. Returns `None` for a frame with no `data:` line —
/// comments and keep-alives carry no pins and are not worth surfacing.
fn parse(body: &[u8]) -> Option<Frame<'_>> {
    let mut event = None;
    let mut data = None;

    for line in body.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b":") {
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (field, mut value) = line.split_at(colon);
        value = &value[1..];
        // One optional leading space, per the SSE spec.
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }

        match field {
            b"event" => event = std::str::from_utf8(value).ok(),
            // Multi-line `data:` is legal in SSE, but no LLM provider emits it
            // and joining fragments would mean allocating. First line wins;
            // revisit if a provider ever needs it.
            b"data" if data.is_none() => data = Some(value),
            _ => {}
        }
    }

    data.map(|data| Frame { event, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(scanner: &mut FrameScanner, chunk: &[u8]) -> Vec<(Option<String>, String)> {
        let mut out = Vec::new();
        scanner
            .feed(chunk, |f| {
                out.push((
                    f.event.map(ToString::to_string),
                    String::from_utf8_lossy(f.data).into_owned(),
                ));
            })
            .unwrap();
        out
    }

    #[test]
    fn splits_complete_frames_in_one_chunk() {
        let mut s = FrameScanner::new();
        let got = collect(&mut s, b"data: one\n\ndata: two\n\n");
        assert_eq!(
            got,
            vec![(None, "one".to_string()), (None, "two".to_string())]
        );
        assert_eq!(s.pending_bytes(), 0);
    }

    #[test]
    fn a_frame_split_across_three_reads_is_reassembled() {
        let mut s = FrameScanner::new();
        assert!(collect(&mut s, b"data: {\"mod").is_empty());
        assert!(collect(&mut s, b"el\":\"gpt-4o").is_empty());
        let got = collect(&mut s, b"-2024-08-06\"}\n\n");
        assert_eq!(got, vec![(None, r#"{"model":"gpt-4o-2024-08-06"}"#.into())]);
    }

    #[test]
    fn a_separator_split_across_reads_still_terminates_the_frame() {
        let mut s = FrameScanner::new();
        assert!(collect(&mut s, b"data: one\n").is_empty());
        let got = collect(&mut s, b"\ndata: two\n\n");
        assert_eq!(
            got,
            vec![(None, "one".to_string()), (None, "two".to_string())]
        );
    }

    #[test]
    fn handles_crlf_separators() {
        let mut s = FrameScanner::new();
        let got = collect(&mut s, b"data: one\r\n\r\ndata: two\r\n\r\n");
        assert_eq!(
            got,
            vec![(None, "one".to_string()), (None, "two".to_string())]
        );
    }

    #[test]
    fn reads_the_event_field() {
        let mut s = FrameScanner::new();
        let got = collect(&mut s, b"event: message_start\ndata: {}\n\n");
        assert_eq!(got, vec![(Some("message_start".into()), "{}".into())]);
    }

    #[test]
    fn skips_comments_and_keepalives() {
        let mut s = FrameScanner::new();
        let got = collect(&mut s, b": ping\n\ndata: real\n\n");
        assert_eq!(got, vec![(None, "real".to_string())]);
    }

    #[test]
    fn recognises_the_done_terminator() {
        let mut s = FrameScanner::new();
        let mut done = false;
        s.feed(b"data: [DONE]\n\n", |f| done = f.is_done()).unwrap();
        assert!(done);
    }

    #[test]
    fn a_trailing_frame_without_a_separator_is_flushed_at_end_of_stream() {
        // A short response can close without a trailing blank line, and that
        // last frame may be the only one carrying usage.
        let mut s = FrameScanner::new();
        assert!(collect(&mut s, b"data: last").is_empty());

        let mut out = Vec::new();
        s.finish(|f| out.push(String::from_utf8_lossy(f.data).into_owned()));
        assert_eq!(out, vec!["last".to_string()]);
    }

    #[test]
    fn a_byte_at_a_time_stream_yields_the_same_frames() {
        let stream = b"event: a\ndata: one\n\nevent: b\ndata: two\n\n";
        let mut s = FrameScanner::new();
        let mut out = Vec::new();
        for i in 0..stream.len() {
            s.feed(&stream[i..=i], |f| {
                out.push(String::from_utf8_lossy(f.data).into_owned());
            })
            .unwrap();
        }
        assert_eq!(out, vec!["one".to_string(), "two".to_string()]);
    }

    /// An upstream that never sends a separator must not be able to grow this
    /// buffer until the node is OOM-killed — that would take down every tenant
    /// on it, not just the one stream.
    #[test]
    fn an_unterminated_frame_is_refused_rather_than_grown() {
        let mut s = FrameScanner::new();
        let junk = vec![b'x'; 64 * 1024];
        let mut err = None;
        for _ in 0..32 {
            if let Err(e) = s.feed(&junk, |_| {}) {
                err = Some(e);
                break;
            }
        }
        assert_eq!(err, Some(SseError::FrameTooLarge));
        assert_eq!(s.pending_bytes(), 0, "the buffer must be released on error");
    }

    #[test]
    fn no_data_line_yields_no_frame() {
        let mut s = FrameScanner::new();
        assert!(collect(&mut s, b"event: ping\nid: 7\n\n").is_empty());
    }

    #[test]
    fn a_value_without_a_leading_space_parses() {
        let mut s = FrameScanner::new();
        assert_eq!(
            collect(&mut s, b"data:tight\n\n"),
            vec![(None, "tight".into())]
        );
    }
}
