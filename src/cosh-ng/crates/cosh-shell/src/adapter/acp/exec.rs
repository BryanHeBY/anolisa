//! Shell-side executor for `cosh_terminal` MCP tool requests (ADR-012).
//!
//! Same dual-lane routing as the ACP terminal path (`acp/terminal.rs`): the
//! unchanged safety gate picks the lane, auto-allowed read-only commands run
//! silently in the background lane, and anything else raises one approval card
//! through the shared `ApprovalRouter`. Output is redacted with the evidence
//! service's exit-redaction pipeline before it leaves the shell, so the proxy
//! (and through it the agent) never sees unredacted data.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::command::{BackgroundLane, LaneEvent, LaneRequest};
use crate::evidence::service::{data_response, error_response, truncate_utf8};
use crate::evidence::{clean_terminal_control_sequences, redact_sensitive_output};
use crate::types::{
    AgentEvent, CommandBlock, CommandOrigin, CommandStatus, OutputRefs,
    COMMAND_OUTPUT_REF_MAX_BYTES,
};

use super::super::control_protocol::{ApprovalDecision, ApprovalResponse};
use super::super::AdapterError;
use super::terminal::{choose_lane, ApprovalRouter, Lane};

/// Output byte cap for one `cosh_terminal` result (matches evidence default).
const MAX_OUTPUT_BYTES: usize = 12 * 1024;
/// Upper bound on command execution before it is abandoned as hung.
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(600);
/// Poll granularity while waiting for lane exit so cancellation stays live.
const EXIT_POLL: Duration = Duration::from_millis(50);

