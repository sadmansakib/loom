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
//! The table is the whole story for model support: an id missing from
//! [`capabilities`] resolves to `None`, and the engine turns that into
//! `UnknownModel` at construction time — a wrong window is never
//! guessed. Keep it current (verified against
//! `platform.claude.com/docs/en/models/overview`, 2026-10-09).
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

/// The provider name the engine matches on (`--provider anthropic`).
pub const PROVIDER_NAME: &str = "anthropic";

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
        PROVIDER_NAME
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
///
/// Every entry is a pinned Claude API id, so dateless aliases
/// (`claude-opus-5`) do *not* resolve. Windows are per the model cards:
/// the 4.5 generation and older bill 200K, the 4.7-and-later
/// tokenizer models bill 1M with a 128K output cap. `reasoning` is
/// `true` for everything here — adaptive thinking ships on the whole
/// current lineup, Haiku included.
fn capabilities(model: &str) -> Option<ModelCapabilities> {
    let (context, output, reasoning) = match model {
        // Claude 5.5 family (2026): 1M context, 128K output, adaptive
        // thinking. Opus 5.5 and Fable 5.1 additionally always think
        // (thinking cannot be disabled) and reject forced tool use.
        "claude-fable-5-1" => (1_000_000, 128_000, true),
        "claude-opus-5-5" => (1_000_000, 128_000, true),
        "claude-sonnet-5-5" => (1_000_000, 128_000, true),
        "claude-haiku-5-5" => (1_000_000, 128_000, true),
        // Claude 4.x long-context models; output caps per the model cards.
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
            let headers = vec![
                (
                    reqwest::header::HeaderName::from_static("x-api-key"),
                    provider.config.api_key.expose().to_owned(),
                ),
                (
                    reqwest::header::HeaderName::from_static("anthropic-version"),
                    ANTHROPIC_VERSION.to_owned(),
                ),
            ];
            let url = format!("{}/v1/messages", provider.config.base_url);
            let response = match crate::wire_stream::send_request(
                &provider.client,
                url,
                timeouts,
                headers,
                body,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => return Some((Err(error), State::Done)),
            };
            if !response.status().is_success() {
                return Some((
                    Err(crate::wire_stream::non_success_error(
                        response,
                        timeouts.idle,
                        classify_status,
                    )
                    .await),
                    State::Done,
                ));
            }
            let body_stream = match crate::wire_stream::sse_body(response) {
                Ok(body_stream) => body_stream,
                Err(error) => return Some((Err(error), State::Done)),
            };
            Some((
                Ok(None),
                State::Body {
                    provider,
                    body_stream,
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

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::model::ModelProvider;

    #[test]
    fn provider_name_matches_the_engine_arm() {
        assert_eq!(PROVIDER_NAME, "anthropic");
        let provider = AnthropicProvider::new(AnthropicConfig::new("k")).unwrap();
        assert_eq!(provider.name(), PROVIDER_NAME);
    }

    #[test]
    fn the_55_family_resolves_with_one_million_token_windows() {
        for model in [
            "claude-fable-5-1",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
        ] {
            let caps = capabilities(model).unwrap_or_else(|| panic!("{model} must resolve"));
            assert_eq!(caps.max_context_tokens, 1_000_000, "{model}");
            assert_eq!(caps.max_output_tokens, Some(128_000), "{model}");
            assert!(caps.reasoning, "{model}: adaptive thinking");
            assert!(caps.tool_calls && caps.parallel_tool_calls, "{model}");
        }
    }

    #[test]
    fn legacy_models_keep_their_windows() {
        assert_eq!(
            capabilities("claude-opus-4-1").unwrap().max_context_tokens,
            200_000
        );
        assert_eq!(
            capabilities("claude-sonnet-4-5").unwrap().max_output_tokens,
            Some(64_000)
        );
        assert!(!capabilities("claude-haiku-4-5").unwrap().reasoning);
    }

    #[test]
    fn unknown_and_dateless_ids_fail_loud() {
        // Dateless aliases float to new snapshots with different windows;
        // resolving them would be a guess.
        for model in ["claude-opus-5", "claude-sonnet", "gpt-5", ""] {
            assert!(capabilities(model).is_none(), "{model:?} must not resolve");
        }
    }
}
