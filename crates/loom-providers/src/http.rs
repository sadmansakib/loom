//! Shared HTTP plumbing for the OpenAI-compatible adapters: liveness
//! timeouts, bounded error-body reads, and `Retry-After` parsing.
//!
//! Everything here is adapter-agnostic. Providers own their status
//! classification and in-stream failure mapping; this module only
//! supplies the mechanics that must not diverge between them.

use std::time::Duration;

use futures::StreamExt;

/// Liveness timeouts for one streamed request.
///
/// Expiry of any of these is a retryable [`ModelErrorKind::Transport`];
/// there is deliberately no total deadline (the cancellation token
/// bounds total time).
///
/// [`ModelErrorKind::Transport`]: loom_kernel::model::ModelErrorKind::Transport
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
    /// Maximum gap between *dispatched SSE events* — real model
    /// progress. Unlike [`idle`](Self::idle), a chunk that dispatches no
    /// event (a keep-alive comment, or an empty `data:` heartbeat) does
    /// not reset this: a provider that pings without ever producing
    /// content is stalled, not alive. Closes the hang where a
    /// keep-alive-only stream reset the byte-level idle window forever
    /// (observed as 30-minute eval watchdog holes, 2026-10-16).
    pub stall: Duration,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            header: Duration::from_secs(300),
            idle: Duration::from_secs(300),
            stall: Duration::from_secs(120),
        }
    }
}

/// Cap on bytes read from an error-response body: enough for the
/// classification excerpt, small enough that a hostile server cannot
/// balloon memory.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// Builds a connection-pooled HTTP client honouring the connect timeout.
///
/// Fails only when the TLS backend cannot be initialized (e.g. missing
/// system roots); every other aspect of construction is infallible
/// configuration.
pub(crate) fn build_client(timeouts: &TimeoutConfig) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .user_agent(concat!("loom/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Reads an error-response body for classification: bounded by
/// [`MAX_ERROR_BODY_BYTES`] and by the idle timeout — which here bounds
/// the **whole** error-body read, not each gap between chunks (fine at
/// a 16 KiB cap). On timeout the body is unknowable, so the caller
/// classifies from the HTTP status alone: the error kind must follow
/// the status, never the stall, or non-retryable statuses would turn
/// into retryable ones. The placeholder notes the missing excerpt.
pub(crate) async fn read_error_body(response: reqwest::Response, idle: Duration) -> String {
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

/// Parses a `Retry-After` header (seconds form) if present.
pub(crate) fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let raw = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let seconds: u64 = std::str::from_utf8(raw.as_bytes())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds))
}

/// Bounded excerpt of an error body for error messages and logs.
pub(crate) fn body_excerpt(body: &str) -> String {
    body.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeouts_are_liveness_scale() {
        let timeouts = TimeoutConfig::default();
        assert_eq!(timeouts.connect, Duration::from_secs(30));
        assert_eq!(timeouts.header, Duration::from_secs(300));
        assert_eq!(timeouts.idle, Duration::from_secs(300));
        assert_eq!(timeouts.stall, Duration::from_secs(120));
    }

    #[test]
    fn body_excerpt_is_capped() {
        let long = "x".repeat(1000);
        assert_eq!(body_excerpt(&long).chars().count(), 500);
        assert_eq!(body_excerpt("short"), "short");
    }
}
