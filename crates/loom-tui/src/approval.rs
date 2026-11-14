//! Write approval: the engine asks, the operator answers on `y/a/n/e`.
//!
//! The write tool calls a [`WriteApproval`] hook from its own task. The hook
//! sends a [`Request`] to the live loop and waits on a oneshot reply. The loop
//! holds at most one [`Pending`] request and resolves it from a key press.

use loom_core::tools::{WriteApproval, WriteDecision};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// One write waiting for the operator.
#[derive(Debug)]
pub struct Request {
    /// Workspace-relative path the write would touch.
    pub path: String,
    /// Proposed file body, shown in the preview.
    pub content: String,
    /// Where the operator's answer goes.
    pub reply: oneshot::Sender<WriteDecision>,
}

/// Builds the engine hook that forwards each write to the live loop.
pub fn hook(
    workspace: std::path::PathBuf,
    requests: mpsc::UnboundedSender<Request>,
) -> WriteApproval {
    Arc::new(move |path, content| {
        let requests = requests.clone();
        let workspace = workspace.clone();
        Box::pin(async move {
            // "Never ask again" was recorded in the trust store by an earlier answer.
            if loom_core::tools::trust::writes_allowed(&workspace) {
                return WriteDecision::Allow;
            }
            let (reply, answer) = oneshot::channel();
            if requests
                .send(Request {
                    path,
                    content,
                    reply,
                })
                .is_err()
            {
                return WriteDecision::Decline;
            }
            // A dropped reply (loop gone) fails closed to a decline.
            answer.await.unwrap_or(WriteDecision::Decline)
        })
    })
}

/// Maps a key to the operator's decision, if the key is an approval answer.
pub fn decision_for_key(key: char) -> Option<WriteDecision> {
    match key {
        'y' | 'a' => Some(WriteDecision::Allow),
        'n' => Some(WriteDecision::Decline),
        _ => None,
    }
}

/// Opens the proposed body in `$EDITOR` and returns the saved text.
///
/// The terminal leaves raw mode and the alternate screen for the editor, then
/// returns to them. Any failure returns `None`, so the caller declines.
pub fn edit_proposal(content: &str) -> Option<String> {
    let path = std::env::temp_dir().join(format!("loom-proposal-{}", std::process::id()));
    std::fs::write(&path, content).ok()?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    let status = std::process::Command::new(&editor).arg(&path).status();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen);
    let _ = crossterm::terminal::enable_raw_mode();

    let edited = match status {
        Ok(status) if status.success() => std::fs::read_to_string(&path).ok(),
        _ => None,
    };
    let _ = std::fs::remove_file(&path);
    edited
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_decisions() {
        assert_eq!(decision_for_key('y'), Some(WriteDecision::Allow));
        assert_eq!(decision_for_key('n'), Some(WriteDecision::Decline));
        assert_eq!(decision_for_key('x'), None);
    }

    #[tokio::test]
    async fn dropped_loop_declines_instead_of_hanging() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let decision = hook(std::env::temp_dir().join("loom-approval-test-ws"), tx)(
            "a.txt".to_string(),
            "body".to_string(),
        )
        .await;
        assert_eq!(decision, WriteDecision::Decline);
    }
}
