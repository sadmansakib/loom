//! The DeepSeek adapter: an OpenAI-compatible wire client plus DeepSeek
//! error classification, in-stream failure mapping, and a capability
//! table.
//!
//! Wire behaviour verified against the DeepSeek API reference
//! (api-docs.deepseek.com, fetched 2026-10-03): `POST /chat/completions`
//! with streamed `reasoning_content` and tool-call fragments, usage on
//! the final content chunk, `data: [DONE]` terminator, `: keep-alive`
//! comments while queued, HTTP-status-only errors.
//!
//! Capabilities come from a built-in table because the API serves none:
//! `GET /models` lists ids and owners only, so the numbers (context and
//! output limits, tool support) live here, mirroring the published
//! "Models & Pricing" page. Unknown ids return `None`; keeping the
//! table correct for new releases is a code change, deliberately —
//! there is no user-facing capability configuration to drift out of
//! sync with the adapter's wire behaviour.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use loom_core::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ModelStream,
};
use serde_json::Value;

use crate::openai_wire::{
    ChatChunk, ChunkAccumulator, DONE_MARKER, WireOptions, build_request_body,
};
use crate::secret::SecretString;
use crate::sse::SseDecoder;

/// Default DeepSeek API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// Reasoning effort the adapter sends explicitly on every request.
///
/// Always sent (never left to endpoint defaults) so provider-side
/// default changes cannot silently alter task behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// Lowest effort. Does **not** disable reasoning — the model may
    /// still reason on tool-planning turns (verified live); use
    /// [`ThinkingMode::Disabled`] to turn reasoning off.
    None,
    /// Low effort.
    Low,
    /// High effort (the API's documented default).
    #[default]
    High,
    /// Maximum effort.
    Max,
}

impl ReasoningEffort {
    /// The wire string for this effort.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Low => "low",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }
}

/// Error returned when a string is not a valid [`ReasoningEffort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseReasoningEffortError;

impl std::fmt::Display for ParseReasoningEffortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "expected one of `none`, `low`, `high`, `max`")
    }
}

impl std::error::Error for ParseReasoningEffortError {}

impl std::str::FromStr for ReasoningEffort {
    type Err = ParseReasoningEffortError;

    /// Parses the wire spelling exactly (lowercase, matching
    /// [`as_str`](ReasoningEffort::as_str)) — the CLI passes user input
    /// through this, so invalid values fail loudly instead of mapping
    /// to a default.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "max" => Ok(Self::Max),
            _ => Err(ParseReasoningEffortError),
        }
    }
}

/// Liveness timeouts for one streamed request (§6.3).
///
/// Expiry of any of these is a retryable [`ModelErrorKind::Transport`];
/// there is deliberately no total deadline (the cancellation token
/// bounds total time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutConfig {
    /// Time allowed to establish the TCP/TLS connection.
    pub connect: Duration,
    /// Time allowed from request sent to response headers.
    pub header: Duration,
    /// Maximum gap between received body chunks on an open stream — and
    /// therefore between SSE events; keep-alive comments arrive as
    /// chunks and reset this too.
    pub idle: Duration,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            header: Duration::from_secs(300),
            idle: Duration::from_secs(300),
        }
    }
}

/// The independent `thinking` request field (research.md §4.1).
///
/// `Disabled` selects the separate non-thinking mode — the actual
/// reasoning off switch (8 KiB default `max_tokens`, no
/// `reasoning_content` in responses). This is distinct from
/// [`ReasoningEffort::None`], which the live API accepts without
/// complaint but which does **not** suppress reasoning while thinking
/// stays enabled (verified live, research.md §4.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    /// `{"type": "enabled"}` — thinking mode, the API default.
    #[default]
    Enabled,
    /// `{"type": "disabled"}` — the non-thinking mode.
    Disabled,
}

