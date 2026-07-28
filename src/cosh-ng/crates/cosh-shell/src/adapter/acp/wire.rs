//! Wire mirrors and line builders for the internal bridge protocol.
//!
//! `crates/cosh-acp/src/protocol.rs` is the source of truth for the contract;
//! these types are mirrored by hand because cosh-shell has no internal crate
//! dependencies (protocol v1, ADR-011).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{AgentEvent, AgentRequest, CoshApprovalMode};

use super::super::prompt_from_request;
use super::{AcpAdapter, ACP_BRIDGE_PROTOCOL_VERSION};
/// Wire mirror of the bridge `initialize` parameters (protocol v1).
#[derive(Debug, Serialize)]
struct InitializeParams<'a> {
    protocol_version: u32,
    agent: LaunchSpec<'a>,
    cwd: String,
    mcp_servers: Vec<serde_json::Value>,
    capabilities: Capabilities,
    locale: Option<String>,
}

#[derive(Debug, Serialize)]
struct LaunchSpec<'a> {
    name: &'a str,
    command: &'a str,
    args: &'a [String],
    env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct Capabilities {
    terminal: bool,
}

/// Wire mirror of one permission option offered by the agent.
#[derive(Debug, Deserialize)]
pub(super) struct BridgePermissionOption {
    pub(super) id: String,
    /// ACP option kind (`allow_once`, `allow_always`, `reject_once`, ...).
    pub(super) kind: String,
}

/// Wire mirror of bridge events consumed in S1; unknown events are ignored
/// so the bridge can evolve additively.
#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(super) enum BridgeEvent {
    Initialized {
        protocol_version: u32,
    },
    SessionCreated {
        session_id: String,
    },
    SessionLoaded {
        session_id: String,
    },
    PermissionRequest {
        request_id: String,
        title: String,
        #[serde(default)]
        options: Vec<BridgePermissionOption>,
    },
    TextDelta {
        text: String,
    },
    ThoughtDelta {
        text: String,
    },
    PromptCompleted {
        stop_reason: String,
    },
    AgentLocalExec {
        pid: u32,
        command: String,
    },
    TerminalCreate {
        terminal_id: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    TerminalKill {
        terminal_id: String,
    },
    TerminalRelease {
        terminal_id: String,
    },
    AgentFailed {
        code: String,
        message: String,
        recoverable: bool,
        #[serde(default)]
        hint: Option<String>,
    },
    #[serde(other)]
    Unknown,
}
/// Asks the bridge to reload a previously committed agent session.
pub(super) fn session_load_line(session_id: &str) -> String {
    serde_json::json!({
        "method": "session_load",
        "request_id": "shell-session-load",
        "session_id": session_id,
    })
    .to_string()
}

/// Asks the bridge to create the agent session for this turn.
pub(super) fn session_new_line() -> String {
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    serde_json::json!({
        "method": "session_new",
        "request_id": "shell-session-new",
        "cwd": cwd,
    })
    .to_string()
}
pub(super) fn map_bridge_event(
    run_id: &str,
    event: BridgeEvent,
    terminal_seen: &mut bool,
) -> Option<AgentEvent> {
    match event {
        // Handshake and permission events are answered by the caller because
        // they write back to the bridge instead of producing an event here.
        BridgeEvent::Initialized { .. }
        | BridgeEvent::SessionCreated { .. }
        | BridgeEvent::SessionLoaded { .. }
        | BridgeEvent::PermissionRequest { .. } => None,
        BridgeEvent::TextDelta { text } => Some(AgentEvent::TextDelta {
            run_id: run_id.to_string(),
            text,
        }),
        BridgeEvent::ThoughtDelta { text } => Some(AgentEvent::StatusChanged {
            run_id: run_id.to_string(),
            phase: "thinking".to_string(),
            message: text,
        }),
        BridgeEvent::PromptCompleted { stop_reason } => {
            *terminal_seen = true;
            if stop_reason == "cancelled" {
                Some(AgentEvent::AgentCancelled {
                    run_id: run_id.to_string(),
                    reason: "agent confirmed cancellation".to_string(),
                })
            } else {
                Some(AgentEvent::AgentCompleted {
                    run_id: run_id.to_string(),
                    summary: format!("agent turn completed ({stop_reason})"),
                })
            }
        }
        BridgeEvent::TerminalCreate { .. }
        | BridgeEvent::TerminalKill { .. }
        | BridgeEvent::TerminalRelease { .. } => {
            // Routed by drive_bridge before mapping; unreachable here.
            None
        }
        BridgeEvent::AgentFailed {
            code,
            message,
            recoverable,
            hint,
        } => {
            *terminal_seen = true;
            let hint_suffix = hint
                .map(|hint| format!(" (hint: {hint})"))
                .unwrap_or_default();
            let recoverable_note = if recoverable { "recoverable" } else { "fatal" };
            Some(AgentEvent::AgentFailed {
                run_id: run_id.to_string(),
                error: format!("[{code}, {recoverable_note}] {message}{hint_suffix}"),
            })
        }
        // Tier 2/3 warning: the agent ran something itself, so that command
        // never reached the shell's safety gate or audit record.
        BridgeEvent::AgentLocalExec { pid, command } => Some(AgentEvent::StatusChanged {
            run_id: run_id.to_string(),
            phase: "agent_local_exec".to_string(),
            message: format!("agent ran '{command}' itself (pid {pid}); not audited by the shell"),
        }),
        BridgeEvent::Unknown => None,
    }
}
