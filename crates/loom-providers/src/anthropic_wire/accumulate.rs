//! Stream accumulation: SSE event ingest, block assembly, and the
//! terminal response build.

use loom_kernel::model::{
    ModelError, ModelErrorKind, ModelEvent, ModelResponse, StopReason, Usage,
};
use loom_kernel::transcript::AssistantMessage;
use serde_json::{Value, json};

use super::classify_stream_error;

/// Assembles a Messages API SSE stream into [`ModelEvent`]s.
///
/// Feed each dispatched SSE event's `data` JSON to [`Self::ingest`];
/// the final [`ModelEvent::Completed`] comes from [`Self::finalize`],
/// called when `message_stop` has been seen.
#[derive(Debug, Default)]
pub(crate) struct AnthropicAssembler {
    /// Accumulated text of the response, in block order.
    text: String,
    /// Blocks under assembly: text spans and tool calls.
    blocks: Vec<Block>,
    /// The block currently receiving deltas (SSE guarantees one open
    /// content block at a time).
    current: Option<Block>,
    /// Reasoning text, if the model emitted thinking deltas.
    reasoning: Option<String>,
    /// Stop reason from `message_delta`, if it arrived.
    stop_reason: Option<StopReason>,
    /// Usage from `message_start` (input side).
    input_usage: u64,
    cached_input: Option<u64>,
    /// Cache-write tokens from `message_start`, if reported (billed
    /// above the base input rate on Anthropic).
    cache_creation: Option<u64>,
    /// Thinking tokens from `message_delta` usage details, if reported
    /// (already counted in the output total — an attribution split).
    thinking: Option<u64>,
    /// Usage from `message_delta` (output side, cumulative).
    output_usage: Option<u64>,
}

/// One content block under assembly.
#[derive(Debug)]
enum Block {
    /// A text block: assembled span.
    Text,
    /// A tool call: id, name, and the raw accumulated argument JSON.
    Tool {
        id: String,
        name: String,
        json: String,
    },
}

