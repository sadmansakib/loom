//! The provider-agnostic streaming driver.
//!
//! One SSE machine serves every OpenAI-compatible adapter: send the
//! request, decode the event stream, accumulate chunks, classify
//! failures, and emit display deltas plus exactly one terminal
//! `Completed`. Adapters differ only in configuration and error
//! classification, which they expose through [`WireEndpoint`]; keeping
//! one machine means the delicate parts — unterminated-`[DONE]`
//! tolerance, idle timeouts, mid-stream error objects, stop-reason
//! normalization — cannot drift between providers.

use std::collections::VecDeque;

use futures::StreamExt;
use loom_kernel::model::{
    ModelError, ModelErrorKind, ModelEvent, ModelRequest, ModelResponse, ModelStream,
};
use serde_json::Value;

use crate::http::TimeoutConfig;
use crate::openai_wire::{ChatChunk, ChunkAccumulator, DONE_MARKER, FinalizePolicy};
use crate::sse::SseDecoder;

/// A configured OpenAI-compatible endpoint.
///
/// Implemented by the adapters; [`ModelProvider::stream`] forwards here.
///
/// [`ModelProvider::stream`]: loom_kernel::model::ModelProvider::stream
pub(crate) trait WireEndpoint: Clone + Send + Sync + 'static {
    /// Provider name: used for synthesized tool-call ids and errors.
    fn name(&self) -> &str;

    /// HTTP client to send with.
    fn client(&self) -> &reqwest::Client;

    /// Fully-built `POST` URL for chat completions.
    fn endpoint_url(&self) -> String;

    /// Bearer token sent in the `Authorization` header.
    fn api_key(&self) -> &str;

    /// Serialized request body for this request.
    fn request_body(&self, request: &ModelRequest) -> Value;

    /// Extra request headers (attribution, proxies). Empty when none.
    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)>;

    /// Liveness timeouts.
    fn timeouts(&self) -> TimeoutConfig;

    /// Classifies a non-success HTTP status together with a bounded
    /// body excerpt.
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError;

    /// Maps a wire `finish_reason` that means transport/overload failure
    /// rather than why the model stopped, if any.
    fn in_stream_failure(reason: &str) -> Option<ModelErrorKind>;

    /// Classifies an error object carried *inside* the stream (some
    /// providers report a failure as a chunk rather than an HTTP status).
    ///
    /// Defaults to the shared status-based classifier; an adapter
    /// overrides this only when its wire deviates.
    fn classify_wire_error(status: Option<u16>, message: &str) -> ModelErrorKind {
        crate::openai_wire::classify_wire_error(status, message)
    }

    /// Wire field this dialect streams reasoning fragments under, if it
    /// deviates from the shared `reasoning_content` default. Groq streams
    /// and replays the trace as `reasoning` and rejects the default key
    /// on an assistant message, so both the stream reader and the request
    /// builder are told which dialect they are speaking.
    fn reasoning_field(&self) -> Option<&'static str> {
        None
    }

    /// Reads this provider's dialect fields out of a usage block's
    /// non-standard keys (everything the OpenAI-standard shapes in
    /// [`WireUsage`](crate::openai_wire::WireUsage) do not claim).
    ///
    /// Defaults to none — an adapter whose usage block sticks to the
    /// standard shapes (GLM) adds no code. DeepSeek overrides for
    /// `prompt_cache_hit_tokens`, OpenRouter for `cost`.
    fn dialect_usage(
        _extra: &serde_json::Map<String, serde_json::Value>,
    ) -> crate::openai_wire::DialectUsage {
        crate::openai_wire::DialectUsage::default()
    }
}

/// Drives one streamed request to completion for `provider`.
pub(crate) fn stream_endpoint<'a, P: WireEndpoint>(
    provider: &'a P,
    request: &'a ModelRequest,
) -> ModelStream<'a> {
    let state = State::<P>::Send {
        provider: provider.clone(),
        url: provider.endpoint_url(),
        headers: provider.extra_headers(),
        body: provider.request_body(request),
    };
    Box::pin(futures::stream::unfold(state, step::<P>).filter_map(
        |item: Result<Option<ModelEvent>, ModelError>| async move {
            match item {
                Ok(Some(event)) => Some(Ok(event)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            }
        },
    ))
}