impl ThinkingMode {
    /// Wire string for the `thinking.type` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

/// Configuration for [`DeepSeekProvider`].
#[derive(Debug, Clone)]
pub struct DeepSeekConfig {
    /// API base URL; override for tests or proxies.
    pub base_url: String,
    /// Bearer token, redacted in `Debug` ([`SecretString`], ADR-009).
    /// Reading `DEEPSEEK_API_KEY` from the environment is the caller's
    /// (CLI/account store) job, not the provider's.
    pub api_key: SecretString,
    /// Sent explicitly on every request; see [`ReasoningEffort`].
    pub reasoning_effort: ReasoningEffort,
    /// The independent thinking on/off switch; see [`ThinkingMode`].
    pub thinking: ThinkingMode,
    /// Liveness timeouts; see [`TimeoutConfig`].
    pub timeouts: TimeoutConfig,
}

impl DeepSeekConfig {
    /// Creates a config with the default base URL, default timeouts,
    /// high reasoning effort, and the given key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_key: SecretString::new(api_key),
            reasoning_effort: ReasoningEffort::default(),
            thinking: ThinkingMode::default(),
            timeouts: TimeoutConfig::default(),
        }
    }

    /// Overrides the base URL (builder style).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Overrides the reasoning effort (builder style).
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// Overrides the thinking on/off switch (builder style).
    #[must_use]
    pub fn with_thinking(mut self, mode: ThinkingMode) -> Self {
        self.thinking = mode;
        self
    }

    /// Overrides the timeouts (builder style).
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        self
    }
}

/// The DeepSeek [`ModelProvider`] adapter.
///
/// `stream` uses `tokio::time::timeout`, so it must be polled inside a
/// tokio runtime with the time driver enabled (`#[tokio::main]` and
/// `#[tokio::test]` both qualify); polling it outside one panics.
#[derive(Debug, Clone)]
pub struct DeepSeekProvider {
    config: Arc<DeepSeekConfig>,
    client: reqwest::Client,
}

impl DeepSeekProvider {
    /// Creates a provider with a connection-pooled HTTP client.
    ///
    /// Fails only when the TLS backend cannot be initialized (e.g.
    /// missing system roots); every other aspect of construction is
    /// infallible configuration.
    pub fn new(config: DeepSeekConfig) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.timeouts.connect)
            .build()?;
        Ok(Self {
            config: Arc::new(config),
            client,
        })
    }

    /// Classifies DeepSeek's in-stream failure `finish_reason` values —
    /// conditions that are transport/overload failures, not why the
    /// model stopped generating.
    fn in_stream_failure(reason: &str) -> Option<ModelErrorKind> {
        match reason {
            "insufficient_system_resource" => Some(ModelErrorKind::Overloaded),
            "aborted" => Some(ModelErrorKind::Transport),
            _ => None,
        }
    }

    /// Classifies an HTTP status per the DeepSeek error model
    /// (HTTP-status only; no business codes).
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        let kind = match status.as_u16() {
            400 | 422 => ModelErrorKind::InvalidRequest,
            401 => ModelErrorKind::Auth,
            402 => ModelErrorKind::QuotaExhausted,
            408 => ModelErrorKind::Transport,
            429 => ModelErrorKind::RateLimited,
            503 => ModelErrorKind::Overloaded,
            _ if status.is_client_error() => ModelErrorKind::InvalidRequest,
            _ => ModelErrorKind::Server,
        };
        // Body text is included best-effort; the shape is verified live
        // (research.md §4.1: OpenAI-style error object) and needs no
        // parsing — the excerpt carries error.message for logs/CLI.
        let excerpt: String = body.chars().take(500).collect();
        ModelError::new(kind, format!("HTTP {status}: {excerpt}"))
    }

    /// Parses a `Retry-After` header (seconds form) if present.
    fn retry_after(response: &reqwest::Response) -> Option<Duration> {
        let raw = response.headers().get(reqwest::header::RETRY_AFTER)?;
        let seconds: u64 = std::str::from_utf8(raw.as_bytes())
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some(Duration::from_secs(seconds))
    }
}

