//! Event projection and log maintenance for the terminal frontend.

use super::*;

pub(super) fn drain_events(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<EventEnvelope>,
    app: &mut App,
) {
    while let Ok(envelope) = receiver.try_recv() {
        app.events_seen = envelope.sequence;
        reduce_event(&envelope, app);
        app.recent_events.push_front(format!(
            "#{:<5} {}",
            envelope.sequence,
            event_name(&envelope.kind)
        ));
        app.recent_events.truncate(5);
    }
}

pub(super) fn reduce_event(envelope: &EventEnvelope, app: &mut App) {
    match &envelope.kind {
        EventKind::TaskStarted => push_log(app, "›", BLUE, "Task accepted", false),
        EventKind::TurnStarted { turn } => {
            app.turns = *turn;
            app.current_turn_input = 0;
            app.current_turn_output = 0;
            app.started_at.get_or_insert_with(Instant::now);
            push_log(app, "●", AMBER, format!("turn {turn}"), false);
            app.current_turn_index = Some(app.timeline.len().saturating_sub(1));
        }
        EventKind::ModelRequestStarted { attempt } => {
            app.requests += 1;
            app.reasoning_chars = 0;
            app.provisional_index = None;
            app.status = format!("Streaming · attempt {attempt}");
            app.status_color = AMBER;
            app.timeline.push_back(LogItem {
                marker: "▸",
                color: AMBER,
                text: format!(
                    "attempt {attempt} / {} · streaming",
                    app.max_attempts.max(1)
                ),
                provisional: true,
                italic: false,
                detail: None,
                expanded: false,
            });
            app.provisional_index = Some(app.timeline.len() - 1);
            push_log(
                app,
                "⁙",
                AMBER,
                format!("request · attempt {attempt}"),
                false,
            );
        }
        EventKind::ReasoningDelta { text } => {
            app.reasoning_chars += text.chars().count();
            let index = match app.provisional_reasoning_index {
                Some(index) => index,
                None => {
                    push_log(app, "▸", VIOLET, "reasoning · provisional", true);
                    let index = app.timeline.len() - 1;
                    app.provisional_reasoning_index = Some(index);
                    index
                }
            };
            if let Some(item) = app.timeline.get_mut(index) {
                item.text.push_str(text);
                item.italic = true;
            }
        }
        EventKind::TextDelta { text } => {
            let index = match app.provisional_index {
                Some(index) => index,
                None => {
                    push_log(app, "┆", VIOLET, "streaming · provisional", true);
                    let index = app.timeline.len() - 1;
                    app.provisional_index = Some(index);
                    index
                }
            };
            if let Some(item) = app.timeline.get_mut(index) {
                item.text.push_str(text);
            }
        }
        EventKind::ModelRetry {
            attempt,
            kind,
            delay_ms,
            response_started,
        } => {
            app.retries += 1;
            app.reasoning_chars = 0;
            discard_provisional(app);
            discard_provisional_reasoning(app);
            app.status = format!("Retrying · attempt {attempt}");
            app.status_color = AMBER;
            app.retries_log.push(RetryRecord {
                attempt: *attempt,
                kind: format!("{kind:?}"),
                delay_ms: *delay_ms,
                response_started: *response_started,
            });
            push_log(
                app,
                "↻",
                AMBER,
                format!("attempt {attempt} failed · {kind:?}"),
                false,
            );
            if let Some(item) = app.timeline.back_mut() {
                item.detail = Some(format!(
                    "{} · discarded · next in {} ms",
                    if *response_started {
                        "response started"
                    } else {
                        "no response"
                    },
                    delay_ms
                ));
            }
        }
        EventKind::ModelCompleted { response } => {
            discard_provisional(app);
            discard_provisional_reasoning(app);
            app.status = "Running".into();
            app.status_color = AMBER;
            app.input_tokens += response.usage.input_tokens;
            app.output_tokens += response.usage.output_tokens;
            app.cached_tokens += response.usage.cached_input_tokens.unwrap_or_default();
            app.current_turn_input += response.usage.input_tokens;
            app.current_turn_output += response.usage.output_tokens;
            if let Some(index) = app.current_turn_index
                && let Some(turn) = app.timeline.get_mut(index)
            {
                turn.detail = Some(format!(
                    "{} in · {} out",
                    ui::compact(app.current_turn_input),
                    ui::compact(app.current_turn_output)
                ));
            }
            let cached = response.usage.cached_input_tokens.unwrap_or_default();
            app.last_cache_pct = if response.usage.input_tokens == 0 {
                "—".into()
            } else {
                format!(
                    "{:.0}%",
                    100.0 * cached as f64 / response.usage.input_tokens as f64
                )
            };
            let mut committed = Vec::new();
            if let Some(reasoning) = response.message.reasoning.as_ref()
                && !reasoning.text.trim().is_empty()
            {
                committed.push(("▸", VIOLET, reasoning.text.clone(), true));
            }
            let body = response.message.content.trim();
            if !body.is_empty() {
                committed.push(("│", IVORY, body.to_owned(), false));
            }
            if let Some(index) = app.provisional_index.take()
                && let Some(item) = app.timeline.get_mut(index)
            {
                item.text = "response committed".into();
                item.provisional = false;
                item.detail = Some(format!(
                    "{} in · {} out · {} cached",
                    ui::compact(response.usage.input_tokens),
                    ui::compact(response.usage.output_tokens),
                    ui::compact(cached)
                ));
            }
            app.provisional_reasoning_index = None;
            for (marker, color, text, italic) in committed {
                push_log(app, marker, color, text, false);
                if let Some(item) = app.timeline.back_mut() {
                    item.italic = italic;
                }
            }
            for call in &response.message.tool_calls {
                push_log(
                    app,
                    "├",
                    BLUE,
                    format!("{} · {}", call.name, *call.arguments),
                    false,
                );
            }
        }
        EventKind::ToolStarted {
            name,
            call_id,
            arguments,
        } => {
            let id = call_id.to_string();
            let index = app.tools.len();
            app.tools.push(ToolRecord {
                name: name.clone(),
                call_id: id.clone(),
                state: ToolRecordState::Running,
                detail: arguments.clone(),
            });
            app.tool_indices.insert(id, index);
            push_log(app, "⋮", BLUE, name.clone(), false);
            if let Some(item) = app.timeline.back_mut() {
                item.detail = Some(arguments.clone());
            }
        }
        EventKind::ToolCompleted {
            name,
            call_id,
            status,
            content_head,
            ..
        } => {
            let (marker, color, outcome, outcome_state) = match status {
                ToolStatus::Success => {
                    app.tools_ok += 1;
                    ("✓", SAGE, "completed", ToolRecordState::Success)
                }
                ToolStatus::Error(_) => {
                    app.tools_failed += 1;
                    ("×", CORAL, "failed", ToolRecordState::Failed)
                }
                ToolStatus::Cancelled { started } => {
                    app.tools_cancelled += 1;
                    if *started {
                        ("△", AMBER, "interrupted", ToolRecordState::Interrupted)
                    } else {
                        ("○", DIM, "not run", ToolRecordState::NotRun)
                    }
                }
                _ => ("·", DIM, "finished", ToolRecordState::Failed),
            };
            let detail = content_head.lines().next().unwrap_or_default();
            let call_key = call_id.to_string();
            if let Some(index) = app.tool_indices.get(&call_key).copied()
                && let Some(tool) = app.tools.get_mut(index)
            {
                tool.state = outcome_state;
                tool.detail = detail.to_owned();
            }
            push_log(
                app,
                marker,
                color,
                format!("{name} · {outcome}  {detail}"),
                false,
            );
            if let Some(item) = app.timeline.back_mut() {
                item.detail = Some(detail.to_owned());
                item.expanded = true;
            }
        }
        EventKind::SteeringApplied { messages, .. } => push_log(
            app,
            "›",
            AMBER,
            format!("applied {messages} steering message(s)"),
            false,
        ),
        EventKind::TaskCompleted => {
            app.status = "Completed".into();
            app.status_color = SAGE;
            if app.current_turn_index.is_some()
                && let Some(turn) = app.current_turn_index.and_then(|i| app.timeline.get_mut(i))
            {
                turn.detail = Some(format!(
                    "{} · {}",
                    turn.detail.as_deref().unwrap_or(""),
                    ui::format_duration(app.elapsed)
                ));
            }
            push_log(app, "✓", SAGE, "Task completed", false);
        }
        EventKind::TaskFailed { reason } => {
            app.status = "Failed".into();
            app.status_color = CORAL;
            if let Some(index) = app.provisional_index.take()
                && let Some(item) = app.timeline.get_mut(index)
            {
                item.text = "attempt failed · provisional output discarded".into();
                item.provisional = false;
                item.color = CORAL;
                item.detail = Some("partial response is not committed".into());
            }
            discard_provisional_reasoning(app);
            push_log(app, "×", CORAL, format!("Task failed · {reason}"), false);
        }
        EventKind::TaskCancelled => {
            app.status = "Cancelled".into();
            app.status_color = AMBER;
            app.last_interrupt = Some(Instant::now());
            if let Some(index) = app.provisional_index.take()
                && let Some(item) = app.timeline.get_mut(index)
            {
                item.text = "attempt interrupted · provisional output discarded".into();
                item.provisional = false;
                item.color = AMBER;
                item.detail = Some("partial response is not committed".into());
            }
            discard_provisional_reasoning(app);
            for tool in &mut app.tools {
                if tool.state == ToolRecordState::Running {
                    tool.state = ToolRecordState::Interrupted;
                    app.tools_cancelled += 1;
                }
            }
            if let Some(index) = app.current_turn_index.take()
                && let Some(turn) = app.timeline.get_mut(index)
            {
                turn.detail = Some(format!(
                    "partial · {} in · {} out · {}",
                    ui::compact(app.current_turn_input),
                    ui::compact(app.current_turn_output),
                    ui::format_duration(app.elapsed)
                ));
            }
            push_log(app, "⊘", AMBER, "Task cancelled", false);
        }
        EventKind::TaskPaused => {
            app.status = "Paused".into();
            app.status_color = AMBER;
            push_log(app, "Ⅱ", AMBER, "Paused at turn boundary", false);
        }
        EventKind::HarnessNotice { category, detail } => {
            push_log(app, "!", AMBER, format!("{category} · {detail}"), false)
        }
        _ => {}
    }
}

