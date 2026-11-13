//! The policy `Ask` → `session/request_permission` bridge.

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::v1::schema as acp;

/// Sends one `session/request_permission` for a blocked command and
/// resolves `true` only for an explicit allow.
///
/// The round-trip is bounded: an editor that never answers (dialog
/// dismissed, extension hung) would otherwise stall the task forever.
/// On expiry the program is denied — fail closed — and a warning is
/// emitted for the operator.
pub(crate) async fn ask_permission(
    cx: agent_client_protocol::v1::ConnectionTo<Client>,
    session: acp::SessionId,
    program: String,
    line: String,
    timeout: std::time::Duration,
) -> bool {
    let title = format!("Run `{line}`");
    let mut fields = acp::ToolCallUpdateFields::new();
    fields.title = Some(title);
    fields.kind = Some(acp::ToolKind::Execute);
    fields.status = Some(acp::ToolCallStatus::InProgress);
    let tool_call = acp::ToolCallUpdate::new(
        acp::ToolCallId::new(uuid::Uuid::now_v7().to_string()),
        fields,
    );
    let request = acp::RequestPermissionRequest::new(
        session,
        tool_call,
        vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow"),
                "Allow",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("reject"),
                "Deny",
                acp::PermissionOptionKind::RejectOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("reject-always"),
                "Always deny",
                acp::PermissionOptionKind::RejectAlways,
            ),
        ],
    );
    match tokio::time::timeout(timeout, cx.send_request(request).block_task()).await {
        // The dialog expired unanswered: deny (fail closed) so the
        // task continues with a refusal the model can see.
        Err(_) => {
            tracing::warn!(
                program = %program,
                timeout = ?timeout,
                "permission request timed out; denying"
            );
            false
        }
        Ok(Ok(response)) => {
            // Upstream enum is non-exhaustive; unknown outcomes fail closed.
            #[allow(clippy::wildcard_enum_match_arm)]
            match response.outcome {
                acp::RequestPermissionOutcome::Selected(selected) => {
                    &*selected.option_id.0 == "allow"
                }
                // Cancelled, or a future outcome kind: fail closed.
                _ => false,
            }
        }
        // Transport failure: fail closed.
        Ok(Err(_)) => false,
    }
}