/// Capability table mirroring the published DeepSeek "Models & Pricing"
/// page (fetched 2026-10-03): both current models are 1M context /
/// 384K (393,216) max output with tools and thinking. Verified live
/// (research.md §4.1): the API enumerates exactly these two ids — the
/// legacy `deepseek-v4-flash` is rejected with a 400, so it maps to
/// `None` (loud unknown-model failure, not a silently wrong row).
fn model_capabilities(model: &str) -> Option<ModelCapabilities> {
    let limits = match model {
        "deepseek-flash" | "deepseek-v4-pro" => ModelCapabilities::new(1_048_576)
            .with_max_output_tokens(393_216)
            .with_tool_calls(true)
            .with_parallel_tool_calls(true)
            .with_reasoning(true),
        _ => return None,
    };
    Some(limits)
}

impl ModelProvider for DeepSeekProvider {
    fn name(&self) -> &str {
        "deepseek"
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        model_capabilities(model)
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        let options = WireOptions {
            thinking: serde_json::json!({"type": self.config.thinking.as_str()}),
            reasoning_effort: self.config.reasoning_effort.as_str().to_owned(),
        };
        let body = build_request_body(request, &options);
        // Tolerate a trailing slash: `https://host/` + path would
        // otherwise produce `//chat/completions`, which some proxies 404.
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let provider = self.clone();
        Box::pin(
            futures::stream::unfold(
                State::Send {
                    provider,
                    url,
                    body,
                },
                step,
            )
            .filter_map(|item: Result<Option<ModelEvent>, ModelError>| async move {
                match item {
                    Ok(Some(event)) => Some(Ok(event)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                }
            }),
        )
    }
}

/// The unfold state machine backing [`DeepSeekProvider::stream`].
enum State {
    /// Request built, not yet sent.
    Send {
        provider: DeepSeekProvider,
        url: String,
        body: Value,
    },
    /// Headers received; body streaming in progress. `pending` holds
    /// display deltas produced by the last ingested chunk (drained
    /// before reading more), `saw_done` records `[DONE]`.
    Body {
        provider: DeepSeekProvider,
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

/// Cap on bytes read from an error-response body: enough for the
/// classification excerpt, small enough that a hostile server cannot
/// balloon memory.
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// Reads an error-response body for classification: bounded by
/// [`MAX_ERROR_BODY_BYTES`] and by the idle timeout — which here bounds
/// the **whole** error-body read, not each gap between chunks (fine at
/// a 16 KiB cap). On timeout the body is unknowable, so the caller
/// classifies from the HTTP status alone: the error kind must follow
/// the status, never the stall, or non-retryable statuses would turn
/// into retryable ones. The placeholder notes the missing excerpt.
async fn read_error_body(response: reqwest::Response, idle: Duration) -> String {
    let read = async {
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    let remaining = MAX_ERROR_BODY_BYTES - buf.len();
                    buf.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    if buf.len() == MAX_ERROR_BODY_BYTES {
                        break;
                    }
                }
                // Read failure mid-body: classify with whatever arrived.
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    };
    match tokio::time::timeout(idle, read).await {
        Ok(body) => body,
        Err(_) => "(error body unread — read timed out)".to_owned(),
    }
}

/// Records the raw wire `finish_reason` (§5.1: always recorded, never
/// acted on) under `model.finish_reason_raw`, on every terminal path —
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
async fn step(state: State) -> Option<(Result<Option<ModelEvent>, ModelError>, State)> {
    match state {
        State::Send {
            provider,
            url,
            body,
        } => {
            let send = provider
                .client
                .post(url)
                .bearer_auth(provider.config.api_key.expose())
                .json(&body)
                .send();
            let response = match tokio::time::timeout(provider.config.timeouts.header, send).await {
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
                let retry_after = DeepSeekProvider::retry_after(&response);
                let body = read_error_body(response, provider.config.timeouts.idle).await;
                let mut error = DeepSeekProvider::classify_status(status, &body);
                if let Some(delay) = retry_after {
                    error = error.with_retry_after(delay);
                }
                return Some((Err(error), State::Done));
            }
            // A 200 with a non-SSE body (captive portal, HTML error
            // page from a proxy) contains no `data:` lines: it would
            // surface as "stream ended without [DONE]" — a retryable
            // Transport error — and be re-sent forever. Fail fast,
            // non-retryably. DeepSeek always answers
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
                let result = accumulator.finalize(
                    true,
                    DeepSeekProvider::in_stream_failure,
                    provider.name(),
                );
                return Some((
                    result.map(|response: ModelResponse| Some(ModelEvent::Completed(response))),
                    State::Done,
                ));
            }
            match tokio::time::timeout(provider.config.timeouts.idle, body_stream.next()).await {
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
                    let result = accumulator.finalize(
                        saw_done,
                        DeepSeekProvider::in_stream_failure,
                        provider.name(),
                    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_table_current_models() {
        for model in ["deepseek-flash", "deepseek-v4-pro"] {
            let caps = model_capabilities(model).expect("known model");
            assert_eq!(caps.max_context_tokens, 1_048_576);
            assert_eq!(caps.max_output_tokens, Some(393_216));
            assert!(caps.tool_calls && caps.parallel_tool_calls && caps.reasoning);
        }
        // Legacy id rejected by the live API (verified 2026-10-03):
        // unknown models resolve to None, never a silently wrong row.
        assert!(model_capabilities("deepseek-v4-flash").is_none());
    }

    #[test]
    fn capability_table_unknown_model_is_none() {
        assert!(model_capabilities("gpt-5").is_none());
        assert!(model_capabilities("").is_none());
    }

    #[test]
    fn http_status_classification() {
        let cases = [
            (400, ModelErrorKind::InvalidRequest),
            (422, ModelErrorKind::InvalidRequest),
            (401, ModelErrorKind::Auth),
            (402, ModelErrorKind::QuotaExhausted),
            (429, ModelErrorKind::RateLimited),
            (500, ModelErrorKind::Server),
            (503, ModelErrorKind::Overloaded),
            (404, ModelErrorKind::InvalidRequest),
            (502, ModelErrorKind::Server),
        ];
        for (code, kind) in cases {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let error = DeepSeekProvider::classify_status(status, "body");
            assert_eq!(error.kind, kind, "HTTP {code}");
            assert_eq!(error.is_retryable(), kind.is_retryable(), "HTTP {code}");
        }
    }

    #[test]
    fn in_stream_failure_mapping() {
        assert_eq!(
            DeepSeekProvider::in_stream_failure("insufficient_system_resource"),
            Some(ModelErrorKind::Overloaded)
        );
        assert_eq!(
            DeepSeekProvider::in_stream_failure("aborted"),
            Some(ModelErrorKind::Transport)
        );
        assert_eq!(DeepSeekProvider::in_stream_failure("stop"), None);
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let config = DeepSeekConfig::new("sk-live-secret-value");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-live-secret-value"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
        let provider = DeepSeekProvider::new(config).unwrap();
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("sk-live-secret-value"), "{rendered}");
    }

    #[test]
    fn reasoning_effort_from_str_round_trips() {
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            let parsed: ReasoningEffort = effort.as_str().parse().unwrap();
            assert_eq!(parsed, effort);
        }
        for bad in ["medium", "HIGH", "", " high"] {
            assert!(bad.parse::<ReasoningEffort>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reasoning_effort_wire_strings() {
        assert_eq!(ReasoningEffort::None.as_str(), "none");
        assert_eq!(ReasoningEffort::Low.as_str(), "low");
        assert_eq!(ReasoningEffort::High.as_str(), "high");
        assert_eq!(ReasoningEffort::Max.as_str(), "max");
    }

    #[test]
    fn thinking_mode_wire_strings() {
        assert_eq!(ThinkingMode::default(), ThinkingMode::Enabled);
        assert_eq!(ThinkingMode::Enabled.as_str(), "enabled");
        assert_eq!(ThinkingMode::Disabled.as_str(), "disabled");
    }
}
