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
            let timeouts = provider.timeouts();
            let mut request = provider
                .client()
                .post(url)
                .bearer_auth(provider.api_key())
                .json(&body);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let send = request.send();
            let response = match tokio::time::timeout(timeouts.header, send).await {
                Err(_) => {
                    return Some((
                        Err(ModelError::new(
                            ModelErrorKind::Transport,
                            "timed out waiting for response headers",
                        )),
                        State::Done,
                    ));
                }
                Ok(Err(error)) => {
                    // Builder failures (malformed base URL, bad options)
                    // are configuration mistakes, not transport faults:
                    // retrying cannot fix them.
                    let kind = if error.is_builder() {
                        ModelErrorKind::InvalidRequest
                    } else {
                        ModelErrorKind::Transport
                    };
                    return Some((
                        Err(ModelError::new(kind, format!("request failed: {error}"))),
                        State::Done,
                    ));
                }
                Ok(Ok(response)) => response,
            };
            let status = response.status();
            if !status.is_success() {
                let retry_after = crate::http::retry_after(&response);
                let body = crate::http::read_error_body(response, timeouts.idle).await;
                let mut error = P::classify_status(status, &body);
                if let Some(delay) = retry_after {
                    error = error.with_retry_after(delay);
                }
                return Some((Err(error), State::Done));
            }
            // A 200 with a non-SSE body (captive portal, HTML error
            // page from a proxy) contains no `data:` lines: it would
            // surface as "stream ended without [DONE]" — a retryable
            // Transport error — and be re-sent forever. Fail fast,
            // non-retryably. OpenAI-compatible endpoints answer
            // `text/event-stream` (possibly with a charset parameter).
            // MIME types are case-insensitive (RFC 9110 §8.3): a proxy
            // rewriting the header to `Text/Event-Stream` is still a
            // valid SSE response. Prefix compare keeps a trailing
            // `;charset=utf-8` parameter accepted.
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            let is_sse = content_type
                .get(.."text/event-stream".len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/event-stream"));
            if !is_sse {
                return Some((
                    Err(ModelError::new(
                        ModelErrorKind::Protocol,
                        format!("expected text/event-stream, got {content_type:?}"),
                    )),
                    State::Done,
                ));
            }
            let body_stream = response.bytes_stream();
            Some((
                Ok(None),
                State::Body {
                    provider,
                    body_stream: Box::pin(body_stream),
                    decoder: SseDecoder::new(),
                    accumulator: Box::new(ChunkAccumulator::default()),
                    pending: VecDeque::new(),
                    saw_done: false,
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
            let idle = provider.timeouts().idle;
            match tokio::time::timeout(idle, body_stream.next()).await {
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
                    // Connection closed without a dispatched [DONE].
                    // Narrow EOF tolerance: gateways may close right
                    // after `data: [DONE]` without the closing blank
                    // line, which the decoder (spec-correctly) never
                    // dispatches — a complete response would otherwise
                    // fail as retryable Transport on every retry. Accept
                    // exactly that pending event: the terminator is the
                    // server's deliberate end signal, so it cannot hide
                    // truncated content. A terminator with no newline
                    // at all (`data: [DONE]` then EOF — a gateway
                    // close) is equally deliberate, so it is matched
                    // on the raw trailing bytes too — but only when
                    // nothing else is pending: an undispatched data
                    // event (blank-line-less gateway) plus the
                    // terminator must NOT pass as a complete response,
                    // because that event was never accumulated.
                    // Dropping it silently would be exactly the
                    // truncated-response takeover the no-flush rule
                    // exists to prevent. Any other pending bytes at
                    // EOF still error (and retry) — deliberately no
                    // flush: accepting a half-received event would
                    // silently take truncated responses (e.g. a proxy
                    // cut mid-event), and error-and-retry is the safe
                    // direction.
                    let saw_done = saw_done
                        || decoder
                            .pending_event()
                            .is_some_and(|event| event.data == "[DONE]")
                        || (decoder.pending_event().is_none()
                            && is_unterminated_done_line(decoder.pending_line()));
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
                    let events = match decoder.push_bytes(&chunk) {
                        Ok(events) => events,
                        Err(error) => {
                            // Oversized pending event: misbehaving or
                            // hostile endpoint, not a transient fault.
                            log_finish_reason(&accumulator);
                            return Some((
                                Err(ModelError::new(
                                    ModelErrorKind::Protocol,
                                    format!("SSE decode failed: {error:?}"),
                                )),
                                State::Done,
                            ));
                        }
                    };
                    for event in events {
                        if event.data == DONE_MARKER {
                            // Anything after [DONE] in the same body read
                            // is trailing junk (some proxies append it);
                            // the response is final, stop ingesting.
                            saw_done = true;
                            break;
                        }
                        let parsed: ChatChunk = match serde_json::from_str(&event.data) {
                            Ok(parsed) => parsed,
                            Err(error) => {
                                log_finish_reason(&accumulator);
                                return Some((
                                    Err(ModelError::new(
                                        ModelErrorKind::Protocol,
                                        format!("unparseable SSE data chunk: {error}"),
                                    )),
                                    State::Done,
                                ));
                            }
                        };
                        pending.extend(accumulator.ingest(parsed));
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
                        },
                    ))
                }
            }
        }
        State::Done => None,
    }
}
