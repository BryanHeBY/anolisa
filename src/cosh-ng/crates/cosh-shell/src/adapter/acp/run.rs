//! Bridge process custody and the per-turn protocol drive loop.
//!
//! One bridge process serves one turn. Conversation continuity comes from
//! reloading the committed agent session rather than from keeping a process
//! alive, so custody stays simple: the turn owns the bridge and takes it down
//! with it (ADR-011).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::command::BackgroundLane;
use crate::types::{AgentEvent, AgentRequest, CoshApprovalMode};

use super::super::claude::{send_agent_event, terminate_process};
use super::super::{
    control_protocol, AdapterError, AgentRunHandle, ProviderCancellationArtifactStore,
};
use super::terminal::{self, BridgeWriter, TerminalCreate};
use super::wire::{map_bridge_event, session_load_line, session_new_line, BridgeEvent};
use super::AcpAdapter;

pub(super) fn start_bridge_run(
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
                send_agent_event(
                    sender,
                    AgentEvent::StatusChanged {
                        run_id: run_id.clone(),
                        phase: "session".to_string(),
                        message: format!("agent session {session_id} ready"),
                    },
                );
                write_bridge_line(
                    &writer,
                    &AcpAdapter::prompt_line(request, &session_id, mode),
                )?;
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
                    approvals,
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