impl AnthropicAssembler {
    /// Ingests one SSE `data` payload; returns display deltas to emit,
    /// whether this event was the terminal `message_stop`, and whether
    /// it counts as content progress for the stall window (false only
    /// for `ping`: pings keep the byte flow alive but are not content).
    pub(crate) fn ingest(
        &mut self,
        data: &str,
    ) -> Result<(Vec<ModelEvent>, bool, bool), ModelError> {
        let event: Value = serde_json::from_str(data).map_err(|error| {
            ModelError::new(
                ModelErrorKind::Protocol,
                format!("malformed Messages API event: {error}"),
            )
        })?;
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message_start" => {
                self.ingest_message_start(&event);
                Ok((Vec::new(), false, true))
            }
            "content_block_start" => {
                // Close any open block (a stop event should have done
                // it; tolerate its absence rather than lose content).
                self.close_current();
                let block = &event["content_block"];
                self.current = Some(match block["type"].as_str() {
                    Some("tool_use") => Block::Tool {
                        id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name: block["name"].as_str().unwrap_or_default().to_owned(),
                        json: String::new(),
                    },
                    _ => Block::Text,
                });
                Ok((Vec::new(), false, true))
            }
            "content_block_delta" => Ok((self.ingest_block_delta(&event["delta"]), false, true)),
            "content_block_stop" => {
                self.close_current();
                Ok((Vec::new(), false, true))
            }
            "message_delta" => {
                self.ingest_message_delta(&event);
                Ok((Vec::new(), false, true))
            }
            "message_stop" => Ok((Vec::new(), true, true)),
            "ping" => Ok((Vec::new(), false, false)),
            "error" => Err(classify_stream_error(data)),
            other => Err(ModelError::new(
                ModelErrorKind::Protocol,
                format!("unknown Messages API event type {other:?}"),
            )),
        }
    }

    /// `message_start`: the input-side usage snapshot. Anthropic's
    /// `input_tokens` already *excludes* both cache reads and cache
    /// writes, so the full prompt — the kernel's `input_tokens`
    /// meaning, matching the OpenAI wire's `prompt_tokens` — is the
    /// sum of all three counters.
    fn ingest_message_start(&mut self, event: &Value) {
        let usage = &event["message"]["usage"];
        let input = usage["input_tokens"].as_u64().unwrap_or(0);
        let cached = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let creation = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        self.input_usage = input.saturating_add(cached).saturating_add(creation);
        self.cached_input = usage["cache_read_input_tokens"].as_u64();
        self.cache_creation = usage["cache_creation_input_tokens"].as_u64();
    }

    /// `content_block_delta`: appends the fragment to whichever buffer
    /// the open block owns and returns the display delta it produced
    /// (text and thinking fragments surface; JSON fragments accumulate
    /// silently).
    fn ingest_block_delta(&mut self, delta: &Value) -> Vec<ModelEvent> {
        match delta["type"].as_str() {
            Some("text_delta") => {
                let text = delta["text"].as_str().unwrap_or_default();
                self.text.push_str(text);
                vec![ModelEvent::TextDelta(text.to_owned())]
            }
            Some("input_json_delta") => {
                if let Some(Block::Tool { json, .. }) = self.current.as_mut() {
                    json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                }
                Vec::new()
            }
            Some("thinking_delta") => {
                let text = delta["thinking"].as_str().unwrap_or_default();
                self.reasoning
                    .get_or_insert_with(String::new)
                    .push_str(text);
                vec![ModelEvent::ReasoningDelta(text.to_owned())]
            }
            _ => Vec::new(),
        }
    }

    /// `message_delta`: the output-side usage snapshot and the stop
    /// reason, either of which the event may omit.
    fn ingest_message_delta(&mut self, event: &Value) {
        if let Some(reason) = event["delta"]["stop_reason"].as_str() {
            self.stop_reason = Some(map_stop_reason(reason));
        }
        if let Some(output) = event["usage"]["output_tokens"].as_u64() {
            self.output_usage = Some(output);
        }
        if let Some(thinking) = event["usage"]["output_tokens_details"]["thinking_tokens"].as_u64()
        {
            self.thinking = Some(thinking);
        }
    }

    /// Seals the open block, if any, into the finished list.
    fn close_current(&mut self) {
        if let Some(block) = self.current.take() {
            self.blocks.push(block);
        }
    }

    /// Builds the terminal response. Structural normalization mirrors
    /// the OpenAI wire's contract: `ToolCalls` without calls and calls
    /// with malformed argument JSON are salvaged (the engine refuses to
    /// dispatch them and reports the parse failure back to the model); a
    /// `MaxTokens` response discards partially-assembled calls.
    pub(crate) fn finalize(mut self) -> Result<ModelResponse, ModelError> {
        self.close_current();
        let wire_reason = self.stop_reason.unwrap_or(StopReason::EndTurn);
        // Truncation discards partial calls before parsing them: a
        // max_tokens stream can cut mid-JSON (mirrors the OpenAI
        // `length` + calls rule).
        if wire_reason == StopReason::MaxTokens {
            return Ok(ModelResponse {
                message: AssistantMessage {
                    content: self.text,
                    reasoning: self
                        .reasoning
                        .map(|text| loom_kernel::transcript::ReasoningTrace {
                            text,
                            provider_data: None,
                        }),
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::MaxTokens,
                },
                usage: Usage {
                    input_tokens: self.input_usage,
                    output_tokens: self.output_usage.unwrap_or(0),
                    cached_input_tokens: self.cached_input,
                    thinking_tokens: self.thinking,
                    cache_creation_input_tokens: self.cache_creation,
                    cost_usd: None,
                },
            });
        }
        let mut tool_calls = Vec::new();
        for block in &self.blocks {
            if let Block::Tool { id, name, json } = block {
                // Salvage instead of failing: unparseable or non-object
                // arguments become a placeholder object under the
                // reserved key (the engine refuses to dispatch it and
                // feeds the excerpt back so the model can reissue the
                // call) — one malformed block no longer kills the turn.
                // The expect is a proven invariant: an empty JSON
                // object is always valid ToolArguments.
                #[allow(clippy::expect_used)]
                let arguments = if json.trim().is_empty() {
                    loom_kernel::transcript::ToolArguments::try_from(json!({}))
                        .expect("the empty object is always valid ToolArguments")
                } else {
                    let parsed = serde_json::from_str::<Value>(json)
                        .map_err(|_| ())
                        .and_then(|value| {
                            loom_kernel::transcript::ToolArguments::try_from(value).map_err(|_| ())
                        });
                    match parsed {
                        Ok(arguments) => arguments,
                        Err(_) => loom_kernel::transcript::ToolArguments::malformed(json),
                    }
                };
                let id =
                    loom_kernel::transcript::ToolCallId::try_from(id.clone()).map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::Protocol,
                            format!("empty tool_use id for {name:?}"),
                        )
                    })?;
                tool_calls.push(loom_kernel::transcript::ToolCall {
                    id,
                    name: name.clone(),
                    arguments,
                });
            }
        }

        // Normalize: any response that produced calls is a ToolCalls
        // response regardless of the reported reason (mirrors the
        // OpenAI `stop` + calls rule).
        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolCalls
        } else if wire_reason == StopReason::ToolCalls {
            return Err(ModelError::new(
                ModelErrorKind::Protocol,
                "stop reason tool_use with zero tool calls",
            ));
        } else {
            wire_reason
        };

        let reasoning = self
            .reasoning
            .map(|text| loom_kernel::transcript::ReasoningTrace {
                text,
                provider_data: None,
            });
        Ok(ModelResponse {
            message: AssistantMessage {
                content: self.text,
                reasoning,
                tool_calls,
                stop_reason,
            },
            usage: Usage {
                input_tokens: self.input_usage,
                output_tokens: self.output_usage.unwrap_or(0),
                cached_input_tokens: self.cached_input,
                thinking_tokens: self.thinking,
                cache_creation_input_tokens: self.cache_creation,
                cost_usd: None,
            },
        })
    }
}

/// Maps a Messages API stop reason to the neutral one.
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolCalls,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::ContentFilter,
        other => {
            tracing::debug!(target: "model", stop_reason_raw = other, "unmapped stop reason");
            StopReason::EndTurn
        }
    }
}
