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

use agent_client_protocol::Client;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::protocol::{
    parse_shell_message, BridgeMessage, InitializeParams, ShellMessage, PROTOCOL_VERSION,
};
use crate::session;

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
    let result = Client
        .builder()
        .name("cosh-acp")
        .connect_with(transport, async move |cx| {
            session::drive(cx, params, lines).await
        })
        .await;
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