/// The unfold state machine backing [`stream_endpoint`].
enum State<P: WireEndpoint> {
    /// Request built, not yet sent.
    Send {
        provider: P,
        url: String,
        headers: Vec<(reqwest::header::HeaderName, String)>,
        body: Value,
    },
    /// Headers received; body streaming in progress. `pending` holds
    /// display deltas produced by the last ingested chunk (drained
    /// before reading more), `saw_done` records `[DONE]`.
    Body {
        provider: P,
        body_stream: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
        decoder: SseDecoder,
        // Boxed: this variant must not dwarf the others (clippy::
        // large_enum_variant); one allocation per request is noise.
        accumulator: Box<ChunkAccumulator>,
        pending: VecDeque<ModelEvent>,
        saw_done: bool,
        /// Instant of the last *dispatched* SSE event; the content-stall
        /// window measures from here, so keep-alive comments and empty
        /// heartbeats (which dispatch nothing) do not reset it.
        last_progress: tokio::time::Instant,
    },
    /// Terminal: `Completed` emitted or error yielded.
    Done,
}

/// Builds the finalize policy for a stream from the adapter's own
/// classification hooks — the driver holds no provider knowledge.
fn policy<P: WireEndpoint>(provider: &P) -> FinalizePolicy<'_> {
    FinalizePolicy {
        classify_in_stream: &P::in_stream_failure,
        classify_wire_error: &P::classify_wire_error,
        id_prefix: provider.name(),
        dialect_usage: &P::dialect_usage,
    }
}

/// Records the raw wire `finish_reason` (always recorded, never acted
/// on) under `model.finish_reason_raw`, on every terminal path —
/// success, early close, or failure.
fn log_finish_reason(accumulator: &ChunkAccumulator) {
    tracing::debug!(
        target: "model",
        finish_reason_raw = accumulator.raw_finish_reason().unwrap_or(""),
        "model stream finished"
    );
}

/// `data: [DONE]` as a trailing line with no terminating newline —
/// lenient about the optional space after the colon and a `\r` left
/// by a half-CRLF close. The terminator is unambiguous, so a gateway
/// that closes the socket right after it must not turn a complete
/// response into retry-until-limit Transport errors. Anything else in
/// the trailing bytes still fails loudly.
fn is_unterminated_done_line(line: &[u8]) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line == b"data: [DONE]" || line == b"data:[DONE]"
}

