//! ACP session wiring: drives the SDK client connection to a spawned agent
//! and translates between the shell JSONL protocol (v1) and ACP.
//!
//! The bridge keeps process custody in `bridge.rs`; this module only borrows
//! the agent's stdio for the JSON-RPC transport. One prompt turn is in flight
//! at a time, matching the shell adapter contract. Permission, auth, and
//! terminal delegation land in later migration stages (ADR-011 S3/S4) and are
//! answered with a recoverable `not_implemented` failure until then.

use futures::channel::mpsc;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{ActiveSession, Agent, ConnectionTo, Lines, SessionMessage};

use crate::bridge::emit;
use crate::protocol::{
    parse_shell_message, AgentCapabilities, AuthMethodInfo, BridgeMessage, InitializeParams,
    McpServerSpec, ShellMessage, PROTOCOL_VERSION,
};

/// Meta key carrying the per-turn approval mode on `session/prompt`
/// (ADR-011 lifecycle model; `_cosh/` is the cosh extension prefix).
const APPROVAL_MODE_META_KEY: &str = "_cosh/approval_mode";

/// Result of one prompt turn, delivered back to the main loop.
enum PromptOutcome {
    Stopped(acp::StopReason),
    Failed(String),
}

/// Builds the SDK line transport over the agent child's stdio.
///
/// The bridge retains ownership of the child process itself; only the pipes
/// move into the transport, so custody (SIGTERM grace, kill on drop) stays
/// with `bridge.rs`.
pub(crate) fn agent_transport(
    stdin: ChildStdin,
    stdout: ChildStdout,
) -> Lines<
    impl futures::Sink<String, Error = std::io::Error> + Send + 'static,
    impl futures::Stream<Item = std::io::Result<String>> + Send + 'static,
> {
    let outgoing = futures::sink::unfold(stdin, |mut stdin, line: String| async move {
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok::<_, std::io::Error>(stdin)
    });
    let incoming =
        futures::stream::unfold(BufReader::new(stdout).lines(), |mut lines| async move {
            match lines.next_line().await {
                Ok(Some(line)) => Some((Ok(line), lines)),
                Ok(None) => None,
                Err(error) => Some((Err(error), lines)),
            }
        });
    Lines::new(outgoing, incoming)
}

