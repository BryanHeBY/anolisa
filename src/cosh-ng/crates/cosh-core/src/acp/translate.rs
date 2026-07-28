//! Translation between the engine's JSONL protocol and ACP.
//!
//! Runs as the connection's main task, so blocking on a client round trip is
//! safe here: whenever the translator waits, the engine is itself parked on
//! the answer it is waiting for.

use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Client, ConnectionTo, UntypedMessage};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::protocol::{
    AuthProvider, AuthReason, ContentBlock, ContentDelta, CoreControlRequest, OutputMessage,
    StreamEventPayload, UserContentBlock,
};

use super::AcpState;

/// Extension method carrying an interactive auth challenge (ADR-012): the
/// credential owner is the shell, so the agent only describes what it needs.
const AUTH_CHALLENGE_METHOD: &str = "_cosh/auth_challenge";

/// Extension method carrying a free-text or multiple-choice question. Never an
/// MCP tool: questions are session-plane traffic because they touch the UI.
const ASK_USER_METHOD: &str = "_cosh/ask_user";

/// Consumes engine output until the engine or the client goes away.
pub(super) async fn pump(
    state: Arc<AcpState>,
    cx: ConnectionTo<Client>,
    mut lines: UnboundedReceiver<String>,
) {
    while let Some(line) = lines.recv().await {
        let message: OutputMessage = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!("ignoring unparsable engine line: {error}");
                continue;
            }
        };
        if let Err(error) = translate(&state, &cx, message).await {
            tracing::warn!("acp translation failed: {error}");
        }
    }
}

async fn translate(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<Client>,
    message: OutputMessage,
) -> Result<(), acp::Error> {
    match message {
        OutputMessage::System { subtype, payload } => {
            if subtype == "init" {
                if let Some(session_id) = payload.session_id.as_deref() {
                    state.complete_session(session_id);
                }
            }
            Ok(())
        }
        OutputMessage::StreamEvent { event } => stream_update(state, cx, event),
        OutputMessage::Assistant { message, .. } => assistant_update(state, cx, &message.content),
        OutputMessage::User { message, .. } => tool_result_update(state, cx, &message.content),
        OutputMessage::ControlRequest {
            request_id,
            request,
        } => control_request(state, cx, &request_id, request).await,
        OutputMessage::Result {
            is_error,
            result,
            errors,
            ..
        } => {
            state.finish_prompt(turn_outcome(is_error, result, errors));
            Ok(())
        }
        OutputMessage::ControlResponse { .. } | OutputMessage::RegistryResponse { .. } => Ok(()),
    }
}

/// Maps a terminal turn result onto a stop reason.
///
/// Failures surface as protocol errors so the client can report them as a
/// recoverable agent failure rather than a silently finished turn.
fn turn_outcome(
    is_error: bool,
    result: Option<String>,
    errors: Option<Vec<String>>,
) -> Result<acp::StopReason, acp::Error> {
    if !is_error {
        return Ok(acp::StopReason::EndTurn);
    }
    let detail = errors
        .filter(|errors| !errors.is_empty())
        .map(|errors| errors.join("; "))
        .or(result)
        .unwrap_or_else(|| "agent turn failed".to_string());
    Err(acp::Error::internal_error().data(detail))
}

/// Forwards streaming text and thinking deltas, and announces tool calls.
fn stream_update(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<Client>,
    event: StreamEventPayload,
) -> Result<(), acp::Error> {
    let Some(session_id) = state.session_id() else {
        return Ok(());
    };
    let update = match event {
        StreamEventPayload::ContentBlockStart {
            content_block: crate::protocol::ContentBlockInfo::ToolUse { id, name },
            ..
        } => acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(id, name).status(acp::ToolCallStatus::Pending),
        ),
        StreamEventPayload::ContentBlockDelta {
            delta: ContentDelta::TextDelta { text },
            ..
        } => acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
            acp::ContentBlock::from(text),
        )),
        StreamEventPayload::ContentBlockDelta {
            delta: ContentDelta::ThinkingDelta { thinking },
            ..
        } => acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
            acp::ContentBlock::from(thinking),
        )),
        // Tool arguments arrive whole on the assistant message, so the partial
        // JSON deltas add nothing the client can render.
        _ => return Ok(()),
    };
    cx.send_notification(acp::SessionNotification::new(session_id, update))
}

