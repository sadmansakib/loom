//! Minimal SSE (server-sent events) decoder for streaming chat APIs.
//!
//! Hand-rolled because `eventsource-stream` is unmaintained and the
//! OpenAI-compatible chat APIs use a small, fixed subset of SSE: `data`
//! fields, blank-line dispatch, and `:`-comment keep-alives. This module
//! is a pure byte→event state machine with no I/O, so it is fully
//! unit-testable.
//!
//! Semantics (per the SSE spec, restricted to what these APIs send):
//!
//! - Lines are `\n`-terminated; a trailing `\r` is stripped (CRLF).
//! - Lines starting with `:` are comments (`: keep-alive`) and are
//!   ignored.
//! - `data: value` appends `value` to the current event (one optional
//!   space after the colon is stripped); multiple `data` lines join
//!   with `\n` at dispatch.
//! - Other fields (`event:`, `id:`, `retry:`) are parsed for the leading
//!   space only and discarded.
//! - A blank line dispatches the accumulated event; events with no
//!   `data` lines dispatch as empty and are skipped.
//! - An event still incomplete at EOF is discarded — early connection
//!   close is the *caller's* signal to classify (the chat API always
//!   terminates with a blank line after `data: [DONE]`). Flushing a
//!   pending event instead would silently accept truncated responses
//!   (e.g. a proxy cut mid-event); the error-and-retry direction is the
//!   safe one, so adapters reusing this decoder get no flush by design.
//!   An adapter facing gateways that close right after `data: [DONE]`
//!   can peek [`SseDecoder::pending_event`] for exactly `[DONE]` at
//!   EOF — the DeepSeek adapter does.
//!
//! Buffering is byte-based so a chunk boundary inside a multi-byte UTF-8
//! sequence cannot corrupt a line (`0x0A` never appears inside one).

/// Upper bound on a single pending SSE line or assembled event (1 MiB).
///
/// Chunks on these APIs are a few KB. A stream that exceeds this — e.g.
/// a proxy sending bytes with no `\n`, or endless `data:` lines with no
/// blank line — is misbehaving or hostile, and must not grow memory
/// without limit (the idle timeout cannot help: bytes keep arriving).
pub(crate) const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// One dispatched SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The event's `data` lines joined with `\n`.
    pub data: String,
}

/// Decoder failure: the byte stream violates the size contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseError {
    /// A pending line or partially assembled event exceeded
    /// [`MAX_EVENT_BYTES`].
    EventTooLarge {
        /// The limit that was exceeded.
        limit: usize,
    },
}

/// Incremental SSE decoder: feed raw body bytes, receive complete events.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    /// Undelivered bytes (no `\n` yet). Bytes, not `String`: a chunk may
    /// end mid-UTF-8-sequence.
    buffer: Vec<u8>,
    /// `data` lines of the event currently being assembled.
    data: Vec<String>,
    /// Running byte size of [`Self::data`]'s contents. Maintained
    /// incrementally so the [`MAX_EVENT_BYTES`] check is O(1) per line —
    /// rescanning `data` per chunk would make a hostile stream of many
    /// small `data:` lines quadratic before the cap fires.
    pending_bytes: usize,
    /// Length of the prefix of `buffer` already scanned for `\n` in
    /// earlier chunks (known newline-free). Without it, a long line
    /// split across many tiny chunks would be re-scanned from the
    /// start on every chunk — quadratic up to [`MAX_EVENT_BYTES`]².
    scanned: usize,
}