/// Runs the post-handshake session loop inside the SDK connection.
///
/// Returns the bridge process exit code. Protocol-level agent failures are
/// reported as `agent_failed` events and keep the loop alive where possible;
/// only shell stream loss or shutdown ends the loop.
///
/// # Errors
///
/// Propagates SDK connection errors (transport loss, handler failure); the
/// caller translates them into a final `agent_failed` event.
pub(crate) async fn drive(
    cx: ConnectionTo<Agent>,
    params: InitializeParams,
    mut shell_lines: tokio::io::Lines<BufReader<tokio::io::Stdin>>,
) -> Result<i32, agent_client_protocol::Error> {
    let request = acp::InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(acp::ClientCapabilities::new().terminal(params.capabilities.terminal))
        .client_info(acp::Implementation::new(
            "cosh-acp",
            env!("CARGO_PKG_VERSION"),
        ));
    // Race initialize against the shell stream: an unresponsive agent must
    // not keep the bridge alive after the shell goes away (custody rule).
    let init_future = cx.send_request(request).block_task();
    tokio::pin!(init_future);
    let response = loop {
        tokio::select! {
            result = &mut init_future => match result {
                Ok(response) => break response,
                Err(error) => {
                    emit(&BridgeMessage::AgentFailed {
                        code: "acp_initialize_failed".to_string(),
                        message: format!("ACP initialize failed: {error}"),
                        recoverable: true,
                        hint: Some(
                            "agent may not speak ACP; check its launch arguments".to_string(),
                        ),
                    });
                    return Ok(1);
                }
            },
            line = shell_lines.next_line() => match line {
                Ok(Some(line)) => match parse_shell_message(&line) {
                    Ok(ShellMessage::Shutdown) => return Ok(0),
                    Ok(message) => tracing::warn!(
                        message = message_variant(&message),
                        "ignoring shell message before ACP initialize completed"
                    ),
                    Err(error) => tracing::warn!("ignoring malformed shell message: {error}"),
                },
                Ok(None) => {
                    tracing::info!("shell stream closed during ACP initialize; stopping agent");
                    return Ok(0);
                }
                Err(error) => {
                    tracing::warn!("shell stream error: {error}");
                    return Ok(1);
                }
            },
        }
    };
    emit(&BridgeMessage::Initialized {
        protocol_version: PROTOCOL_VERSION,
        agent_capabilities: translate_agent_capabilities(&response.agent_capabilities),
        auth_methods: response
            .auth_methods
            .iter()
            .map(translate_auth_method)
            .collect(),
    });

    let (outcome_tx, mut outcome_rx) = mpsc::unbounded::<(String, PromptOutcome)>();
    let mut active: Option<ActiveSession<'static, Agent>> = None;
    let mut prompt_inflight = false;

    loop {
        tokio::select! {
            // Biased: drain queued session updates before the prompt outcome
            // so prompt_completed never overtakes the deltas of its turn.
            biased;
            // The precondition guards the expect: the branch is never polled
            // when `active` is None.
            update = async { active.as_mut().expect("guarded by precondition").read_update().await },
                if active.is_some() =>
            {
                match update {
                    Ok(update) => translate_session_message(update).await?,
                    Err(error) => {
                        emit(&BridgeMessage::AgentFailed {
                            code: "session_stream_failed".to_string(),
                            message: format!("session update stream failed: {error}"),
                            recoverable: true,
                            hint: None,
                        });
                        active = None;
                        prompt_inflight = false;
                    }
                }
            },
            outcome = outcome_rx.next() => {
                if let Some((request_id, outcome)) = outcome {
                    prompt_inflight = false;
                    match outcome {
                        PromptOutcome::Stopped(reason) => emit(&BridgeMessage::PromptCompleted {
                            request_id,
                            stop_reason: enum_token(&reason),
                        }),
                        PromptOutcome::Failed(message) => emit(&BridgeMessage::AgentFailed {
                            code: "prompt_failed".to_string(),
                            message,
                            recoverable: true,
                            hint: None,
                        }),
                    }
                }
            },
            line = shell_lines.next_line() => match line {
                Ok(Some(line)) => match parse_shell_message(&line) {
                    Ok(ShellMessage::Shutdown) => {
                        tracing::info!("shutdown requested");
                        return Ok(0);
                    }
                    Ok(ShellMessage::Initialize(_)) => {
                        emit(&BridgeMessage::protocol_error("duplicate initialize".to_string()));
                        return Ok(1);
                    }
                    Ok(message) => {
                        handle_shell_message(
                            &cx,
                            &params,
                            &mut active,
                            &mut prompt_inflight,
                            &outcome_tx,
                            message,
                        )
                        .await?;
                    }
                    Err(error) => tracing::warn!("ignoring malformed shell message: {error}"),
                },
                Ok(None) => {
                    tracing::info!("shell stream closed; stopping agent");
                    return Ok(0);
                }
                Err(error) => {
                    tracing::warn!("shell stream error: {error}");
                    return Ok(1);
                }
            },
        }
    }
}

