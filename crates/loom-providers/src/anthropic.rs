//! Anthropic Messages API provider adapter (roadmap item 14).
//!
//! Pressures the `ModelProvider` boundary with a second wire family:
//! header auth (`x-api-key` + `anthropic-version`) instead of bearer,
//! a top-level `system` parameter, typed content blocks, and an SSE
//! stream that terminates with a `message_stop` event instead of the
//! OpenAI `[DONE]` sentinel.
//!
//! This is deliberately **not** built on [`crate::wire_stream`]: that
//! machine's accumulator is the OpenAI chunk shape, and refactoring the
//! delicate OpenAI paths (unterminated-`[DONE]` tolerance, mid-stream
//! error objects) without a funded Anthropic key to re-verify live
//! behaviour risks drift in the working providers for ~100 lines of
//! duplication. The HTTP/SSE plumbing (timeouts, error-body reads,
//! `SseDecoder`, idle handling) is shared; only the event assembly and
//! termination differ.
//!
//! Capability discovery deviates from the OpenRouter catalog pattern on
//! purpose: Anthropic's models endpoint exposes no capability fields,
//! so a static table stands in until live verification is possible.
//!
//! No live key exists for this provider — verification is offline
//! (unit + wiremock tests).

use std::collections::VecDeque;

use futures::StreamExt;
use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ModelStream,
};

use crate::anthropic_wire::{
    ANTHROPIC_VERSION, AnthropicAssembler, build_request_body, classify_status,
};
use crate::http::{TimeoutConfig, build_client};
use crate::secret::SecretString;
use crate::sse::SseDecoder;

/// Default Messages API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Configuration for [`AnthropicProvider`].
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    base_url: String,
    api_key: SecretString,
    timeouts: TimeoutConfig,
}

impl AnthropicConfig {
    /// Creates a config with the production base URL and timeouts.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: SecretString::new(api_key.into()),
            timeouts: TimeoutConfig::default(),
        }
    }

    /// Overrides the base URL (wiremock tests, proxies).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Overrides the liveness timeouts.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        self
    }
}

/// The Messages API adapter.
#[derive(Clone)]
pub struct AnthropicProvider {
    config: std::sync::Arc<AnthropicConfig>,
    client: reqwest::Client,
}

impl AnthropicProvider {
    /// Creates the provider, building its HTTP client.
    ///
    /// Errors only when the client cannot be built (TLS backend
    /// failure) — same contract as the other adapters.
    pub fn new(config: AnthropicConfig) -> Result<Self, reqwest::Error> {
        let client = build_client(&config.timeouts)?;
        Ok(Self {
            config: std::sync::Arc::new(config),
            client,
        })
    }
}

impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        capabilities(model)
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        let state = State::Send {
            provider: self.clone(),
            body: build_request_body(request),
        };
        Box::pin(futures::stream::unfold(state, step).filter_map(
            |item: Result<Option<ModelEvent>, ModelError>| async move {
                match item {
                    Ok(Some(event)) => Some(Ok(event)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                }
            },
        ))
    }
}

/// Resolves capabilities from the static table.
///
/// Anthropic's models endpoint carries no capability fields, so there
/// is nothing to reduce a catalog from (the deliberate deviation from
/// the OpenRouter pattern). Unknown models resolve to `None` — the
/// core fails loud rather than guessing a window.
fn capabilities(model: &str) -> Option<ModelCapabilities> {
    let (context, output, reasoning) = match model {
        // Long-context Claude models; output caps per the model cards.
        "claude-opus-4-1" => (200_000, 32_000, true),
        "claude-sonnet-4-5" => (200_000, 64_000, true),
        "claude-haiku-4-5" => (200_000, 32_000, false),
        _ => return None,
    };
    Some(
        ModelCapabilities::new(context)
            .with_max_output_tokens(output)
            .with_tool_calls(true)
            .with_parallel_tool_calls(true)
            .with_reasoning(reasoning),
    )
}

/// The unfold state machine backing the stream.
enum State {
    /// Request built, not yet sent.
    Send {
        provider: AnthropicProvider,
        body: serde_json::Value,
    },
    /// Headers received; body streaming. `pending` holds display
    /// deltas from the last ingested event; `saw_stop` records
    /// `message_stop`.
    Body {
        provider: AnthropicProvider,
        body_stream: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
        decoder: SseDecoder,
        assembler: Box<AnthropicAssembler>,
        pending: VecDeque<ModelEvent>,
        saw_stop: bool,
    },
    /// Terminal.
    Done,
}

/// One unfold step — same shape as [`crate::wire_stream`]'s machine,
/// minus the OpenAI `[DONE]` sentinel handling.
async fn step(state: State) -> Option<(Result<Option<ModelEvent>, ModelError>, State)> {
    match state {
        State::Send { provider, body } => {
            let timeouts = provider.config.timeouts;
            let send = provider
                .client
                .post(format!("{}/v1/messages", provider.config.base_url))
                .header("x-api-key", provider.config.api_key.expose())
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body)
                .send();
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
                let mut error = classify_status(status, &body);
                if let Some(delay) = retry_after {
                    error = error.with_retry_after(delay);
                }
                return Some((Err(error), State::Done));
            }
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
                    assembler: Box::default(),
                    pending: VecDeque::new(),
                    saw_stop: false,
                },
            ))
        }
        State::Body {
            provider,
            mut body_stream,
            mut decoder,
            mut assembler,
            mut pending,
            mut saw_stop,
        } => {
            if let Some(event) = pending.pop_front() {
                return Some((
                    Ok(Some(event)),
                    State::Body {
                        provider,
                        body_stream,
                        decoder,
                        assembler,
                        pending,
                        saw_stop,
                    },
                ));
            }
            if saw_stop {
                let result: Result<ModelResponse, ModelError> = assembler.finalize();
                return Some((
                    result.map(|response| Some(ModelEvent::Completed(response))),
                    State::Done,
                ));
            }
            let idle = provider.config.timeouts.idle;
            match tokio::time::timeout(idle, body_stream.next()).await {
                Err(_) => Some((
                    Err(ModelError::new(
                        ModelErrorKind::Transport,
                        "idle timeout: no SSE event within the configured window",
                    )),
                    State::Done,
                )),
                Ok(None) => {
                    // The Messages API terminates with a dispatched
                    // `message_stop`; EOF without it is a truncated
                    // response — retryable, never silently accepted.
                    Some((
                        Err(ModelError::new(
                            ModelErrorKind::Transport,
                            "stream ended without message_stop",
                        )),
                        State::Done,
                    ))
                }
                Ok(Some(Err(error))) => Some((
                    Err(ModelError::new(
                        ModelErrorKind::Transport,
                        format!("body stream failed: {error}"),
                    )),
                    State::Done,
                )),
                Ok(Some(Ok(chunk))) => {
                    let events = match decoder.push_bytes(&chunk) {
                        Ok(events) => events,
                        Err(error) => {
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
                        match assembler.ingest(&event.data) {
                            Ok((mut deltas, stop)) => {
                                pending.extend(deltas.drain(..));
                                if stop {
                                    saw_stop = true;
                                }
                            }
                            Err(error) => {
                                return Some((Err(error), State::Done));
                            }
                        }
                    }
                    Some((
                        Ok(None),
                        State::Body {
                            provider,
                            body_stream,
                            decoder,
                            assembler,
                            pending,
                            saw_stop,
                        },
                    ))
                }
            }
        }
        State::Done => None,
    }
}
