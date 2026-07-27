//! Adapter that talks to the cosh-acp bridge over the internal JSONL protocol.
//!
//! cosh-shell has no internal crate dependencies, so the wire format is
//! mirrored here by hand; `crates/cosh-acp/src/protocol.rs` is the source of
//! truth for the contract (protocol v1, ADR-011).
//!
//! S1 scope: handshake, prompt dispatch, event mapping, cancellation, and
//! bridge process custody. Terminal delegation, permissions, and auth land
//! with the later migration stages.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::types::{AgentEvent, AgentRequest, CoshApprovalMode};

use super::claude::{send_agent_event, terminate_process};
use super::prompt_from_request;
use super::{
    control_protocol, start_threaded_adapter_run, AdapterError, AdapterInstance, AgentAdapter,
    AgentBackendCapabilities, AgentRunHandle, ProviderCancellationArtifactStore,
};

/// Internal JSONL protocol version this adapter speaks.
const ACP_BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// Adapter that delegates Agent turns to an ACP agent via the cosh-acp bridge.
#[derive(Debug, Clone)]
pub struct AcpAdapter {
    /// cosh-acp executable path.
    pub program: String,
    /// Configured agent name (used for diagnostics and trust tiering).
    pub agent_name: String,
    /// Agent launch command forwarded to the bridge.
    pub agent_command: String,
    /// Agent launch arguments forwarded to the bridge.
    pub agent_args: Vec<String>,
    /// Whether this adapter may start real bridge/agent processes.
    pub allow_spawn: bool,
}

impl Default for AcpAdapter {
    fn default() -> Self {
        Self {
            program: discover_binary("COSH_ACP_PATH", "cosh-acp"),
            agent_name: "cosh-core".to_string(),
            agent_command: discover_binary("COSH_CORE_PATH", "cosh-core"),
            agent_args: vec!["--acp".to_string()],
            allow_spawn: false,
        }
    }
}

/// Resolves a companion binary: env override, then sibling, then PATH.
fn discover_binary(env_var: &str, name: &str) -> String {
    if let Ok(path) = std::env::var(env_var) {
        if !path.is_empty() {
            return path;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling.to_string_lossy().into_owned();
            }
        }
    }
    name.to_string()
}

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

/// Wire mirror of bridge events consumed in S1; unknown events are ignored
/// so the bridge can evolve additively.
#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum BridgeEvent {
    Initialized {
        protocol_version: u32,
    },
    SessionCreated {
        session_id: String,
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

impl AcpAdapter {
    /// Creates an adapter for explicit bridge and agent executables.
    pub fn new(program: impl Into<String>, allow_spawn: bool) -> Self {
        Self {
            program: program.into(),
            allow_spawn,
            ..Self::default()
        }
    }

    fn initialize_line(&self) -> String {
        let cwd = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let params = serde_json::json!({
            "method": "initialize",
            "protocol_version": ACP_BRIDGE_PROTOCOL_VERSION,
            "agent": {
                "name": self.agent_name,
                "command": self.agent_command,
                "args": self.agent_args,
                "env": {},
            },
            "cwd": cwd,
            "mcp_servers": [],
            "capabilities": { "terminal": false },
            "locale": serde_json::Value::Null,
        });
        params.to_string()
    }

    fn prompt_line(request: &AgentRequest, mode: CoshApprovalMode) -> String {
        let approval_mode = match mode {
            CoshApprovalMode::Recommend => "strict",
            CoshApprovalMode::Auto => "auto",
            CoshApprovalMode::Trust => "trust",
        };
        serde_json::json!({
            "method": "prompt",
            "request_id": request.id,
            "session_id": request.session_id,
            "text": prompt_from_request(request),
            "approval_mode": approval_mode,
        })
        .to_string()
    }

    /// Starts a cancellable bridge turn.
    pub fn start_cancellable(
        &self,
        request: AgentRequest,
        mode: CoshApprovalMode,
    ) -> AgentRunHandle {
        if !self.allow_spawn {
            let adapter = AdapterInstance::Acp(self.clone());
            return start_threaded_adapter_run(adapter, request);
        }
        start_bridge_run(self.clone(), request, mode)
    }
}

impl AgentAdapter for AcpAdapter {
    fn name(&self) -> &'static str {
        "acp"
    }

    fn capabilities(&self) -> AgentBackendCapabilities {
        AgentBackendCapabilities {
            text_stream: true,
            thinking_stream: true,
            session_resume: false,
            tool_intent: false,
            user_question: false,
            cancellable: true,
            control_protocol: false,
        }
    }

    fn run(&self, request: &AgentRequest) -> Result<Vec<AgentEvent>, AdapterError> {
        // Dry-run mode: report the prepared invocation without spawning.
        Ok(vec![AgentEvent::StatusChanged {
            run_id: request.id.clone(),
            phase: "prepared".to_string(),
            message: format!(
                "cosh-acp bridge prepared: {} bridge (agent: {} {})",
                self.program,
                self.agent_command,
                self.agent_args.join(" ")
            ),
        }])
    }
}

