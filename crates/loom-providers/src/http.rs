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

impl TimeoutConfig {
    /// The idle window in whole milliseconds, as reported on the wire.
    #[must_use]
    pub fn idle_millis(&self) -> u64 {
        u64::try_from(self.idle.as_millis()).unwrap_or(u64::MAX)
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

/// Parses a `Retry-After` header if present, in either RFC 9110 form:
/// delay-seconds (`Retry-After: 120`) or HTTP-date
/// (`Retry-After: Sun, 06 Nov 1994 08:49:37 GMT` — how long from
/// *now* until that instant, clamped so a date already in the past
/// means "now").
pub(crate) fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let raw = response.headers().get(reqwest::header::RETRY_AFTER)?;
    retry_after_text(std::str::from_utf8(raw.as_bytes()).ok()?)
}

/// The parsed form of [`retry_after`], shared with tests.
pub(crate) fn retry_after_text(text: &str) -> Option<Duration> {
    let text = text.trim();
    if let Ok(seconds) = text.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let target = httpdate::parse_http_date(text).ok()?;
    let now = std::time::SystemTime::now();
    match now.duration_since(target) {
        // The date already passed: retry at once, never a negative delay.
        Ok(_) => Some(Duration::ZERO),
        Err(_) => Some(target.duration_since(now).ok()?),
    }
}

/// Whether a base URL is plain HTTP to a non-local host: the API key
/// and every conversation byte would travel unencrypted. Loopback and
/// link-local targets are the legitimate local-gateway case (a dev
/// proxy, an llama.cpp server) and do not count.
pub(crate) fn plain_http_to_non_local(base_url: &str) -> bool {
    let lower = base_url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("http://") else {
        return false;
    };
    let host_port = rest.split('/').next().unwrap_or_default();
    let host = host_port
        .strip_prefix('[')
        .and_then(|bracketed| bracketed.split(']').next())
        .unwrap_or_else(|| host_port.split(':').next().unwrap_or_default());
    // The allow is a false positive: `host` was lowercased with the
    // URL above, so these suffix checks are already case-insensitive.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    let suffix_local =
        host.ends_with(".localhost") || host.ends_with(".local") || host.ends_with(".internal");
    let local = matches!(host, "localhost" | "::1" | "0.0.0.0" | "" | "[::1]")
        || host.starts_with("127.")
        || host.starts_with("169.254.")
        || suffix_local;
    !local
}

/// Warns when a configured base URL is plain HTTP to a non-local
/// host: the API key and every conversation byte travel unencrypted.
pub(crate) fn warn_plain_http(base_url: &str) {
    if plain_http_to_non_local(base_url) {
        tracing::warn!(
            "base URL {base_url} uses plain HTTP to a non-local host — \
             the API key and the conversation travel unencrypted; \
             prefer https://"
        );
    }
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

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        use std::time::Duration;

        // Delay-seconds form.
        assert_eq!(retry_after_text("120"), Some(Duration::from_secs(120)));
        assert_eq!(retry_after_text(" 0 "), Some(Duration::from_secs(0)));
        assert_eq!(retry_after_text(""), None);
        assert_eq!(retry_after_text("soon"), None);

        // HTTP-date form: a date two hours out waits about two hours.
        let target = std::time::SystemTime::now() + Duration::from_secs(7_200);
        let date = httpdate::fmt_http_date(target);
        let parsed = retry_after_text(&date).expect("date form parses");
        assert!(
            parsed >= Duration::from_secs(7_195) && parsed <= Duration::from_secs(7_200),
            "about two hours, got {parsed:?}"
        );

        // A date already in the past means "now", not a negative wait.
        let past = httpdate::fmt_http_date(std::time::SystemTime::now() - Duration::from_secs(60));
        assert_eq!(retry_after_text(&past), Some(Duration::from_secs(0)));
    }

    #[test]
    fn plain_http_warning_targets_only_non_local_hosts() {
        for url in [
            "http://gateway.example.com/v1",
            "http://10.0.0.5/v1",
            "HTTP://EXAMPLE.COM/v1",
        ] {
            assert!(plain_http_to_non_local(url), "{url} should warn");
        }
        for url in [
            "http://localhost:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:9000/v1",
            "http://my-mac.local/v1",
            "http://proxy.localhost/v1",
            "http://169.254.10.5/v1",
            "http://gateway.internal/v1",
            "https://api.example.com",
            "file:///none",
            "",
        ] {
            assert!(!plain_http_to_non_local(url), "{url} should stay quiet");
        }
    }
}
