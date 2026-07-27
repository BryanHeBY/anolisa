//! Internal JSONL protocol (v1) between cosh-shell and the cosh-acp bridge.
//!
//! Contract rules (ADR-011/012):
//! - argv selects the process form only; all structured configuration arrives
//!   in the first `initialize` message.
//! - stdout carries protocol messages exclusively; logs go to stderr.
//! - Secret values may transit these messages (e.g. auth field values) but
//!   must never be logged or echoed back.
//! - Unknown message types are ignored with a warning; they never abort the
//!   stream, so both ends can evolve additively within a protocol version.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Version negotiated in the `initialize` handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// Launch specification for the ACP agent child process.
///
/// Credentials are injected through `env` at spawn time and are treated as
/// opaque strings (ADR-012).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunchSpec {
    /// Configured agent name (`cosh-core`, `claude`, ...).
    pub name: String,
    /// Executable path or PATH lookup name.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// MCP server entry forwarded verbatim into ACP `session/new`/`session/load`.
///
/// The bridge does not interpret these fields; cosh-shell owns the socket
/// path and token they may reference (ADR-012).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Client capabilities cosh-shell asks the bridge to advertise over ACP.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Advertise the ACP terminal capability so agent command execution
    /// routes through the shell (ADR-011 trust tiers).
    #[serde(default)]
    pub terminal: bool,
}

/// Agent capabilities discovered from the ACP `initialize` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(default)]
    pub resume: bool,
    #[serde(default)]
    pub close: bool,
}

/// Structured configuration carried by the first message on the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub agent: AgentLaunchSpec,
    /// Initial workspace directory for `session/new`; per-command cwd is
    /// resolved by the shell at execution time (ADR-012).
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerSpec>,
    #[serde(default)]
    pub capabilities: ClientCapabilities,
    #[serde(default)]
    pub locale: Option<String>,
}

/// One permission option offered by an agent `request_permission` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    /// ACP option kind (`allow_once`, `allow_always`, `reject_once`, ...).
    pub kind: String,
}

/// One authentication method advertised by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthMethodInfo {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Messages sent by cosh-shell to the bridge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ShellMessage {
    Initialize(Box<InitializeParams>),
    SessionNew {
        request_id: String,
        cwd: String,
    },
    SessionLoad {
        request_id: String,
        session_id: String,
    },
    Prompt {
        request_id: String,
        session_id: String,
        text: String,
        /// Per-prompt approval field replacing the old spawn-time
        /// `--approval-mode` (ADR-011 lifecycle model).
        approval_mode: String,
    },
    Cancel {
        session_id: String,
    },
    PermissionResponse {
        request_id: String,
        option_id: Option<String>,
        cancelled: bool,
    },
    AuthResponse {
        request_id: String,
        method_id: Option<String>,
        /// Field values for `_cosh/auth_challenge`; may contain secrets and
        /// must never be logged.
        #[serde(default)]
        values: BTreeMap<String, String>,
        #[serde(default)]
        cancelled: bool,
    },
    TerminalCreated {
        terminal_id: String,
    },
    TerminalOutput {
        terminal_id: String,
        chunk: String,
        #[serde(default)]
        truncated: bool,
    },
    TerminalExit {
        terminal_id: String,
        exit_code: Option<i32>,
        signal: Option<String>,
    },
    TerminalDenied {
        terminal_id: String,
        reason: String,
    },
    Shutdown,
}

/// Messages sent by the bridge to cosh-shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BridgeMessage {
    Initialized {
        protocol_version: u32,
        agent_capabilities: AgentCapabilities,
        #[serde(default)]
        auth_methods: Vec<AuthMethodInfo>,
    },
    SessionCreated {
        request_id: String,
        session_id: String,
    },
    SessionLoaded {
        request_id: String,
        session_id: String,
    },
    TextDelta {
        session_id: String,
        text: String,
    },
    ThoughtDelta {
        session_id: String,
        text: String,
    },
    ToolCall {
        session_id: String,
        tool_call_id: String,
        title: String,
        kind: String,
        status: String,
    },
    TerminalCreate {
        session_id: String,
        terminal_id: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    TerminalKill {
        terminal_id: String,
    },
    TerminalRelease {
        terminal_id: String,
    },
    PermissionRequest {
        session_id: String,
        request_id: String,
        title: String,
        options: Vec<PermissionOption>,
    },
    AuthRequired {
        request_id: String,
        methods: Vec<AuthMethodInfo>,
    },
    PromptCompleted {
        request_id: String,
        stop_reason: String,
    },
    AgentFailed {
        code: String,
        message: String,
        recoverable: bool,
        #[serde(default)]
        hint: Option<String>,
    },
}

impl BridgeMessage {
    /// Non-retryable protocol failure (version mismatch, malformed handshake).
    pub fn protocol_error(message: impl Into<String>) -> Self {
        Self::AgentFailed {
            code: "protocol_error".to_string(),
            message: message.into(),
            recoverable: false,
            hint: None,
        }
    }
}

/// Parses one JSONL line from cosh-shell.
///
/// # Errors
///
/// Returns the serde error message for malformed or unknown input; callers
/// decide whether the failure is fatal (handshake) or ignorable (post-init).
pub fn parse_shell_message(line: &str) -> Result<ShellMessage, String> {
    serde_json::from_str(line).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_line() -> String {
        serde_json::to_string(&ShellMessage::Initialize(Box::new(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            agent: AgentLaunchSpec {
                name: "cosh-core".to_string(),
                command: "cosh-core".to_string(),
                args: vec!["--acp".to_string()],
                env: BTreeMap::new(),
            },
            cwd: "/tmp".to_string(),
            mcp_servers: vec![],
            capabilities: ClientCapabilities { terminal: true },
            locale: Some("zh-CN".to_string()),
        })))
        .expect("serialize")
    }

    #[test]
    fn initialize_round_trips() {
        let line = initialize_line();
        let parsed = parse_shell_message(&line).expect("parse");
        let ShellMessage::Initialize(params) = parsed else {
            panic!("expected initialize");
        };
        assert_eq!(params.protocol_version, PROTOCOL_VERSION);
        assert!(params.capabilities.terminal);
        assert_eq!(params.agent.args, vec!["--acp".to_string()]);
    }

    #[test]
    fn method_tag_uses_snake_case() {
        let line = serde_json::to_string(&ShellMessage::SessionNew {
            request_id: "r1".to_string(),
            cwd: "/tmp".to_string(),
        })
        .expect("serialize");
        assert!(line.contains("\"method\":\"session_new\""), "{line}");
    }

    #[test]
    fn event_tag_uses_snake_case() {
        let line = serde_json::to_string(&BridgeMessage::PromptCompleted {
            request_id: "r1".to_string(),
            stop_reason: "end_turn".to_string(),
        })
        .expect("serialize");
        assert!(line.contains("\"event\":\"prompt_completed\""), "{line}");
    }

    #[test]
    fn unknown_method_is_a_parse_error() {
        assert!(parse_shell_message("{\"method\":\"warp_drive\"}").is_err());
        assert!(parse_shell_message("not json").is_err());
    }

    #[test]
    fn terminal_lifecycle_messages_round_trip() {
        let exit = ShellMessage::TerminalExit {
            terminal_id: "t1".to_string(),
            exit_code: Some(0),
            signal: None,
        };
        let line = serde_json::to_string(&exit).expect("serialize");
        assert_eq!(parse_shell_message(&line).expect("parse"), exit);
    }
}