fn start_bridge_run(
    adapter: AcpAdapter,
    request: AgentRequest,
    mode: CoshApprovalMode,
) -> AgentRunHandle {
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let child_pid = Arc::new(Mutex::new(None::<u32>));

    let cancel_flag = Arc::clone(&cancelled);
    let cancel_pid = Arc::clone(&child_pid);
    let cancel = Arc::new(move || {
        cancel_flag.store(true, Ordering::SeqCst);
        if let Some(pid) = cancel_pid.lock().ok().and_then(|guard| *guard) {
            terminate_process(pid);
        }
    });

    thread::spawn(move || {
        let run_id = request.id.clone();
        send_agent_event(
            &sender,
            AgentEvent::StatusChanged {
                run_id: run_id.clone(),
                phase: "starting".to_string(),
                message: format!("starting cosh-acp bridge (agent: {})", adapter.agent_name),
            },
        );

        let mut child = match spawn_bridge(&adapter) {
            Ok(child) => child,
            Err(message) => {
                let _ = sender.send(Err(AdapterError { message }));
                return;
            }
        };
        if let Ok(mut pid) = child_pid.lock() {
            *pid = Some(child.id());
        }
        if cancelled.load(Ordering::SeqCst) {
            terminate_process(child.id());
        }

        let outcome = drive_bridge(&adapter, &request, mode, &mut child, &sender, &cancelled);
        let _ = child.wait();
        if let Err(error) = outcome {
            let _ = sender.send(Err(error));
        }
    });

    AgentRunHandle {
        receiver,
        cancel,
        approval_sender: None,
        question_answer_confirmation: None,
        auth_sender: None,
        control_capabilities: Arc::new(Mutex::new(
            control_protocol::ControlProtocolCapabilities::default(),
        )),
        pending_provider_session: None,
        cancellation_artifacts: ProviderCancellationArtifactStore::default(),
    }
}

fn spawn_bridge(adapter: &AcpAdapter) -> Result<Child, String> {
    Command::new(&adapter.program)
        .arg("bridge")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!(
                "failed to spawn cosh-acp bridge '{}': {error}",
                adapter.program
            )
        })
}

