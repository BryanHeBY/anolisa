//! Adapter that talks to the cosh-acp bridge over the internal JSONL protocol.
//!
//! cosh-shell has no internal crate dependencies, so the wire format is
//! mirrored here by hand; `crates/cosh-acp/src/protocol.rs` is the source of
//! truth for the contract (protocol v1, ADR-011).
//!
//! Scope: handshake, prompt dispatch, event mapping, cancellation, bridge
//! process custody, and dual-lane terminal delegation (see `acp/terminal.rs`).
//! Permissions beyond command approval and auth land with S4.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::command::BackgroundLane;
use crate::types::{AgentEvent, AgentRequest, CoshApprovalMode};

mod terminal;
mod wire;

use self::terminal::{BridgeWriter, TerminalCreate};
use self::wire::{map_bridge_event, session_new_line, BridgeEvent};
use super::claude::{send_agent_event, terminate_process};
use super::prompt_from_request;
use super::{
    control_protocol, start_threaded_adapter_run, AdapterError, AdapterInstance, AgentAdapter,
    AgentBackendCapabilities, AgentRunHandle, ProviderCancellationArtifactStore,
};

/// Internal JSONL protocol version this adapter speaks.
pub(super) const ACP_BRIDGE_PROTOCOL_VERSION: u32 = 1;

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
            // Advertise terminal delegation: the shell executes agent
            // commands through the dual-lane executor, which keeps them on
            // the audited path (ADR-011 trust tiers).
            "capabilities": { "terminal": true },
            "locale": serde_json::Value::Null,
        });
        params.to_string()
    }

    /// Builds the prompt message for the session the bridge just created.
    ///
    /// The session id comes from the agent, not from the shell: the shell's
    /// own session id names a terminal session, not an agent transcript.
    fn prompt_line(request: &AgentRequest, session_id: &str, mode: CoshApprovalMode) -> String {
        let approval_mode = match mode {
            CoshApprovalMode::Recommend => "strict",
            CoshApprovalMode::Auto => "auto",
            CoshApprovalMode::Trust => "trust",
        };
        serde_json::json!({
            "method": "prompt",
            "request_id": request.id,
            "session_id": session_id,
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
    let (approval_tx, approval_rx) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let child_pid = Arc::new(Mutex::new(None::<u32>));
    let writer_slot: Arc<Mutex<Option<BridgeWriter>>> = Arc::new(Mutex::new(None));

    let cancel_flag = Arc::clone(&cancelled);
    let cancel_pid = Arc::clone(&child_pid);
    let cancel_writer = Arc::clone(&writer_slot);
    let cancel_session = request.session_id.clone();
    let cancel = Arc::new(move || {
        cancel_flag.store(true, Ordering::SeqCst);
        // Stage 1: protocol-level cancel so the agent can stop cleanly and
        // the bridge kills this session's live terminals (stage 2 happens
        // shell-side when the reader observes the flag).
        let sent = cancel_writer
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .map(|writer| {
                terminal::write_message(
                    &writer,
                    &serde_json::json!({ "method": "cancel", "session_id": cancel_session }),
                );
            })
            .is_some();
        // Stage 3: process escalation. Immediate when the protocol path is
        // unavailable, otherwise after a short grace so a cooperative agent
        // can still finish the turn with stop_reason=cancelled.
        let pid = cancel_pid.lock().ok().and_then(|guard| *guard);
        if let Some(pid) = pid {
            if sent {
                thread::spawn(move || {
                    thread::sleep(std::time::Duration::from_secs(2));
                    terminate_process(pid);
                });
            } else {
                terminate_process(pid);
            }
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

        let outcome = drive_bridge(
            &adapter,
            &request,
            mode,
            &mut child,
            &sender,
            &approval_rx,
            &writer_slot,
            &cancelled,
        );
        if let Ok(mut slot) = writer_slot.lock() {
            *slot = None;
        }
        let _ = child.wait();
        if let Err(error) = outcome {
            let _ = sender.send(Err(error));
        }
    });

    AgentRunHandle {
        receiver,
        cancel,
        approval_sender: Some(approval_tx),
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
/// Writes one protocol line, treating a write failure as a fatal turn error.
fn write_bridge_line(writer: &BridgeWriter, line: &str) -> Result<(), AdapterError> {
    let mut stdin = writer.lock().map_err(|_| AdapterError {
        message: "bridge writer poisoned".to_string(),
    })?;
    writeln!(stdin, "{line}").map_err(|error| AdapterError {
        message: format!("failed to write to bridge: {error}"),
    })
}

/// Writes the handshake and prompt, then maps bridge events until a terminal
/// event or stream end. Terminal lifecycle events route through the dual-lane
/// executor in `acp/terminal.rs`.
#[allow(clippy::too_many_arguments)]
fn drive_bridge(
    adapter: &AcpAdapter,
    request: &AgentRequest,
    mode: CoshApprovalMode,
    child: &mut Child,
    sender: &mpsc::Sender<Result<AgentEvent, AdapterError>>,
    approvals: &mpsc::Receiver<control_protocol::ApprovalResponse>,
    writer_slot: &Arc<Mutex<Option<BridgeWriter>>>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), AdapterError> {
    let stdin = child.stdin.take().ok_or_else(|| AdapterError {
        message: "failed to capture bridge stdin".to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| AdapterError {
        message: "failed to capture bridge stdout".to_string(),
    })?;
    let writer: BridgeWriter = Arc::new(Mutex::new(stdin));
    if let Ok(mut slot) = writer_slot.lock() {
        *slot = Some(Arc::clone(&writer));
    }

    // Only the handshake goes out now: the bridge mints the session id, so
    // session_new and prompt follow as its replies arrive.
    write_bridge_line(&writer, &adapter.initialize_line())?;

    // Background lane plus a pump that forwards its output/exit events to
    // the bridge while the reader below may be blocked on stdout.
    let lane = Arc::new(BackgroundLane::default());
    let pump_stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let lane = Arc::clone(&lane);
        let writer = Arc::clone(&writer);
        let stop = Arc::clone(&pump_stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                terminal::pump_lane_events(&lane, &writer);
                thread::sleep(terminal::LANE_PUMP_INTERVAL);
            }
            terminal::pump_lane_events(&lane, &writer);
        })
    };

    let run_id = request.id.clone();
    let mut terminal_seen = false;
    let mut lanes_killed = false;
    for line in BufReader::new(stdout).lines() {
        if cancelled.load(Ordering::SeqCst) && !lanes_killed {
            // Stage 2 of the three-stage cancellation: kill this run's
            // background commands; the turn still ends via the bridge.
            lane.kill_all();
            lanes_killed = true;
        }
        let line = line.map_err(|error| AdapterError {
            message: format!("failed to read bridge stream: {error}"),
        })?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<BridgeEvent>(&line) {
            Ok(BridgeEvent::TerminalCreate {
                terminal_id,
                command,
                args,
                env,
                cwd,
            }) => {
                terminal::handle_terminal_create(
                    &run_id,
                    TerminalCreate {
                        terminal_id,
                        command,
                        args,
                        env: env.into_iter().collect(),
                        cwd,
                    },
                    &writer,
                    &lane,
                    sender,
                    approvals,
                    cancelled,
                );
            }
            Ok(BridgeEvent::TerminalKill { terminal_id })
            | Ok(BridgeEvent::TerminalRelease { terminal_id }) => {
                lane.kill(&terminal_id);
            }
            Ok(BridgeEvent::Initialized { protocol_version }) => {
                send_agent_event(
                    sender,
                    AgentEvent::StatusChanged {
                        run_id: run_id.clone(),
                        phase: "connected".to_string(),
                        message: format!("cosh-acp bridge ready (protocol v{protocol_version})"),
                    },
                );
                write_bridge_line(&writer, &session_new_line())?;
            }
            Ok(BridgeEvent::SessionCreated { session_id }) => {
                send_agent_event(
                    sender,
                    AgentEvent::StatusChanged {
                        run_id: run_id.clone(),
                        phase: "session".to_string(),
                        message: format!("agent session {session_id} created"),
                    },
                );
                write_bridge_line(
                    &writer,
                    &AcpAdapter::prompt_line(request, &session_id, mode),
                )?;
            }
            Ok(BridgeEvent::PermissionRequest {
                request_id,
                title,
                options,
            }) => {
                terminal::handle_permission_request(
                    &run_id,
                    &request_id,
                    &title,
                    &options
                        .iter()
                        .map(|option| (option.id.as_str(), option.kind.as_str()))
                        .collect::<Vec<_>>(),
                    &writer,
                    sender,
                    approvals,
                    cancelled,
                );
            }
            Ok(event) => {
                if let Some(mapped) = map_bridge_event(&run_id, event, &mut terminal_seen) {
                    send_agent_event(sender, mapped);
                }
                if terminal_seen {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!("ignoring malformed bridge event: {error}");
            }
        }
    }
    lane.kill_all();
    pump_stop.store(true, Ordering::Release);
    let _ = pump.join();
    if !terminal_seen {
        if cancelled.load(Ordering::SeqCst) {
            send_agent_event(
                sender,
                AgentEvent::AgentCancelled {
                    run_id,
                    reason: "user requested cancellation".to_string(),
                },
            );
        } else {
            send_agent_event(
                sender,
                AgentEvent::AgentFailed {
                    run_id,
                    error: "bridge stream ended without a terminal event".to_string(),
                },
            );
        }
    }
    Ok(())
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
        let line = AcpAdapter::prompt_line(&request(), "agent-session", CoshApprovalMode::Trust);
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