/// Dispatches one post-handshake shell message.
async fn handle_shell_message(
    cx: &ConnectionTo<Agent>,
    params: &InitializeParams,
    active: &mut Option<ActiveSession<'static, Agent>>,
    prompt_inflight: &mut bool,
    outcome_tx: &mpsc::UnboundedSender<(String, PromptOutcome)>,
    message: ShellMessage,
) -> Result<(), agent_client_protocol::Error> {
    match message {
        ShellMessage::SessionNew { request_id, cwd } => {
            let request = acp::NewSessionRequest::new(cwd)
                .mcp_servers(translate_mcp_servers(&params.mcp_servers));
            match cx
                .build_session_from(request)
                .block_task()
                .start_session()
                .await
            {
                Ok(session) => {
                    emit(&BridgeMessage::SessionCreated {
                        request_id,
                        session_id: session.session_id().to_string(),
                    });
                    *active = Some(session);
                    *prompt_inflight = false;
                }
                Err(error) => emit(&BridgeMessage::AgentFailed {
                    code: "session_new_failed".to_string(),
                    message: format!("session/new failed: {error}"),
                    recoverable: true,
                    hint: None,
                }),
            }
        }
        ShellMessage::Prompt {
            request_id,
            session_id,
            text,
            approval_mode,
        } => {
            let Some(session) = active.as_ref() else {
                emit(&prompt_rejected(
                    "no active session; send session_new first",
                ));
                return Ok(());
            };
            if session.session_id().to_string() != session_id {
                emit(&prompt_rejected("prompt targets an unknown session"));
                return Ok(());
            }
            if *prompt_inflight {
                emit(&prompt_rejected("a prompt is already in flight"));
                return Ok(());
            }
            let mut meta = serde_json::Map::new();
            meta.insert(APPROVAL_MODE_META_KEY.to_string(), approval_mode.into());
            let request = acp::PromptRequest::new(session_id, vec![text.into()]).meta(meta);
            let tx = outcome_tx.clone();
            cx.send_request(request)
                .on_receiving_result(async move |result| {
                    let outcome = match result {
                        Ok(response) => PromptOutcome::Stopped(response.stop_reason),
                        Err(error) => PromptOutcome::Failed(format!("prompt failed: {error}")),
                    };
                    let _ = tx.unbounded_send((request_id, outcome));
                    Ok(())
                })?;
            *prompt_inflight = true;
        }
        ShellMessage::Cancel { session_id } => {
            cx.send_notification(acp::CancelNotification::new(session_id))?;
        }
        other => {
            // Only the variant name is logged: AuthResponse values may carry
            // secrets (ADR-012).
            tracing::debug!(
                message = message_variant(&other),
                "shell message not implemented yet"
            );
            emit(&BridgeMessage::AgentFailed {
                code: "not_implemented".to_string(),
                message: format!("{} is not implemented yet", message_variant(&other)),
                recoverable: true,
                hint: None,
            });
        }
    }
    Ok(())
}

/// Translates one session update from the agent into bridge events.
async fn translate_session_message(
    message: SessionMessage,
) -> Result<(), agent_client_protocol::Error> {
    match message {
        SessionMessage::SessionMessage(dispatch) => {
            MatchDispatch::new(dispatch)
                .if_notification(async |notification: acp::SessionNotification| {
                    emit_session_update(notification);
                    Ok(())
                })
                .await
                .otherwise_ignore()?;
        }
        // Stop reasons flow through the prompt response callback because the
        // bridge sends its own PromptRequest instead of ActiveSession's.
        SessionMessage::StopReason(_) => {}
        _ => {}
    }
    Ok(())
}

/// Emits the bridge event for one `session/update` notification.
fn emit_session_update(notification: acp::SessionNotification) {
    let session_id = notification.session_id.to_string();
    match notification.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            if let acp::ContentBlock::Text(text) = chunk.content {
                emit(&BridgeMessage::TextDelta {
                    session_id,
                    text: text.text,
                });
            }
        }
        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            if let acp::ContentBlock::Text(text) = chunk.content {
                emit(&BridgeMessage::ThoughtDelta {
                    session_id,
                    text: text.text,
                });
            }
        }
        acp::SessionUpdate::ToolCall(call) => emit(&BridgeMessage::ToolCall {
            session_id,
            tool_call_id: call.tool_call_id.to_string(),
            title: call.title,
            kind: enum_token(&call.kind),
            status: enum_token(&call.status),
        }),
        acp::SessionUpdate::ToolCallUpdate(update) => emit(&BridgeMessage::ToolCall {
            session_id,
            tool_call_id: update.tool_call_id.to_string(),
            title: update.fields.title.unwrap_or_default(),
            kind: update
                .fields
                .kind
                .map(|kind| enum_token(&kind))
                .unwrap_or_default(),
            status: update
                .fields
                .status
                .map(|status| enum_token(&status))
                .unwrap_or_default(),
        }),
        other => {
            tracing::trace!(update = ?std::mem::discriminant(&other), "ignoring session update")
        }
    }
}