/// Publishes the resolved arguments of each tool call.
fn assistant_update(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<Client>,
    blocks: &[ContentBlock],
) -> Result<(), acp::Error> {
    let Some(session_id) = state.session_id() else {
        return Ok(());
    };
    for block in blocks {
        let ContentBlock::ToolUse { id, name, input } = block else {
            continue;
        };
        let fields = acp::ToolCallUpdateFields::new()
            .title(name.clone())
            .status(acp::ToolCallStatus::InProgress)
            .raw_input(input.clone());
        cx.send_notification(acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(id.clone(), fields)),
        ))?;
    }
    Ok(())
}

/// Publishes tool results as terminal tool-call updates.
fn tool_result_update(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<Client>,
    blocks: &[UserContentBlock],
) -> Result<(), acp::Error> {
    let Some(session_id) = state.session_id() else {
        return Ok(());
    };
    for block in blocks {
        let UserContentBlock::ToolResult {
            tool_use_id,
            is_error,
            content,
        } = block;
        let status = if *is_error {
            acp::ToolCallStatus::Failed
        } else {
            acp::ToolCallStatus::Completed
        };
        let fields = acp::ToolCallUpdateFields::new()
            .status(status)
            .content(vec![acp::ToolCallContent::from(content.clone())]);
        cx.send_notification(acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                tool_use_id.clone(),
                fields,
            )),
        ))?;
    }
    Ok(())
}

/// Turns an engine control request into a client round trip, then feeds the
/// answer back on the engine's input stream.
async fn control_request(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<Client>,
    request_id: &str,
    request: CoreControlRequest,
) -> Result<(), acp::Error> {
    let body = match request {
        CoreControlRequest::CanUseTool {
            tool_name,
            input,
            description,
            tool_use_id,
            ..
        } => request_permission(state, cx, &tool_name, description, tool_use_id, input).await?,
        CoreControlRequest::AskUser {
            question,
            options,
            allow_free_text,
            multi_select,
        } => {
            ask_user(
                cx,
                state,
                &question,
                &options,
                allow_free_text,
                multi_select,
            )
            .await?
        }
        CoreControlRequest::AuthRequired {
            reason,
            error_message,
            providers,
        } => auth_challenge(cx, state, &reason, error_message, &providers).await?,
        // Terminal-output evidence is data-plane traffic served over MCP, so
        // the session plane must not answer it (ADR-012).
        CoreControlRequest::ShellEvidence { .. } => serde_json::json!({
            "behavior": "deny",
            "message": "shell evidence is served over MCP in ACP mode",
        }),
    };
    state
        .write_line(&serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": body,
            },
        }))
        .await
}