pub(super) fn push_log(
    app: &mut App,
    marker: &'static str,
    color: Color,
    text: impl Into<String>,
    provisional: bool,
) {
    let marker = display_marker(marker, std::env::var_os("LOOM_ASCII").is_some());
    app.timeline.push_back(LogItem {
        marker,
        color,
        text: text.into(),
        provisional,
        italic: false,
        detail: None,
        expanded: false,
    });
    while app.timeline.len() > EVENT_LOG_LIMIT {
        app.timeline.pop_front();
        app.provisional_index = app.provisional_index.and_then(|index| index.checked_sub(1));
        app.provisional_reasoning_index = app
            .provisional_reasoning_index
            .and_then(|index| index.checked_sub(1));
        app.current_turn_index = app
            .current_turn_index
            .and_then(|index| index.checked_sub(1));
    }
}

pub(super) fn display_marker(marker: &'static str, ascii: bool) -> &'static str {
    if ascii {
        match marker {
            "›" | "▸" => ">",
            "●" => "*",
            "⁙" => "!",
            "┆" | "│" | "├" | "⋮" => "|",
            "↻" => "~",
            "✓" => "+",
            "×" => "x",
            "⊘" => "/",
            "Ⅱ" => "||",
            other => other,
        }
    } else {
        marker
    }
}

pub(super) fn discard_provisional_reasoning(app: &mut App) {
    if let Some(index) = app.provisional_reasoning_index.take()
        && index < app.timeline.len()
    {
        app.timeline.remove(index);
        if let Some(text_index) = app.provisional_index {
            app.provisional_index = text_index.checked_sub(usize::from(text_index > index));
        }
    }
}

pub(super) fn discard_provisional(app: &mut App) {
    let mut removed_index = None;
    if let Some(index) = app.provisional_index.take()
        && index < app.timeline.len()
    {
        app.timeline.remove(index);
        removed_index = Some(index);
    }
    if let Some(removed) = removed_index
        && let Some(reasoning_index) = app.provisional_reasoning_index
    {
        app.provisional_reasoning_index =
            reasoning_index.checked_sub(usize::from(reasoning_index > removed));
    }
}

pub(super) fn event_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::TaskStarted => "TaskStarted",
        EventKind::TurnStarted { .. } => "TurnStarted",
        EventKind::ModelRequestStarted { .. } => "ModelRequestStarted",
        EventKind::TextDelta { .. } => "TextDelta",
        EventKind::ReasoningDelta { .. } => "ReasoningDelta",
        EventKind::ModelRetry { .. } => "ModelRetry",
        EventKind::ModelCompleted { .. } => "ModelCompleted",
        EventKind::ToolStarted { .. } => "ToolStarted",
        EventKind::ToolCompleted { .. } => "ToolCompleted",
        EventKind::TaskCompleted => "TaskCompleted",
        EventKind::TaskFailed { .. } => "TaskFailed",
        EventKind::TaskCancelled => "TaskCancelled",
        _ => "Event",
    }
}