fn prompt_rejected(message: &str) -> BridgeMessage {
    BridgeMessage::AgentFailed {
        code: "prompt_rejected".to_string(),
        message: message.to_string(),
        recoverable: true,
        hint: None,
    }
}

fn translate_agent_capabilities(caps: &acp::AgentCapabilities) -> AgentCapabilities {
    AgentCapabilities {
        load_session: caps.load_session,
        resume: caps.session_capabilities.resume.is_some(),
        close: caps.session_capabilities.close.is_some(),
    }
}

fn translate_auth_method(method: &acp::AuthMethod) -> AuthMethodInfo {
    AuthMethodInfo {
        id: method.id().to_string(),
        label: method.name().to_string(),
        description: method.description().map(str::to_owned),
    }
}

fn translate_mcp_servers(specs: &[McpServerSpec]) -> Vec<acp::McpServer> {
    specs
        .iter()
        .map(|spec| {
            acp::McpServer::Stdio(
                acp::McpServerStdio::new(&spec.name, &spec.command)
                    .args(spec.args.clone())
                    .env(
                        spec.env
                            .iter()
                            .map(|(name, value)| acp::EnvVariable::new(name.clone(), value.clone()))
                            .collect(),
                    ),
            )
        })
        .collect()
}

/// Serializes a snake_case wire enum (StopReason, ToolKind, ...) to its token.
fn enum_token<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Variant name for logging; never includes payload fields because auth
/// responses may carry secret values (ADR-012).
fn message_variant(message: &ShellMessage) -> &'static str {
    match message {
        ShellMessage::Initialize(_) => "initialize",
        ShellMessage::SessionNew { .. } => "session_new",
        ShellMessage::SessionLoad { .. } => "session_load",
        ShellMessage::Prompt { .. } => "prompt",
        ShellMessage::Cancel { .. } => "cancel",
        ShellMessage::PermissionResponse { .. } => "permission_response",
        ShellMessage::AuthResponse { .. } => "auth_response",
        ShellMessage::TerminalCreated { .. } => "terminal_created",
        ShellMessage::TerminalOutput { .. } => "terminal_output",
        ShellMessage::TerminalExit { .. } => "terminal_exit",
        ShellMessage::TerminalDenied { .. } => "terminal_denied",
        ShellMessage::Shutdown => "shutdown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_token_serializes_snake_case() {
        assert_eq!(enum_token(&acp::StopReason::EndTurn), "end_turn");
        assert_eq!(enum_token(&acp::ToolCallStatus::InProgress), "in_progress");
    }

    #[test]
    fn agent_capabilities_translate_from_session_capabilities() {
        let caps = acp::AgentCapabilities::new().load_session(true);
        let translated = translate_agent_capabilities(&caps);
        assert!(translated.load_session);
        assert!(!translated.resume);
        assert!(!translated.close);
    }

    #[test]
    fn mcp_specs_translate_to_stdio_servers() {
        let specs = vec![McpServerSpec {
            name: "cosh-shell".to_string(),
            command: "/usr/bin/cosh-acp".to_string(),
            args: vec!["mcp-shell".to_string()],
            env: [("COSH_TOKEN".to_string(), "t".to_string())].into(),
        }];
        let servers = translate_mcp_servers(&specs);
        assert_eq!(servers.len(), 1);
        let acp::McpServer::Stdio(stdio) = &servers[0] else {
            panic!("expected stdio transport");
        };
        assert_eq!(stdio.name, "cosh-shell");
        assert_eq!(stdio.args, vec!["mcp-shell".to_string()]);
        assert_eq!(stdio.env.len(), 1);
    }

    #[test]
    fn auth_methods_translate_id_and_label() {
        let method = acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
            acp::AuthMethodId::new("oauth"),
            "OAuth login",
        ));
        let info = translate_auth_method(&method);
        assert_eq!(info.id, "oauth");
        assert_eq!(info.label, "OAuth login");
        assert!(info.description.is_none());
    }
}
