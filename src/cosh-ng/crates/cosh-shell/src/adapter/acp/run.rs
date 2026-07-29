//! Bridge process custody and the per-turn protocol drive loop.
//!
//! One bridge process serves one turn. Conversation continuity comes from
//! reloading the committed agent session rather than from keeping a process
//! alive, so custody stays simple: the turn owns the bridge and takes it down
//! with it (ADR-011).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::command::BackgroundLane;
use crate::evidence::service::{EvidenceService, EVIDENCE_SOCKET_ENV, EVIDENCE_TOKEN_ENV};
use crate::types::{AgentEvent, AgentRequest, CommandBlock, CoshApprovalMode};

use super::super::claude::{send_agent_event, terminate_process};
use super::super::{
    control_protocol, AdapterError, AgentRunHandle, ProviderCancellationArtifactStore,
};
use super::exec::mcp_run_command_executor;
use super::terminal::{self, ApprovalRouter, BridgeWriter, TerminalCreate};
use super::wire::{map_bridge_event, session_load_line, session_new_line, BridgeEvent};
use super::{AcpAdapter, ShellMcpServer};

pub(super) fn start_bridge_run(
    adapter: AcpAdapter,
    request: AgentRequest,
    mode: CoshApprovalMode,
) -> AgentRunHandle {
    let (sender, receiver) = mpsc::channel();
    let (approval_tx, approval_rx) = mpsc::channel();
    let router = Arc::new(ApprovalRouter::new(approval_rx));
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
        tracing::info!(
            agent_name = %adapter.agent_name,
            agent_command = %adapter.agent_command,
            agent_args = ?adapter.agent_args,
            agent_trusted = adapter.agent_trusted,
            injects_mcp = adapter.injects_shell_mcp(),
            bridge_program = %adapter.program,
            "starting ACP bridge turn"
        );
        send_agent_event(
            &sender,
            AgentEvent::StatusChanged {
                run_id: run_id.clone(),
                phase: "starting".to_string(),
                message: format!("starting cosh-acp bridge (agent: {})", adapter.agent_name),
            },
        );

        let mut child = match spawn_bridge(&adapter) {
            Ok(child) => {
                tracing::info!(pid = child.id(), "bridge process spawned");
                child
            }
            Err(message) => {
                tracing::warn!(%message, "failed to spawn bridge");
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

        // Evidence Service persists across turns so the socket and token stay
        // stable: the MCP server proxy spawned by the agent can always reach
        // the socket (ADR-012). First turn starts it; later turns reuse it.
        let mcp_servers = match adapter.evidence.lock() {
            Ok(mut slot) => {
                if slot.is_none() {
                    *slot = EvidenceService::start(&request.session_id).ok();
                }
                let mcp = build_mcp_servers(&adapter, slot.as_ref());
                if let Some(service) = slot.as_ref() {
                    service.update_blocks(evidence_blocks(&request));
                    let blocks_handle = service.blocks_handle();
                    let output_dir = service
                        .socket_path()
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join("output-refs");
                    service.set_executor(Some(mcp_run_command_executor(
                        &run_id,
                        &request.session_id,
                        blocks_handle,
                        output_dir,
                        sender.clone(),
                        Arc::clone(&router),
                        Arc::clone(&cancelled),
                    )));
                }
                mcp
            }
            Err(_) => Vec::new(),
        };

        let outcome = drive_bridge(
            &adapter,
            &request,
            mode,
            &mcp_servers,
            &mut child,
            &sender,
            &router,
            &writer_slot,
            &cancelled,
        );
        // Clear the per-turn executor but keep the service alive for reuse.
        if let Ok(slot) = adapter.evidence.lock() {
            if let Some(service) = slot.as_ref() {
                service.set_executor(None);
            }
        }
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
        control_capabilities: Arc::new(Mutex::new(control_protocol::ControlProtocolCapabilities {
            // A foreground command's result is replayed to the agent as a
            // terminal lifecycle (`acp/terminal.rs`), so the shell can hand
            // it back inline and keep the turn alive instead of cancelling
            // and re-prompting with an analysis-only continuation.
            can_handle_host_executed_shell_tool_result: true,
            ..Default::default()
        })),
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
    mcp_servers: &[ShellMcpServer],
    child: &mut Child,
    sender: &mpsc::Sender<Result<AgentEvent, AdapterError>>,
    router: &ApprovalRouter,
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
    let init_line = adapter.initialize_line(mcp_servers);
    tracing::info!(%init_line, "sending initialize to bridge");
    write_bridge_line(&writer, &init_line)?;

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
    let committed = adapter.session_id.lock().ok().and_then(|id| id.clone());
    // Session the bridge actually bound, committed only once the turn
    // completes so a failed turn cannot record an id the agent never kept.
    let mut bound_session: Option<String> = None;
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
        tracing::info!(%line, "bridge event");
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
                    router,
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
                // Reload the committed session so the conversation continues
                // across turns; a fresh bridge process is fine because the
                // transcript lives in the agent's session store.
                let line = match committed.as_deref() {
                    Some(session_id) => session_load_line(session_id),
                    None => session_new_line(),
                };
                write_bridge_line(&writer, &line)?;
            }
            Ok(BridgeEvent::SessionLoaded { session_id })
            | Ok(BridgeEvent::SessionCreated { session_id }) => {
                tracing::info!(%session_id, "agent session ready, sending prompt");
                send_agent_event(
                    sender,
                    AgentEvent::StatusChanged {
                        run_id: run_id.clone(),
                        phase: "session".to_string(),
                        message: format!("agent session {session_id} ready"),
                    },
                );
                write_bridge_line(&writer, &adapter.prompt_line(request, &session_id, mode))?;
                bound_session = Some(session_id);
            }
            Ok(BridgeEvent::PermissionRequest {
                request_id,
                title,
                kind,
                raw_input,
                options,
            }) => {
                terminal::handle_permission_request(
                    &run_id,
                    terminal::PermissionRequest {
                        request_id: &request_id,
                        title: &title,
                        kind: &kind,
                        raw_input: raw_input.as_ref(),
                    },
                    &options
                        .iter()
                        .map(|option| (option.id.as_str(), option.kind.as_str()))
                        .collect::<Vec<_>>(),
                    &writer,
                    sender,
                    router,
                    cancelled,
                );
            }
            Ok(event) => {
                let completed = matches!(event, BridgeEvent::PromptCompleted { .. });
                if let Some(mapped) = map_bridge_event(&run_id, event, &mut terminal_seen) {
                    send_agent_event(sender, mapped);
                }
                if completed {
                    if let (Some(session_id), Ok(mut slot)) =
                        (bound_session.as_ref(), adapter.session_id.lock())
                    {
                        *slot = Some(session_id.clone());
                    }
                }
                if terminal_seen {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(%line, %error, "malformed bridge event");
            }
        }
    }
    lane.kill_all();
    pump_stop.store(true, Ordering::Release);
    let _ = pump.join();
    if !terminal_seen {
        if cancelled.load(Ordering::SeqCst) {
            tracing::warn!("bridge stream ended: cancelled by user");
            send_agent_event(
                sender,
                AgentEvent::AgentCancelled {
                    run_id,
                    reason: "user requested cancellation".to_string(),
                },
            );
        } else {
            tracing::warn!("bridge stream ended without a terminal event");
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

/// Builds the MCP server list for the handshake. The `cosh-shell` server is
/// injected only when the adapter wants the shell MCP surface (ADR-012): it
/// exposes the evidence tools and `cosh_terminal` to agents that do not bring
/// their own audited execution path.
fn build_mcp_servers(
    adapter: &AcpAdapter,
    evidence: Option<&EvidenceService>,
) -> Vec<ShellMcpServer> {
    let Some(service) = evidence else {
        return Vec::new();
    };
    if !adapter.injects_shell_mcp() {
        return Vec::new();
    }
    vec![ShellMcpServer {
        name: "cosh-shell".to_string(),
        command: adapter.program.clone(),
        args: vec!["mcp-shell".to_string()],
        env: std::collections::BTreeMap::from([
            (
                EVIDENCE_SOCKET_ENV.to_string(),
                service.socket_path().to_string_lossy().into_owned(),
            ),
            (EVIDENCE_TOKEN_ENV.to_string(), service.token().to_string()),
        ]),
    }]
}

/// Assembles the command-history snapshot served to evidence clients for one
/// turn: the request's context blocks plus the current command block, deduped
/// by id so a block present in both is not double-counted.
fn evidence_blocks(request: &AgentRequest) -> Vec<CommandBlock> {
    let mut blocks: Vec<CommandBlock> = request.context_blocks.clone();
    if !blocks
        .iter()
        .any(|block| block.id == request.command_block.id)
    {
        blocks.push(request.command_block.clone());
    }
    blocks
}