/// Builds the executor installed on the evidence service for one turn.
pub(super) fn mcp_run_command_executor(
    run_id: &str,
    session_id: &str,
    blocks: Arc<Mutex<Vec<CommandBlock>>>,
    output_dir: PathBuf,
    events: mpsc::Sender<Result<AgentEvent, AdapterError>>,
    router: Arc<ApprovalRouter>,
    cancelled: Arc<AtomicBool>,
) -> super::terminal::RunCommandExecutor {
    let run_id = run_id.to_string();
    let session_id = session_id.to_string();
    Arc::new(move |params: &Value| {
        execute_run_command(
            &run_id,
            &session_id,
            &blocks,
            &output_dir,
            params,
            &events,
            &router,
            &cancelled,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_run_command(
    run_id: &str,
    session_id: &str,
    blocks: &Arc<Mutex<Vec<CommandBlock>>>,
    output_dir: &Path,
    params: &Value,
    events: &mpsc::Sender<Result<AgentEvent, AdapterError>>,
    router: &ApprovalRouter,
    cancelled: &AtomicBool,
) -> Value {
    let Some(command) = params.get("command").and_then(Value::as_str) else {
        return error_response("bad_request", "missing command");
    };
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);

    match choose_lane(command) {
        Lane::Denied { reason } => error_response("denied", &reason),
        Lane::Background => run_captured(
            command,
            cwd.as_deref(),
            cancelled,
            session_id,
            blocks,
            output_dir,
        ),
        Lane::Approval { reason } => {
            let request_id = format!("cosh-terminal-{}", next_correlation_id());
            let sent = events.send(Ok(AgentEvent::ToolPermissionRequest {
                run_id: run_id.to_string(),
                request_id: request_id.clone(),
                tool_name: "Bash".to_string(),
                tool_input: json!({
                    "command": command,
                    "reason": reason,
                }),
                tool_use_id: request_id.clone(),
                hook_requires_approval: false,
                audit_ref: None,
            }));
            if sent.is_err() {
                return error_response("unavailable", "shell is shutting down");
            }
            match router.wait(&request_id, cancelled) {
                Some(ApprovalDecision::Allow) => run_captured(
                    command,
                    cwd.as_deref(),
                    cancelled,
                    session_id,
                    blocks,
                    output_dir,
                ),
                Some(ApprovalDecision::Deny { message }) => error_response("denied", &message),
                Some(_) => error_response("denied", "approval returned an unrelated decision"),
                None => error_response("denied", "command was not approved in time"),
            }
        }
    }
}

fn run_captured(
    command: &str,
    cwd: Option<&str>,
    cancelled: &AtomicBool,
    session_id: &str,
    blocks: &Arc<Mutex<Vec<CommandBlock>>>,
    output_dir: &Path,
) -> Value {
    let correlation_id = next_correlation_id();
    let block_id = format!("mcp-cmd-{correlation_id}");
    let terminal_id = format!("mcp-{correlation_id}");
    let started_at = SystemTime::now();
    let lane = BackgroundLane::default();
    let request = LaneRequest {
        terminal_id: terminal_id.clone(),
        command: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), command.to_string()],
        env: Vec::new(),
        cwd: cwd.map(str::to_string),
    };
    if let Err(reason) = lane.spawn(&request) {
        return error_response("spawn_failed", &reason);
    }

    let deadline = Instant::now() + EXECUTION_TIMEOUT;
    let mut output = String::new();
    let mut exit_code: Option<i32> = None;
    let mut signal: Option<String> = None;
    while Instant::now() < deadline {
        if cancelled.load(Ordering::Acquire) {
            lane.kill(&terminal_id);
        }
        let mut saw_exit = false;
        for event in lane.drain_events() {
            match event {
                LaneEvent::Output { chunk, .. } => output.push_str(&chunk),
                LaneEvent::Exit {
                    exit_code: code,
                    signal: sig,
                    ..
                } => {
                    exit_code = code;
                    signal = sig;
                    saw_exit = true;
                }
            }
        }
        if saw_exit {
            break;
        }
        std::thread::sleep(EXIT_POLL);
    }
    if exit_code.is_none() {
        lane.kill_all();
        return error_response("timeout", "command did not finish within the time budget");
    }

    let ended_at = SystemTime::now();
    let cleaned = clean_terminal_control_sequences(&output);
    let (redacted, was_redacted) = redact_sensitive_output(&cleaned);
    let (text, truncated) = truncate_utf8(&redacted, MAX_OUTPUT_BYTES);
    let exit_code = exit_code.unwrap_or(-1);

    record_execution_block(
        blocks,
        output_dir,
        &block_id,
        session_id,
        command,
        cwd.unwrap_or(""),
        started_at,
        ended_at,
        exit_code,
        &redacted,
    );

    data_response(json!({
        "command": command,
        "exit_code": exit_code,
        "signal": signal,
        "output": text,
        "truncated": truncated,
        "redacted": was_redacted,
    }))
}

/// Writes the redacted output to a file and appends a `CommandBlock` to the
/// evidence service's blocks list so `list_shell_commands` and
/// `read_command_output` can see the execution for the rest of the turn.
#[allow(clippy::too_many_arguments)]
fn record_execution_block(
    blocks: &Arc<Mutex<Vec<CommandBlock>>>,
    output_dir: &Path,
    block_id: &str,
    session_id: &str,
    command: &str,
    cwd: &str,
    started_at: SystemTime,
    ended_at: SystemTime,
    exit_code: i32,
    redacted_output: &str,
) {
    let (file_output, _) = truncate_utf8(redacted_output, COMMAND_OUTPUT_REF_MAX_BYTES);
    let output_path = output_dir.join(format!("{block_id}.txt"));
    let wrote_file = match std::fs::create_dir_all(output_dir) {
        Ok(()) => {
            std::fs::set_permissions(output_dir, std::fs::Permissions::from_mode(0o700)).ok();
            match std::fs::write(&output_path, file_output.as_bytes()) {
                Ok(()) => {
                    std::fs::set_permissions(&output_path, std::fs::Permissions::from_mode(0o600))
                        .ok();
                    true
                }
                Err(error) => {
                    tracing::warn!(%block_id, %error, "failed to write evidence output file");
                    false
                }
            }
        }
        Err(error) => {
            tracing::warn!(%block_id, %error, "failed to create evidence output directory");
            false
        }
    };

    let terminal_output_ref = if wrote_file {
        Some(output_path.to_string_lossy().into_owned())
    } else {
        None
    };

    let started_at_ms = started_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let ended_at_ms = ended_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let block = CommandBlock {
        id: block_id.to_string(),
        session_id: session_id.to_string(),
        command: command.to_string(),
        origin: CommandOrigin::ProviderTool,
        cwd: cwd.to_string(),
        end_cwd: cwd.to_string(),
        started_at_ms,
        ended_at_ms,
        duration_ms: ended_at_ms.saturating_sub(started_at_ms),
        exit_code,
        status: if exit_code == 0 {
            CommandStatus::Completed
        } else {
            CommandStatus::Failed
        },
        output: OutputRefs {
            terminal_output_ref,
            terminal_output_bytes: file_output.len() as u64,
        },
        shell_environment_generation: None,
        audit_identity: None,
    };

    if let Ok(mut guard) = blocks.lock() {
        guard.push(block);
    }
}

/// Monotonic correlation id for `cosh_terminal` requests. Guarded so the
/// counter survives concurrent invocations from overlapping MCP tool calls.
fn next_correlation_id() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::super::terminal::RunCommandExecutor;
    use super::*;

    fn router() -> Arc<ApprovalRouter> {
        let (_tx, rx) = mpsc::channel::<ApprovalResponse>();
        Arc::new(ApprovalRouter::new(rx))
    }

    fn test_blocks() -> Arc<Mutex<Vec<CommandBlock>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn test_output_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cosh-exec-test-{}-{}",
            std::process::id(),
            next_correlation_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[allow(clippy::type_complexity)]
    fn make_executor(
        cancelled: Arc<AtomicBool>,
    ) -> (
        RunCommandExecutor,
        Arc<Mutex<Vec<CommandBlock>>>,
        PathBuf,
        mpsc::Receiver<Result<AgentEvent, AdapterError>>,
    ) {
        let (tx, rx) = mpsc::channel();
        let blocks = test_blocks();
        let output_dir = test_output_dir();
        let exec = mcp_run_command_executor(
            "run-1",
            "test-session",
            Arc::clone(&blocks),
            output_dir.clone(),
            tx,
            router(),
            cancelled,
        );
        (exec, blocks, output_dir, rx)
    }

    #[test]
    fn missing_command_is_bad_request() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (exec, _, _, _rx) = make_executor(cancelled);
        let response = exec(&json!({}));
        assert_eq!(response["error"]["code"], "bad_request", "{response}");
    }

    #[test]
    fn blocked_commands_are_denied_without_spawning() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (exec, blocks, _, _rx) = make_executor(cancelled);
        let response = exec(&json!({ "command": "rm -rf /" }));
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(
            response["error"]["code"], "denied",
            "rm -rf must be denied or blocked, got: {response}"
        );
        assert!(
            blocks.lock().unwrap().is_empty(),
            "denied commands must not record a block"
        );
    }

    #[test]
    fn readonly_command_runs_and_reports_exit_output() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (exec, _, _, _rx) = make_executor(cancelled);
        let response = exec(&json!({ "command": "echo hello-mcp" }));
        assert_eq!(response["ok"], true, "{response}");
        let output = response["data"]["output"].as_str().expect("output");
        assert!(output.contains("hello-mcp"), "{output}");
        assert_eq!(response["data"]["exit_code"], 0);
    }

    #[test]
    fn cancellation_kills_in_flight_command() {
        let cancelled = Arc::new(AtomicBool::new(true));
        let (exec, _, _, _rx) = make_executor(cancelled);
        let response = exec(&json!({ "command": "sleep 30" }));
        let code = response["error"]["code"].as_str().unwrap_or("");
        assert!(
            code == "timeout" || code == "denied",
            "expected timeout or denied, got: {response}"
        );
    }

    #[test]
    fn execution_records_block_in_evidence() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (exec, blocks, _output_dir, _rx) = make_executor(cancelled);
        let response = exec(&json!({ "command": "echo block-test" }));
        assert_eq!(response["ok"], true, "{response}");

        let guard = blocks.lock().unwrap();
        assert_eq!(guard.len(), 1, "exactly one block should be recorded");
        let block = &guard[0];
        assert!(block.id.starts_with("mcp-cmd-"), "block id: {}", block.id);
        assert_eq!(block.command, "echo block-test");
        assert_eq!(block.exit_code, 0);
        assert_eq!(block.status, CommandStatus::Completed);
        assert_eq!(block.origin, CommandOrigin::ProviderTool);
        assert_eq!(block.session_id, "test-session");
        assert!(
            block.output.terminal_output_ref.is_some(),
            "terminal_output_ref must be set"
        );
        let ref_path = block.output.terminal_output_ref.as_ref().unwrap();
        let file_content = std::fs::read_to_string(ref_path).expect("output file must be readable");
        assert!(
            file_content.contains("block-test"),
            "output file content: {file_content}"
        );
    }

    #[test]
    fn failed_command_records_failed_status() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (exec, blocks, _, _rx) = make_executor(cancelled);
        let response = exec(&json!({ "command": "ls /nonexistent" }));
        assert_eq!(response["ok"], true, "{response}");
        assert_ne!(
            response["data"]["exit_code"], 0,
            "ls /nonexistent must fail"
        );

        let guard = blocks.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].status, CommandStatus::Failed);
        assert_ne!(guard[0].exit_code, 0);
    }
}
