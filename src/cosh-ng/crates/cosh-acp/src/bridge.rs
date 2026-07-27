//! Bridge runtime: JSONL handshake with cosh-shell and agent process custody.
//!
//! The bridge owns exactly one agent child process. Custody rules (ADR-011):
//! process group spawn, kill on drop, SIGTERM then SIGKILL on shutdown, and
//! stdin EOF from the shell always takes the agent down with the bridge.
//!
//! ACP session wiring (session/new, prompt streaming, terminal delegation) is
//! layered on top of this skeleton in a follow-up change; until then every
//! post-handshake message except `shutdown` is answered with a recoverable
//! `agent_failed` so cosh-shell can degrade cleanly.

use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::protocol::{
    parse_shell_message, AgentCapabilities, BridgeMessage, InitializeParams, ShellMessage,
    PROTOCOL_VERSION,
};

/// Grace period between SIGTERM and SIGKILL when stopping the agent.
const AGENT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
fn emit(message: &BridgeMessage) {
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

    // ACP initialize round-trip is wired in the follow-up change; the
    // skeleton reports default capabilities so cosh-shell can gate features
    // conservatively.
    emit(&BridgeMessage::Initialized {
        protocol_version: PROTOCOL_VERSION,
        agent_capabilities: AgentCapabilities::default(),
        auth_methods: Vec::new(),
    });

    let exit_code = loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                tracing::info!("shell stream closed; stopping agent");
                break 0;
            }
            Err(error) => {
                tracing::warn!("shell stream error: {error}");
                break 1;
            }
        };
        match parse_shell_message(&line) {
            Ok(ShellMessage::Shutdown) => {
                tracing::info!("shutdown requested");
                break 0;
            }
            Ok(ShellMessage::Initialize(_)) => {
                emit(&BridgeMessage::protocol_error(
                    "duplicate initialize".to_string(),
                ));
                break 1;
            }
            Ok(message) => {
                // Session wiring lands in the follow-up change; answer with a
                // recoverable failure instead of silently dropping requests.
                // Only the variant name is logged: AuthResponse values may
                // carry secrets (ADR-012).
                tracing::debug!(
                    message = message_variant(&message),
                    "session message before ACP wiring"
                );
                emit(&BridgeMessage::AgentFailed {
                    code: "not_implemented".to_string(),
                    message: "ACP session wiring is not implemented yet".to_string(),
                    recoverable: true,
                    hint: None,
                });
            }
            Err(error) => {
                tracing::warn!("ignoring malformed shell message: {error}");
            }
        }
    };

    stop_agent(&mut agent).await;
    exit_code
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