impl SseDecoder {
    /// Creates an empty decoder.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds one body chunk and returns every event completed by it.
    ///
    /// Errors once a pending line or partially assembled event exceeds
    /// [`MAX_EVENT_BYTES`] — the caller should fail the stream; the
    /// decoder is left empty and unusable.
    pub(crate) fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        // Scan with an offset and drain once at the end: draining per
        // line would move the remaining bytes once per line. The first
        // search resumes after `self.scanned` — the tail already
        // scanned in earlier chunks — so a partial line is never
        // re-scanned; afterwards the cursors merge.
        let mut start = 0;
        let mut search = self.scanned;
        while let Some(offset) = self.buffer[search..].iter().position(|&b| b == b'\n') {
            let newline = search + offset;
            let mut line = &self.buffer[start..newline];
            start = newline + 1;
            search = start;
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if line.is_empty() {
                // Spec §9 step 9: an empty data buffer dispatches
                // nothing. Covers both "no data lines" and `data:` with
                // nothing after the colon — some OpenAI-compatible
                // gateways send the latter as a heartbeat, and
                // dispatching an empty-string event would make the
                // caller's JSON parse fail as a Protocol error.
                let data = std::mem::take(&mut self.data).join("\n");
                self.pending_bytes = 0;
                if !data.is_empty() {
                    events.push(SseEvent { data });
                }
                continue;
            }
            if line.first() == Some(&b':') {
                continue; // comment / keep-alive
            }
            let (field, value) = match line.iter().position(|&b| b == b':') {
                Some(colon) => {
                    let value = line[colon + 1..]
                        .strip_prefix(b" ")
                        .unwrap_or(&line[colon + 1..]);
                    (&line[..colon], value)
                }
                None => (line, &[][..]),
            };
            // Only `data` carries a payload for these APIs; `event`/`id`/
            // `retry` are acknowledged and dropped. The size check runs
            // per line, so a single chunk cannot pile up lines past the
            // cap before it fires.
            if field == b"data" {
                let line_data = String::from_utf8_lossy(value).into_owned();
                self.pending_bytes += line_data.len();
                self.data.push(line_data);
                if self.pending_bytes > MAX_EVENT_BYTES {
                    self.buffer.clear();
                    self.data.clear();
                    self.pending_bytes = 0;
                    self.scanned = 0;
                    return Err(SseError::EventTooLarge {
                        limit: MAX_EVENT_BYTES,
                    });
                }
            }
        }
        self.buffer.drain(..start);
        // The failing `position()` just scanned the whole remaining
        // buffer, so it is now known newline-free.
        self.scanned = self.buffer.len();
        // At this point the buffer holds only a partial line (no `\n`).
        if self.buffer.len() > MAX_EVENT_BYTES {
            self.buffer.clear();
            self.data.clear();
            self.pending_bytes = 0;
            self.scanned = 0;
            return Err(SseError::EventTooLarge {
                limit: MAX_EVENT_BYTES,
            });
        }
        // The many-small-`data:`-lines threat is handled per line via
        // `pending_bytes` inside the loop above.
        Ok(events)
    }

    /// The event pending in the decoder, if any: complete `data:` lines
    /// seen since the last dispatched event, joined per SSE. Peek only
    /// — nothing is consumed. Callers use this at end-of-stream to
    /// detect a terminator the server sent without the closing blank
    /// line, which [`SseDecoder::push_bytes`](Self::push_bytes)
    /// spec-correctly never dispatches.
    pub(crate) fn pending_event(&self) -> Option<SseEvent> {
        if self.data.is_empty() {
            None
        } else {
            Some(SseEvent {
                data: self.data.join("\n"),
            })
        }
    }

    /// The unterminated trailing line, if any: bytes still in the
    /// buffer after the last `\n` (at rest the buffer holds only this
    /// partial line). Peek only — nothing is consumed. Callers use
    /// this at end-of-stream to detect a terminator sent with no
    /// newline at all, which [`SseDecoder::pending_event`] (complete
    /// lines only) cannot see.
    pub(crate) fn pending_line(&self) -> &[u8] {
        &self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(parts: &[&str]) -> Vec<String> {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for part in parts {
            out.extend(decoder.push_bytes(part.as_bytes()).unwrap());
        }
        out.into_iter().map(|event| event.data).collect()
    }

    #[test]
    fn single_data_event() {
        assert_eq!(decode(&["data: {\"a\":1}\n\n"]), [r#"{"a":1}"#.to_string()]);
    }

    #[test]
    fn event_split_across_chunks() {
        assert_eq!(
            decode(&["data: {\"a\"", ":1}\n\n"]),
            [r#"{"a":1}"#.to_string()]
        );
    }

    #[test]
    fn crlf_line_endings() {
        assert_eq!(decode(&["data: hello\r\n\r\n"]), ["hello".to_string()]);
    }

    #[test]
    fn keep_alive_comments_ignored() {
        assert_eq!(
            decode(&[": keep-alive\ndata: x\n\n: keep-alive\n\n"]),
            ["x".to_string()]
        );
    }

    #[test]
    fn empty_data_line_dispatches_nothing() {
        // Spec §9 step 9: an empty data buffer dispatches nothing —
        // covers `data:` heartbeats sent by some OpenAI-compatible
        // gateways (DeepSeek itself uses `: keep-alive` comments).
        assert_eq!(decode(&["data:\n\n"]), Vec::<String>::new());
        assert_eq!(decode(&["data:\n\ndata: x\n\n"]), ["x".to_string()]);
    }

    #[test]
    fn pending_line_exposes_unterminated_bytes() {
        // At rest the buffer holds only the partial line; the adapter
        // matches it at EOF for a newline-less `data: [DONE]` close.
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push_bytes(b"data: x\n\n").unwrap().len(), 1);
        assert_eq!(decoder.pending_event(), None);
        assert_eq!(decoder.pending_line(), b"");
        assert!(decoder.push_bytes(b"data: [DONE]").unwrap().is_empty());
        assert_eq!(decoder.pending_line(), b"data: [DONE]");
    }

    #[test]
    fn long_line_split_across_one_byte_chunks_decodes() {
        // Worst case for scan resumption: one 200 KB line fed one byte
        // at a time. Re-scanning the partial line per chunk would be
        // quadratic here (~2x10^10 byte comparisons); resuming at
        // `scanned` keeps it linear.
        let payload = "x".repeat(200_000);
        let stream = format!("data: {payload}\n\n");
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for byte in stream.as_bytes() {
            events.extend(decoder.push_bytes(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(events, vec![SseEvent { data: payload }]);
    }

    #[test]
    fn multiple_data_lines_join_with_newline() {
        assert_eq!(
            decode(&["data: first\ndata: second\n\n"]),
            ["first\nsecond".to_string()]
        );
    }

    #[test]
    fn unknown_fields_discarded() {
        assert_eq!(
            decode(&["event: message\nid: 7\ndata: payload\n\n"]),
            ["payload".to_string()]
        );
    }

    #[test]
    fn no_space_after_colon() {
        assert_eq!(decode(&["data:tight\n\n"]), ["tight".to_string()]);
    }

    #[test]
    fn incomplete_event_at_eof_discarded() {
        // No blank line: the pending event never dispatches.
        assert_eq!(decode(&["data: partial\n"]), Vec::<String>::new());
    }

    #[test]
    fn blank_line_without_data_skipped() {
        assert_eq!(decode(&["\n\ndata: real\n\n"]), ["real".to_string()]);
    }

    #[test]
    fn utf8_split_across_chunks() {
        // "ü" is 0xC3 0xBC; split it across chunks.
        let mut decoder = SseDecoder::new();
        assert!(decoder.push_bytes(b"data: \xc3").unwrap().is_empty());
        let events = decoder.push_bytes(b"\xbc\n\n").unwrap();
        assert_eq!(events, [SseEvent { data: "ü".into() }]);
    }

    #[test]
    fn oversized_pending_line_is_rejected() {
        // A line with no `\n` in sight grows past the cap.
        let mut decoder = SseDecoder::new();
        let blob = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert_eq!(
            decoder.push_bytes(&blob).unwrap_err(),
            SseError::EventTooLarge {
                limit: MAX_EVENT_BYTES
            }
        );
    }

    #[test]
    fn oversized_event_via_many_data_lines_is_rejected() {
        // Small, newline-terminated lines are each fine; the joined
        // payload of the never-dispatching event trips the cap.
        let mut decoder = SseDecoder::new();
        let line = b"data: 0123456789abcdef\n";
        let mut error = None;
        // 22 fed bytes per line carry 16 payload bytes; iterating up to
        // cap/8 fed-payload-equivalents terminates even if the line
        // shape changes, without relying on the exact ratio.
        for _ in 0..(MAX_EVENT_BYTES / 8) {
            match decoder.push_bytes(line) {
                Ok(events) => assert!(events.is_empty()),
                Err(found) => {
                    error = Some(found);
                    break;
                }
            }
        }
        assert_eq!(
            error,
            Some(SseError::EventTooLarge {
                limit: MAX_EVENT_BYTES
            })
        );
    }

    #[test]
    fn large_chunk_of_complete_lines_is_fine() {
        // The cap bounds *pending* bytes only: a big chunk full of
        // complete events must decode.
        let mut decoder = SseDecoder::new();
        let stream = "data: x\n\n".repeat(300_000);
        let events = decoder.push_bytes(stream.as_bytes()).unwrap();
        assert_eq!(events.len(), 300_000);
    }

    #[test]
    fn done_marker_is_ordinary_data() {
        assert_eq!(decode(&["data: [DONE]\n\n"]), ["[DONE]".to_string()]);
    }
}