async fn request_permission(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<Client>,
    tool_name: &str,
    description: Option<String>,
    tool_use_id: String,
    input: serde_json::Value,
) -> Result<serde_json::Value, acp::Error> {
    let Some(session_id) = state.session_id() else {
        return Ok(serde_json::json!({
            "behavior": "deny",
            "message": "no active session",
        }));
    };
    let fields = acp::ToolCallUpdateFields::new()
        .title(description.unwrap_or_else(|| tool_name.to_string()))
        .raw_input(input);
    let request = acp::RequestPermissionRequest::new(
        session_id,
        acp::ToolCallUpdate::new(tool_use_id, fields),
        vec![
            acp::PermissionOption::new(
                "allow_once",
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                "allow_always",
                "Always allow",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                "reject_once",
                "Reject",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ],
    );
    let response = cx.send_request(request).block_task().await?;
    Ok(permission_body(&response.outcome))
}

/// Maps a permission outcome onto the engine's approval response body.
fn permission_body(outcome: &acp::RequestPermissionOutcome) -> serde_json::Value {
    match outcome {
        acp::RequestPermissionOutcome::Selected(selected) => {
            let allowed = matches!(selected.option_id.0.as_ref(), "allow_once" | "allow_always");
            if allowed {
                serde_json::json!({ "behavior": "allow" })
            } else {
                serde_json::json!({ "behavior": "deny", "message": "rejected by the user" })
            }
        }
        acp::RequestPermissionOutcome::Cancelled => serde_json::json!({
            "behavior": "deny",
            "message": "cancelled",
        }),
        _ => serde_json::json!({
            "behavior": "deny",
            "message": "unsupported permission outcome",
        }),
    }
}

async fn ask_user(
    cx: &ConnectionTo<Client>,
    state: &Arc<AcpState>,
    question: &str,
    options: &[crate::protocol::AskUserOption],
    allow_free_text: bool,
    multi_select: bool,
) -> Result<serde_json::Value, acp::Error> {
    let params = serde_json::json!({
        "sessionId": state.session_id(),
        "question": question,
        "options": options,
        "allowFreeText": allow_free_text,
        "multiSelect": multi_select,
    });
    let answer = extension_round_trip(cx, ASK_USER_METHOD, params).await?;
    let Some(answer) = answer else {
        // No answer means the client declined; the engine reports the tool as
        // unanswered rather than hanging.
        return Ok(serde_json::json!({ "behavior": "deny", "message": "unanswered" }));
    };
    Ok(serde_json::json!({
        "behavior": "allow",
        "answer": answer.get("answer").and_then(|value| value.as_str()),
    }))
}

async fn auth_challenge(
    cx: &ConnectionTo<Client>,
    state: &Arc<AcpState>,
    reason: &AuthReason,
    error_message: Option<String>,
    providers: &[AuthProvider],
) -> Result<serde_json::Value, acp::Error> {
    let params = serde_json::json!({
        "sessionId": state.session_id(),
        "reason": reason,
        "errorMessage": error_message,
        "providers": providers,
    });
    let response = extension_round_trip(cx, AUTH_CHALLENGE_METHOD, params).await?;
    let Some(response) = response else {
        return Ok(serde_json::json!({ "behavior": "deny", "message": "auth declined" }));
    };
    if response
        .get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(serde_json::json!({ "behavior": "deny", "message": "auth cancelled" }));
    }
    // Credential values are relayed verbatim and never logged (ADR-012).
    Ok(serde_json::json!({
        "provider_id": response.get("providerId"),
        "provider_type": response.get("providerType"),
        "values": response.get("values"),
        "persist": response.get("persist"),
    }))
}

/// Sends one `_cosh/*` extension request, treating a rejection as "no answer"
/// so an older client that does not implement it cannot stall the turn.
async fn extension_round_trip(
    cx: &ConnectionTo<Client>,
    method: &str,
    params: serde_json::Value,
) -> Result<Option<serde_json::Value>, acp::Error> {
    let message = UntypedMessage::new(method, params)?;
    match cx.send_request(message).block_task().await {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            tracing::warn!(method, "extension request rejected: {error}");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_turn_ends_normally() {
        let outcome = turn_outcome(false, Some("completed".to_string()), None);
        assert!(matches!(outcome, Ok(acp::StopReason::EndTurn)));
    }

    #[test]
    fn failed_turn_carries_the_engine_errors() {
        let outcome = turn_outcome(true, None, Some(vec!["boom".to_string()]));
        let error = outcome.expect_err("expected failure");
        assert_eq!(error.data.as_ref().and_then(|d| d.as_str()), Some("boom"));
    }

    #[test]
    fn allow_options_approve_and_others_deny() {
        let selected = acp::RequestPermissionOutcome::Selected(
            acp::SelectedPermissionOutcome::new("allow_once"),
        );
        assert_eq!(permission_body(&selected)["behavior"], "allow");
        let rejected = acp::RequestPermissionOutcome::Selected(
            acp::SelectedPermissionOutcome::new("reject_once"),
        );
        assert_eq!(permission_body(&rejected)["behavior"], "deny");
        assert_eq!(
            permission_body(&acp::RequestPermissionOutcome::Cancelled)["behavior"],
            "deny"
        );
    }
}
