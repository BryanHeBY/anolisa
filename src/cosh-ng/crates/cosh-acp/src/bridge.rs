//! Bridge runtime: JSONL handshake with cosh-shell and agent process custody.
//!
//! The bridge owns exactly one agent child process. Custody rules (ADR-011):
//! process group spawn, kill on drop, SIGTERM then SIGKILL on shutdown, and
//! stdin EOF from the shell always takes the agent down with the bridge.
//!
//! After the handshake the agent's stdio moves into the ACP SDK transport and
//! `session::drive` translates between the shell JSONL protocol and ACP; the
//! child process handle itself stays here so custody survives SDK shutdown.

use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::Client;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::protocol::{
    parse_shell_message, BridgeMessage, InitializeParams, ShellMessage, PROTOCOL_VERSION,
};
use crate::session;

/// Grace period between SIGTERM and SIGKILL when stopping the agent.
const AGENT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Namespace of cosh's ACP extension methods.
const COSH_EXTENSION_PREFIX: &str = "_cosh/";

/// Interactive credential challenge; the shell owns the credentials (ADR-012).
const AUTH_CHALLENGE_METHOD: &str = "_cosh/auth_challenge";

/// Free-text or multiple-choice question. Session-plane traffic by design:
/// questions touch the UI, so they never travel on the MCP data plane.
const ASK_USER_METHOD: &str = "_cosh/ask_user";

/// Parks one `_cosh/*` request and republishes it for the shell to answer.
fn emit_extension_request(
    pending: &crate::pending::PendingRequests,
    message: &agent_client_protocol::UntypedMessage,
    responder: agent_client_protocol::Responder<serde_json::Value>,
) {
    let method = message.method().to_string();
    // Only the method name is logged: auth challenges describe credentials.
    tracing::debug!(method = %method, "cosh extension request received");
    let params = message.params();
    match method.as_str() {
        AUTH_CHALLENGE_METHOD => {
            let request_id = pending.park_extension(responder);
            emit(&BridgeMessage::AuthRequired {
                request_id,
                methods: Vec::new(),
                reason: string_field(params, "reason"),
                error_message: optional_string_field(params, "errorMessage"),
                providers: parse_field(params, "providers"),
            });
        }
        ASK_USER_METHOD => {
            let request_id = pending.park_extension(responder);
            emit(&BridgeMessage::AskUser {
                session_id: string_field(params, "sessionId"),
                request_id,
                question: string_field(params, "question"),
                options: parse_field(params, "options"),
                allow_free_text: bool_field(params, "allowFreeText"),
                multi_select: bool_field(params, "multiSelect"),
            });
        }
        _ => {
            let _ = responder
                .respond_with_error(agent_client_protocol::Error::method_not_found().data(method));
        }
    }
}

fn string_field(params: &serde_json::Value, name: &str) -> String {
    optional_string_field(params, name).unwrap_or_default()
}