/// Sends the request and awaits response headers, mapping the two
/// send-phase failures: a header timeout is retryable transport, a
/// reqwest builder failure is a configuration mistake (non-retryable),
/// anything else is transport. Authentication travels in `headers` —
/// the OpenAI-compatible endpoints as `Authorization: Bearer`, the
/// Messages API as `x-api-key` — so the wire bytes stay each
/// protocol's own.
pub(crate) async fn send_request(
    client: &reqwest::Client,
    url: String,
    timeouts: TimeoutConfig,
    headers: Vec<(reqwest::header::HeaderName, String)>,
    body: Value,
) -> Result<reqwest::Response, ModelError> {
    let mut request = client.post(url).json(&body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    match tokio::time::timeout(timeouts.header, request.send()).await {
        Err(_) => Err(ModelError::new(
            ModelErrorKind::Transport,
            "timed out waiting for response headers",
        )),
        Ok(Err(error)) => {
            // Builder failures (malformed base URL, bad options) are
            // configuration mistakes, not transport faults: retrying
            // cannot fix them.
            let kind = if error.is_builder() {
                ModelErrorKind::InvalidRequest
            } else {
                ModelErrorKind::Transport
            };
            Err(ModelError::new(kind, format!("request failed: {error}")))
        }
        Ok(Ok(response)) => {
            // Live-stall forensics (NIM, 2026-10-15): no request-send
            // logging existed, so a silently stalled stream left nothing
            // to correlate against frozen socket byte counts.
            tracing::debug!(
                status = %response.status(),
                "sse response headers received"
            );
            Ok(response)
        }
    }
}

/// Classifies a non-success status into `classify`'s error, with any
/// `Retry-After` header attached and a bounded body excerpt read.
pub(crate) async fn non_success_error(
    response: reqwest::Response,
    idle: std::time::Duration,
    classify: impl Fn(reqwest::StatusCode, &str) -> ModelError,
) -> ModelError {
    let status = response.status();
    let retry_after = crate::http::retry_after(&response);
    let body = crate::http::read_error_body(response, idle).await;
    let mut error = classify(status, &body);
    if let Some(delay) = retry_after {
        error = error.with_retry_after(delay);
    }
    error
}

/// Takes apart a success response into its SSE body stream, refusing a
/// 200 that is not `text/event-stream` (captive portal, HTML error page
/// from a proxy): such a body contains no `data:` lines and would
/// otherwise surface as retryable Transport forever. MIME types are
/// case-insensitive (RFC 9110 §8.3); prefix compare keeps a trailing
/// `;charset=utf-8` parameter accepted.
pub(crate) fn sse_body(
    response: reqwest::Response,
) -> Result<futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>, ModelError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let is_sse = content_type
        .get(.."text/event-stream".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/event-stream"));
    if is_sse {
        Ok(Box::pin(response.bytes_stream()))
    } else {
        Err(ModelError::new(
            ModelErrorKind::Protocol,
            format!("expected text/event-stream, got {content_type:?}"),
        ))
    }
}

/// Whether EOF on the connection still counts as a deliberate `[DONE]`.
///
/// Narrow EOF tolerance: gateways may close right after `data: [DONE]`
/// without the closing blank line, which the decoder (spec-correctly)
/// never dispatches — a complete response would otherwise fail as
/// retryable Transport on every retry. Accept exactly that pending
/// event: the terminator is the server's deliberate end signal, so it
/// cannot hide truncated content. A terminator with no newline at all
/// (`data: [DONE]` then EOF — a gateway close) is equally deliberate,
/// matched on the raw trailing bytes too — but only when nothing else is
/// pending: an undispatched data event (blank-line-less gateway) plus
/// the terminator must NOT pass as a complete response, because that
/// event was never accumulated. Dropping it silently would be exactly
/// the truncated-response takeover the no-flush rule exists to prevent.
/// Any other pending bytes at EOF still error (and retry) — deliberately
/// no flush: accepting a half-received event would silently take
/// truncated responses (e.g. a proxy cut mid-event), and error-and-retry
/// is the safe direction.
fn eof_is_done(saw_done: bool, decoder: &SseDecoder) -> bool {
    saw_done
        || decoder
            .pending_event()
            .is_some_and(|event| event.data == "[DONE]")
        || (decoder.pending_event().is_none() && is_unterminated_done_line(decoder.pending_line()))
}

/// Outcome of ingesting one body chunk.
struct Ingested {
    /// `[DONE]` was seen; the response is final.
    saw_done: bool,
    /// At least one SSE data event was dispatched — real content
    /// progress, the thing the content-stall window measures. A
    /// keep-alive comment or an empty `data:` heartbeat dispatches none.
    progressed: bool,
}

/// Ingests one body chunk: decodes SSE events, parses each data chunk
/// into the accumulator, and extends `pending` with the display deltas.
/// Reports the updated `saw_done` and whether any event was dispatched;
/// decode failures and unparseable chunks are non-retryable protocol
/// errors.
fn ingest_chunk(
    decoder: &mut SseDecoder,
    accumulator: &mut ChunkAccumulator,
    pending: &mut VecDeque<ModelEvent>,
    saw_done: bool,
    chunk: &[u8],
) -> Result<Ingested, ModelError> {
    let events = match decoder.push_bytes(chunk) {
        Ok(events) => events,
        // Oversized pending event: misbehaving or hostile endpoint,
        // not a transient fault.
        Err(error) => {
            return Err(ModelError::new(
                ModelErrorKind::Protocol,
                format!("SSE decode failed: {error:?}"),
            ));
        }
    };
    let progressed = !events.is_empty();
    for event in events {
        if event.data == DONE_MARKER {
            // Anything after [DONE] in the same body read is trailing
            // junk (some proxies append it); the response is final,
            // stop ingesting.
            return Ok(Ingested {
                saw_done: true,
                progressed,
            });
        }
        let parsed: ChatChunk = match serde_json::from_str(&event.data) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    format!("unparseable SSE data chunk: {error}"),
                ));
            }
        };
        pending.extend(accumulator.ingest(parsed));
    }
    Ok(Ingested {
        saw_done,
        progressed,
    })
}

