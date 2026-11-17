//! Error classification for the Messages API: non-success HTTP
//! statuses and `error` events carried inside an otherwise-200
//! SSE stream.

use loom_kernel::model::{ModelError, ModelErrorKind};
use serde_json::Value;

/// Classifies a non-success HTTP status from the Messages API.
///
/// Anthropic error bodies are `{"type": "error", "error": {"type",
/// "message"}}`; the type names are matched where they change the
/// retry decision.
pub(crate) fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
    let kind = match status.as_u16() {
        400 => {
            // Anthropic reports context overflows as 400 with a
            // `context_length_exceeded`-family error type.
            if body.contains("context_length_exceeded") {
                ModelErrorKind::ContextOverflow
            } else {
                ModelErrorKind::InvalidRequest
            }
        }
        401 | 403 => ModelErrorKind::Auth,
        404 => ModelErrorKind::InvalidRequest,
        413 => ModelErrorKind::ContextOverflow,
        429 => ModelErrorKind::RateLimited,
        529 => ModelErrorKind::Overloaded,
        _ => ModelErrorKind::Server,
    };
    let message = extract_error_message(body).map_or_else(
        || format!("HTTP {status} from the Messages API"),
        |detail| format!("HTTP {status}: {detail}"),
    );
    ModelError::new(kind, message).with_http_status(status.as_u16())
}

/// Maps an `error` event carried inside the stream to its kind.
pub(crate) fn classify_stream_error(body: &str) -> ModelError {
    let kind = match extract_error_type(body).as_deref() {
        Some("overloaded_error") => ModelErrorKind::Overloaded,
        Some("rate_limit_error") => ModelErrorKind::RateLimited,
        Some("invalid_request_error" | "not_found_error") => ModelErrorKind::InvalidRequest,
        Some("authentication_error" | "permission_error") => ModelErrorKind::Auth,
        Some("request_too_large") => ModelErrorKind::ContextOverflow,
        // `api_error` and every unrecognized type land on `Server`.
        _ => ModelErrorKind::Server,
    };
    ModelError::new(
        kind,
        extract_error_message(body).map_or_else(
            || "error event inside the stream".to_owned(),
            |detail| format!("error event: {detail}"),
        ),
    )
}

/// Pulls `error.message` out of an Anthropic error body, falling back
/// to a bounded excerpt for non-JSON bodies.
fn extract_error_message(body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body).ok();
    if let Some(message) = parsed.as_ref().and_then(|value| {
        value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
    }) {
        return Some(message.to_owned());
    }
    (!body.trim().is_empty()).then(|| crate::http::body_excerpt(body))
}

/// Pulls `error.type` out of an Anthropic error body.
fn extract_error_type(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .get("type")?
        .as_str()
        .map(str::to_owned)
}