fn optional_string_field(params: &serde_json::Value, name: &str) -> Option<String> {
    params
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn bool_field(params: &serde_json::Value, name: &str) -> bool {
    params
        .get(name)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Deserializes one field, dropping it when the agent sent an unusable shape.
fn parse_field<T: serde::de::DeserializeOwned + Default>(
    params: &serde_json::Value,
    name: &str,
) -> T {
    params
        .get(name)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Validates the first JSONL line as a version-compatible handshake.
///
/// # Errors
///
/// Returns a human-readable reason; handshake failures are fatal and must be
/// reported as a non-recoverable `protocol_error`.
pub fn handshake_from_line(line: &str) -> Result<InitializeParams, String> {
    match parse_shell_message(line) {
        Ok(ShellMessage::Initialize(params)) => {
            if params.protocol_version != PROTOCOL_VERSION {
                return Err(format!(
                    "unsupported protocol version {} (bridge speaks {PROTOCOL_VERSION})",
                    params.protocol_version
                ));
            }
            if params.agent.command.is_empty() {
                return Err("agent launch spec has an empty command".to_string());
            }
            Ok(*params)
        }
        Ok(_) => Err("first message must be initialize".to_string()),
        Err(error) => Err(format!("malformed initialize message: {error}")),
    }
}

/// Spawns the agent child from the launch spec.
///
/// The agent gets its own process group so cancellation can signal the whole
/// tree, and `kill_on_drop` guarantees custody even on bridge panic. Secrets
/// in `env` are injected without being logged.
fn spawn_agent(params: &InitializeParams) -> Result<Child, String> {
    let mut command = Command::new(&params.agent.command);
    command
        .args(&params.agent.args)
        .envs(&params.agent.env)
        .current_dir(&params.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command
        .spawn()
        .map_err(|error| format!("failed to spawn agent '{}': {error}", params.agent.command))
}

/// Stops the agent with SIGTERM, escalating to SIGKILL after the grace period.
async fn stop_agent(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Signal the whole process group; the agent may have MCP children.
        let group = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGTERM);
        if tokio::time::timeout(AGENT_SHUTDOWN_GRACE, child.wait())
            .await
            .is_ok()
        {
            return;
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Writes one protocol message as a JSONL line on stdout.
pub(crate) fn emit(message: &BridgeMessage) {
    match serde_json::to_string(message) {
        Ok(line) => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
        Err(error) => tracing::error!("failed to serialize bridge message: {error}"),
    }
}

/// Runs the bridge until shell stdin closes or `shutdown` arrives.
///
/// # Errors
///
/// Returns a process exit code; handshake failures exit non-zero after
/// emitting a `protocol_error` event.
pub async fn run() -> i32 {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    let first_line = match lines.next_line().await {
        Ok(Some(line)) => line,
        Ok(None) => {
            tracing::warn!("shell closed the stream before initialize");
            return 1;
        }
        Err(error) => {
            emit(&BridgeMessage::protocol_error(format!(
                "failed to read initialize: {error}"
            )));
            return 1;
        }
    };
    let params = match handshake_from_line(&first_line) {
        Ok(params) => params,
        Err(reason) => {
            emit(&BridgeMessage::protocol_error(reason));
            return 1;
        }
    };

    let mut agent = match spawn_agent(&params) {
        Ok(child) => child,
        Err(message) => {
            emit(&BridgeMessage::AgentFailed {
                code: "agent_spawn_failed".to_string(),
                message,
                recoverable: true,
                hint: Some("check [acp.agents] configuration and PATH".to_string()),
            });
            return 1;
        }
    };
    tracing::info!(agent = %params.agent.name, "agent process started");
    // Captured before the stdio moves into the transport; the sentinel needs
    // the pid to watch the agent's children.
    let agent_pid = agent.id();

    // Only the pipes move into the SDK transport; the child handle stays here
    // so custody (SIGTERM grace, kill on drop) survives SDK shutdown.
    let (Some(agent_stdin), Some(agent_stdout)) = (agent.stdin.take(), agent.stdout.take()) else {
        emit(&BridgeMessage::AgentFailed {
            code: "agent_spawn_failed".to_string(),
            message: "agent process has no piped stdio".to_string(),
            recoverable: true,
            hint: None,
        });
        stop_agent(&mut agent).await;
        return 1;
    };
    let transport = session::agent_transport(agent_stdin, agent_stdout);
    let registry = std::sync::Arc::new(crate::terminal::TerminalRegistry::default());
    let pending = std::sync::Arc::new(crate::pending::PendingRequests::default());
    let result = Client
        .builder()
        .name("cosh-acp")
        .on_receive_notification(
            async move |notification: acp::SessionNotification, _cx| {
                // Session updates are handled at the connection level so both
                // a freshly created and a reloaded session stream through the
                // same path; the SDK's ActiveSession only covers the former.
                session::emit_session_update(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let pending = std::sync::Arc::clone(&pending);
                async move |request: acp::RequestPermissionRequest, responder, _cx| {
                    let session_id = request.session_id.to_string();
                    let title = request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| request.tool_call.tool_call_id.to_string());
                    let options = request
                        .options
                        .iter()
                        .map(|option| crate::protocol::PermissionOption {
                            id: option.option_id.0.to_string(),
                            label: option.name.clone(),
                            kind: session::enum_token(&option.kind),
                        })
                        .collect();
                    let request_id = pending.park_permission(responder);
                    emit(&BridgeMessage::PermissionRequest {
                        session_id,
                        request_id,
                        title,
                        options,
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = std::sync::Arc::clone(&registry);
                async move |request: acp::CreateTerminalRequest, responder, _cx| {
                    registry.create(request, responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = std::sync::Arc::clone(&registry);
                async move |request: acp::TerminalOutputRequest, responder, _cx| {
                    registry.output(&request, responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = std::sync::Arc::clone(&registry);
                async move |request: acp::WaitForTerminalExitRequest, responder, _cx| {
                    registry.wait_for_exit(&request, responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = std::sync::Arc::clone(&registry);
                async move |request: acp::KillTerminalRequest, responder, _cx| {
                    registry.kill(&request, responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = std::sync::Arc::clone(&registry);
                async move |request: acp::ReleaseTerminalRequest, responder, _cx| {
                    registry.release(&request, responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            {
                let pending = std::sync::Arc::clone(&pending);
                async move |dispatch: agent_client_protocol::Dispatch<
                    agent_client_protocol::UntypedMessage,
                    agent_client_protocol::UntypedMessage,
                >,
                            _cx| {
                    match dispatch {
                        // Responses must always be routed to the caller that
                        // is waiting for them: declining one from the last
                        // handler in the chain drops it silently.
                        agent_client_protocol::Dispatch::Response(result, router) => router
                            .route_with_result(result)
                            .map(|()| agent_client_protocol::Handled::Yes),
                        // Registered last on purpose: an untyped message
                        // matches every method, so anything outside `_cosh/`
                        // is declined and left to the handlers that own it.
                        agent_client_protocol::Dispatch::Request(message, responder)
                            if message.method().starts_with(COSH_EXTENSION_PREFIX) =>
                        {
                            emit_extension_request(&pending, &message, responder);
                            Ok(agent_client_protocol::Handled::Yes)
                        }
                        agent_client_protocol::Dispatch::Notification(message)
                            if message.method().starts_with(COSH_EXTENSION_PREFIX) =>
                        {
                            tracing::debug!(
                                method = message.method(),
                                "ignoring unknown cosh extension notification"
                            );
                            Ok(agent_client_protocol::Handled::Yes)
                        }
                        other => Ok(agent_client_protocol::Handled::No {
                            message: other,
                            retry: false,
                        }),
                    }
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(transport, {
            let pending = std::sync::Arc::clone(&pending);
            async move |cx| session::drive(cx, params, lines, registry, pending, agent_pid).await
        })
        .await;
    pending.fail_all("bridge connection closed");
    let exit_code = match result {
        Ok(code) => code,
        Err(error) => {
            emit(&BridgeMessage::AgentFailed {
                code: "acp_connection_failed".to_string(),
                message: format!("ACP connection failed: {error}"),
                recoverable: true,
                hint: None,
            });
            1
        }
    };

    stop_agent(&mut agent).await;
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AgentLaunchSpec;
    use std::collections::BTreeMap;

    fn params_line(version: u32, command: &str) -> String {
        serde_json::to_string(&ShellMessage::Initialize(Box::new(InitializeParams {
            protocol_version: version,
            agent: AgentLaunchSpec {
                name: "fake".to_string(),
                command: command.to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            cwd: "/tmp".to_string(),
            mcp_servers: Vec::new(),
            capabilities: Default::default(),
            locale: None,
        })))
        .expect("serialize")
    }

    #[test]
    fn handshake_accepts_matching_version() {
        let params =
            handshake_from_line(&params_line(PROTOCOL_VERSION, "true")).expect("handshake");
        assert_eq!(params.agent.name, "fake");
    }

    #[test]
    fn handshake_rejects_version_mismatch() {
        let error = handshake_from_line(&params_line(99, "true")).unwrap_err();
        assert!(error.contains("unsupported protocol version"), "{error}");
    }

    #[test]
    fn handshake_rejects_empty_command() {
        let error = handshake_from_line(&params_line(PROTOCOL_VERSION, "")).unwrap_err();
        assert!(error.contains("empty command"), "{error}");
    }

    #[test]
    fn handshake_rejects_non_initialize_first_message() {
        let line = serde_json::to_string(&ShellMessage::Shutdown).expect("serialize");
        let error = handshake_from_line(&line).unwrap_err();
        assert!(error.contains("must be initialize"), "{error}");
    }

    #[tokio::test]
    async fn spawn_and_stop_agent_reaps_the_child() {
        let params = handshake_from_line(&params_line(PROTOCOL_VERSION, "sleep"))
            .map(|mut params| {
                params.agent.args = vec!["30".to_string()];
                params
            })
            .expect("handshake");
        let mut child = spawn_agent(&params).expect("spawn");
        stop_agent(&mut child).await;
        let status = child.try_wait().expect("wait");
        assert!(status.is_some(), "agent must be reaped after stop");
    }
}