/// Writes the handshake and prompt, then maps bridge events until a terminal
/// event or stream end.
fn drive_bridge(
    adapter: &AcpAdapter,
    request: &AgentRequest,
    mode: CoshApprovalMode,
    child: &mut Child,
    sender: &mpsc::Sender<Result<AgentEvent, AdapterError>>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), AdapterError> {
    let mut stdin = child.stdin.take().ok_or_else(|| AdapterError {
        message: "failed to capture bridge stdin".to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| AdapterError {
        message: "failed to capture bridge stdout".to_string(),
    })?;

    writeln!(stdin, "{}", adapter.initialize_line()).map_err(|error| AdapterError {
        message: format!("failed to write initialize: {error}"),
    })?;
    writeln!(stdin, "{}", AcpAdapter::prompt_line(request, mode)).map_err(|error| {
        AdapterError {
            message: format!("failed to write prompt: {error}"),
        }
    })?;

    let run_id = request.id.clone();
    let mut terminal_seen = false;
    for line in BufReader::new(stdout).lines() {
        if cancelled.load(Ordering::SeqCst) {
            send_agent_event(
                sender,
                AgentEvent::AgentCancelled {
                    run_id: run_id.clone(),
                    reason: "user requested cancellation".to_string(),
                },
            );
            return Ok(());
        }
        let line = line.map_err(|error| AdapterError {
            message: format!("failed to read bridge stream: {error}"),
        })?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<BridgeEvent>(&line) {
            Ok(event) => {
                if let Some(mapped) = map_bridge_event(&run_id, event, &mut terminal_seen) {
                    send_agent_event(sender, mapped);
                }
                if terminal_seen {
                    return Ok(());
                }
            }
            Err(error) => {
                tracing::warn!("ignoring malformed bridge event: {error}");
            }
        }
    }
    if !terminal_seen {
        send_agent_event(
            sender,
            AgentEvent::AgentFailed {
                run_id,
                error: "bridge stream ended without a terminal event".to_string(),
            },
        );
    }
    Ok(())
}

fn map_bridge_event(
    run_id: &str,
    event: BridgeEvent,
    terminal_seen: &mut bool,
) -> Option<AgentEvent> {
    match event {
        BridgeEvent::Initialized { protocol_version } => Some(AgentEvent::StatusChanged {
            run_id: run_id.to_string(),
            phase: "connected".to_string(),
            message: format!("cosh-acp bridge ready (protocol v{protocol_version})"),
        }),
        BridgeEvent::SessionCreated { session_id } => Some(AgentEvent::StatusChanged {
            run_id: run_id.to_string(),
            phase: "session".to_string(),
            message: format!("agent session {session_id} created"),
        }),
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
            Some(AgentEvent::AgentCompleted {
                run_id: run_id.to_string(),
                summary: format!("agent turn completed ({stop_reason})"),
            })
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
        BridgeEvent::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentMode, CommandBlock, CommandStatus, OutputRefs};

    fn request() -> AgentRequest {
        AgentRequest {
            id: "run-1".to_string(),
            session_id: "session-1".to_string(),
            command_block: CommandBlock {
                id: "blk".to_string(),
                session_id: "session-1".to_string(),
                command: "echo test".to_string(),
                origin: Default::default(),
                cwd: "/tmp".to_string(),
                end_cwd: "/tmp".to_string(),
                started_at_ms: 0,
                ended_at_ms: 0,
                duration_ms: 0,
                exit_code: 1,
                status: CommandStatus::Failed,
                output: OutputRefs {
                    terminal_output_ref: None,
                    terminal_output_bytes: 0,
                },
                shell_environment_generation: None,
                audit_identity: None,
            },
            context_blocks: Vec::new(),
            context_hints: Vec::new(),
            user_input: Some("why did it fail".to_string()),
            findings: Vec::new(),
            mode: AgentMode::RecommendOnly,
            user_confirmed: false,
            hook_finding: None,
            recommended_skill: None,
        }
    }

    #[test]
    fn initialize_line_declares_protocol_v1() {
        let adapter = AcpAdapter::default();
        let line = adapter.initialize_line();
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["agent"]["args"][0], "--acp");
    }

    #[test]
    fn prompt_line_carries_approval_mode() {
        let line = AcpAdapter::prompt_line(&request(), CoshApprovalMode::Trust);
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["method"], "prompt");
        assert_eq!(value["approval_mode"], "trust");
        assert_eq!(value["request_id"], "run-1");
    }

    #[test]
    fn text_delta_maps_to_agent_event() {
        let mut terminal_seen = false;
        let event = map_bridge_event(
            "run-1",
            BridgeEvent::TextDelta {
                text: "hello".to_string(),
            },
            &mut terminal_seen,
        );
        assert_eq!(
            event,
            Some(AgentEvent::TextDelta {
                run_id: "run-1".to_string(),
                text: "hello".to_string(),
            })
        );
        assert!(!terminal_seen);
    }

    #[test]
    fn agent_failed_is_terminal_and_keeps_code() {
        let mut terminal_seen = false;
        let event = map_bridge_event(
            "run-1",
            BridgeEvent::AgentFailed {
                code: "not_implemented".to_string(),
                message: "no wiring".to_string(),
                recoverable: true,
                hint: None,
            },
            &mut terminal_seen,
        );
        assert!(terminal_seen);
        let Some(AgentEvent::AgentFailed { error, .. }) = event else {
            panic!("expected failure event");
        };
        assert!(error.contains("not_implemented"), "{error}");
        assert!(error.contains("recoverable"), "{error}");
    }

    #[test]
    fn unknown_bridge_events_are_ignored() {
        let parsed: BridgeEvent =
            serde_json::from_str("{\"event\":\"future_thing\",\"x\":1}").expect("parse");
        let mut terminal_seen = false;
        assert_eq!(map_bridge_event("run-1", parsed, &mut terminal_seen), None);
        assert!(!terminal_seen);
    }

    #[test]
    fn dry_run_reports_prepared_invocation() {
        let adapter = AcpAdapter::default();
        let events = adapter.run(&request()).expect("dry run");
        assert_eq!(events.len(), 1);
        let AgentEvent::StatusChanged { phase, message, .. } = &events[0] else {
            panic!("expected status event");
        };
        assert_eq!(phase, "prepared");
        assert!(message.contains("bridge"), "{message}");
    }
}