/// One unfold step: `Ok((None, next))` transitions without emitting,
/// `Ok((Some(event), next))` emits, `Err` ends the stream with a
/// failure after which the state is `Done`.
async fn step<P: WireEndpoint>(
    state: State<P>,
) -> Option<(Result<Option<ModelEvent>, ModelError>, State<P>)> {
    match state {
        State::Send {
            provider,
            url,
            headers,
            body,
        } => {
            // Auth travels as a header so the shared send path stays
            // protocol-neutral (the Messages API sends x-api-key).
            let mut headers = headers;
            headers.insert(
                0,
                (
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", provider.api_key()),
                ),
            );
            let response = match send_request(
                provider.client(),
                url,
                provider.timeouts(),
                headers,
                body,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => return Some((Err(error), State::Done)),
            };
            if !response.status().is_success() {
                let idle = provider.timeouts().idle;
                return Some((
                    Err(non_success_error(response, idle, P::classify_status).await),
                    State::Done,
                ));
            }
            let body_stream = match sse_body(response) {
                Ok(body_stream) => body_stream,
                Err(error) => return Some((Err(error), State::Done)),
            };
            // Read the dialect before `provider` moves into the state.
            let reasoning_field = provider.reasoning_field();
            Some((
                Ok(None),
                State::Body {
                    provider,
                    body_stream,
                    decoder: SseDecoder::new(),
                    accumulator: Box::new(
                        ChunkAccumulator::default().with_reasoning_field(reasoning_field),
                    ),
                    pending: VecDeque::new(),
                    saw_done: false,
                    last_progress: tokio::time::Instant::now(),
                },
            ))
        }
        State::Body {
            provider,
            mut body_stream,
            mut decoder,
            mut accumulator,
            mut pending,
            mut saw_done,
            mut last_progress,
        } => {
            if let Some(event) = pending.pop_front() {
                return Some((
                    Ok(Some(event)),
                    State::Body {
                        provider,
                        body_stream,
                        decoder,
                        accumulator,
                        pending,
                        saw_done,
                        last_progress,
                    },
                ));
            }
            if saw_done {
                log_finish_reason(&accumulator);
                let result = accumulator.finalize(true, &policy(&provider));
                return Some((
                    result.map(|response: ModelResponse| Some(ModelEvent::Completed(response))),
                    State::Done,
                ));
            }
            // Two windows bound this read: the idle window measures raw
            // byte flow (keep-alive comments reset it), the stall window
            // measures dispatched events (they do not). Whichever expiry
            // comes first decides the error.
            let timeouts = provider.timeouts();
            let stall_remaining = timeouts.stall.saturating_sub(last_progress.elapsed());
            let window = timeouts.idle.min(stall_remaining);
            let content_stall = stall_remaining <= timeouts.idle;
            match tokio::time::timeout(window, body_stream.next()).await {
                Err(_) if content_stall => {
                    log_finish_reason(&accumulator);
                    Some((
                        Err(ModelError::new(
                            ModelErrorKind::Transport,
                            "stall timeout: no SSE event within the configured window \
                             (keep-alive pings do not count as progress)",
                        )),
                        State::Done,
                    ))
                }
                Err(_) => {
                    log_finish_reason(&accumulator);
                    Some((
                        Err(ModelError::new(
                            ModelErrorKind::Transport,
                            "idle timeout: no SSE event within the configured window",
                        )),
                        State::Done,
                    ))
                }
                Ok(None) => {
                    let saw_done = eof_is_done(saw_done, &decoder);
                    log_finish_reason(&accumulator);
                    let result = accumulator.finalize(saw_done, &policy(&provider));
                    if saw_done {
                        // EOF tolerance fired: emit the response.
                        return Some((
                            result.map(|response: ModelResponse| {
                                Some(ModelEvent::Completed(response))
                            }),
                            State::Done,
                        ));
                    }
                    let error = match result {
                        Err(error) => error,
                        // Unreachable: finalize rejects `!saw_done`.
                        Ok(_) => ModelError::new(
                            ModelErrorKind::Protocol,
                            "stream ended without [DONE] but assembled a response",
                        ),
                    };
                    Some((Err(error), State::Done))
                }
                Ok(Some(Err(error))) => {
                    log_finish_reason(&accumulator);
                    Some((
                        Err(ModelError::new(
                            ModelErrorKind::Transport,
                            format!("body read failed: {error}"),
                        )),
                        State::Done,
                    ))
                }
                Ok(Some(Ok(chunk))) => {
                    match ingest_chunk(
                        &mut decoder,
                        &mut accumulator,
                        &mut pending,
                        saw_done,
                        &chunk,
                    ) {
                        Ok(ingested) => {
                            saw_done = ingested.saw_done;
                            if ingested.progressed {
                                last_progress = tokio::time::Instant::now();
                            }
                        }
                        Err(error) => {
                            log_finish_reason(&accumulator);
                            return Some((Err(error), State::Done));
                        }
                    }
                    Some((
                        Ok(None),
                        State::Body {
                            provider,
                            body_stream,
                            decoder,
                            accumulator,
                            pending,
                            saw_done,
                            last_progress,
                        },
                    ))
                }
            }
        }
        State::Done => None,
    }
}
