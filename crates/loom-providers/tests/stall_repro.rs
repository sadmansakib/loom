//! Stall repro: an SSE body that starts and then goes silent must trip
//! the idle timeout and surface a retryable Transport error, never hang
//! the runtime. Found live against NVIDIA NIM (z-ai/glm-5.3): the
//! stream delivered ~135 KB, then zero bytes in either direction for
//! 20+ minutes with the connection held open — the parked runtime had
//! no timer armed, so the 300 s idle window at `wire_stream` never
//! fired. This file pins the client half against a raw TCP server that
//! reproduces the exact server behavior (one event, then silence, no
//! EOF), with no tokens spent.

use futures::StreamExt;
use loom_kernel::model::{ModelError, ModelErrorKind, ModelProvider, ModelRequest};
use loom_providers::{NvidiaConfig, NvidiaProvider, TimeoutConfig};
use std::future::pending;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_at(base_url: &str, idle: Duration) -> NvidiaProvider {
    NvidiaProvider::new(
        NvidiaConfig::new("test-key")
            .with_base_url(base_url)
            .with_timeouts(TimeoutConfig {
                idle,
                ..TimeoutConfig::default()
            }),
    )
    .unwrap()
}

fn request() -> ModelRequest {
    // Heavy tasks (B7) send large contexts; a small one reproduces the
    // transport behavior identically — the stall is body-level.
    ModelRequest {
        model: "z-ai/glm-5.3".into(),
        messages: vec![loom_kernel::model::Message::User { text: "hi".into() }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        stable_prefix_messages: None,
    }
}

/// Serve one SSE event, then hold the connection open in silence
/// forever — the exact NIM stall signature.
#[expect(
    clippy::unwrap_used,
    reason = "test-only socket setup; failures should panic"
)]
async fn stalling_server(listener: tokio::net::TcpListener) {
    let (mut sock, _) = listener.accept().await.unwrap();
    let mut buf = [0u8; 8192];
    let _ = sock.read(&mut buf).await; // request head (may split; enough)
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
    sock.write_all(head.as_bytes()).await.unwrap();
    sock.write_all(b"data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\"}\n\n")
        .await
        .unwrap();
    let _ = sock.flush().await;
    pending::<()>().await;
}

#[expect(
    clippy::unwrap_used,
    reason = "test-only bind; panic is the right failure"
)]
async fn bound_listener() -> (tokio::net::TcpListener, std::net::SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}
#[tokio::test]
async fn stalled_sse_stream_trips_idle_timeout() {
    let (listener, addr) = bound_listener().await;
    tokio::spawn(stalling_server(listener));
    let provider = provider_at(&format!("http://{addr}"), Duration::from_secs(1));
    let request = request();
    let mut stream = provider.stream(&request);

    // Outer bound: the whole point is that this cannot hang. Generous
    // 10 s vs the 1 s idle window leaves no room for scheduling noise.
    let outcome = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;

    let event = outcome.expect("stream hung: idle timeout never fired");
    match event {
        Some(Err(ModelError {
            kind: ModelErrorKind::Transport,
            ..
        })) => {}
        other => panic!("expected Transport error, got {other:?}"),
    }
}
