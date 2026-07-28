//! Shell-side terminal lane routing for the ACP adapter (ADR-011).
//!
//! Decides which lane a delegated `terminal/create` takes and reports the
//! outcome back to the bridge:
//!
//! - the existing `assess_shell_command` gate blocks what must not run;
//! - auto-allowed read-only commands go straight to the background lane;
//! - anything else raises one approval card, and an approved shell command is
//!   executed by the foreground handoff the runtime already owns, whose result
//!   comes back as `ApprovalDecision::HostExecutedShell`.
//!
//! Lane output and exits are written back as protocol messages, so the bridge
//! (and through it the agent) sees one uniform terminal lifecycle regardless
//! of which lane ran the command.

use std::io::Write;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::command::{BackgroundLane, LaneEvent, LaneRequest};
use crate::tools::{
    assess_shell_command, AssessmentSource, AutoExecutionPolicy, AutoExecutionRoute,
};
use crate::types::AgentEvent;

use super::super::control_protocol::{ApprovalDecision, ApprovalResponse};
use super::super::AdapterError;

/// How long an approval card may stay unanswered before the terminal is
/// denied. Matches the shell handoff patience budget.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
/// Poll granularity while waiting so cancellation stays responsive.
const APPROVAL_POLL: Duration = Duration::from_millis(100);
/// Interval at which lane events are flushed to the bridge.
pub(super) const LANE_PUMP_INTERVAL: Duration = Duration::from_millis(20);

/// Shared, line-buffered writer to the bridge.
pub(super) type BridgeWriter = Arc<Mutex<ChildStdin>>;

/// One delegated terminal as seen by the shell.
pub(super) struct TerminalCreate {
    pub(super) terminal_id: String,
    pub(super) command: String,
    pub(super) args: Vec<String>,
    pub(super) env: Vec<(String, String)>,
    pub(super) cwd: Option<String>,
}

impl TerminalCreate {
    /// Human-readable command line used for assessment and approval display.
    fn command_line(&self) -> String {
        if self.args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.args.join(" "))
        }
    }
}

/// Writes one protocol message to the bridge.
pub(super) fn write_message(writer: &BridgeWriter, message: &serde_json::Value) {
    let Ok(mut stdin) = writer.lock() else { return };
    let _ = writeln!(stdin, "{message}");
    let _ = stdin.flush();
}

fn terminal_created(writer: &BridgeWriter, terminal_id: &str) {
    write_message(
        writer,
        &serde_json::json!({ "method": "terminal_created", "terminal_id": terminal_id }),
    );
}

fn terminal_denied(writer: &BridgeWriter, terminal_id: &str, reason: &str) {
    write_message(
        writer,
        &serde_json::json!({
            "method": "terminal_denied",
            "terminal_id": terminal_id,
            "reason": reason,
        }),
    );
}

fn terminal_output(writer: &BridgeWriter, terminal_id: &str, chunk: &str) {
    write_message(
        writer,
        &serde_json::json!({
            "method": "terminal_output",
            "terminal_id": terminal_id,
            "chunk": chunk,
            "truncated": false,
        }),
    );
}

fn terminal_exit(
    writer: &BridgeWriter,
    terminal_id: &str,
    exit_code: Option<i32>,
    signal: Option<&str>,
) {
    write_message(
        writer,
        &serde_json::json!({
            "method": "terminal_exit",
            "terminal_id": terminal_id,
            "exit_code": exit_code,
            "signal": signal,
        }),
    );
}

/// Lane chosen for one delegated command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Lane {
    /// Run hidden, streaming output back (auto-allowed read-only commands).
    Background,
    /// Raise one approval card first; approval routes to the foreground
    /// handoff the runtime owns.
    Approval { reason: String },
    /// Refused by the safety gate; never runs.
    Denied { reason: String },
}

/// Applies the unchanged safety gate to pick a lane.
pub(super) fn choose_lane(command_line: &str) -> Lane {
    let policy = AutoExecutionPolicy::current_runtime();
    let assessment = assess_shell_command(
        command_line,
        policy.assessment_policy(AssessmentSource::ProviderShellTool),
    );
    match policy.route(&assessment) {
        AutoExecutionRoute::Block => Lane::Denied {
            reason: format!(
                "blocked by the shell safety gate: {}",
                assessment
                    .reasons
                    .first()
                    .copied()
                    .unwrap_or("command is not permitted")
            ),
        },
        AutoExecutionRoute::DirectReadonlyBroker => Lane::Background,
        // Guarded/pipeline executors are not enabled in the runtime policy;
        // treat them like AskUser rather than silently widening auto-exec.
        _ => Lane::Approval {
            reason: assessment
                .reasons
                .first()
                .copied()
                .unwrap_or("command needs user approval")
                .to_string(),
        },
    }
}

/// Routes one `terminal/create` to its lane and reports the outcome.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_terminal_create(
    run_id: &str,
    create: TerminalCreate,
    writer: &BridgeWriter,
    lane: &BackgroundLane,
    events: &mpsc::Sender<Result<AgentEvent, AdapterError>>,
    approvals: &mpsc::Receiver<ApprovalResponse>,
    cancelled: &Arc<AtomicBool>,
) {
    let command_line = create.command_line();
    match choose_lane(&command_line) {
        Lane::Denied { reason } => {
            terminal_denied(writer, &create.terminal_id, &reason);
        }
        Lane::Background => start_background(&create, writer, lane),
        Lane::Approval { reason } => {
            let decision = request_approval(
                run_id,
                &create.terminal_id,
                &command_line,
                &reason,
                events,
                approvals,
                cancelled,
            );
            match decision {
                Some(ApprovalDecision::Allow) => start_background(&create, writer, lane),
                Some(ApprovalDecision::HostExecutedShell { result }) => {
                    // Foreground lane: the command already ran in the user's
                    // PTY, so replay its result as a terminal lifecycle.
                    terminal_created(writer, &create.terminal_id);
                    if !result.llm_content.is_empty() {
                        terminal_output(writer, &create.terminal_id, &result.llm_content);
                    }
                    terminal_exit(
                        writer,
                        &create.terminal_id,
                        Some(result.metadata.exit_code),
                        result.metadata.signal.as_deref(),
                    );
                }
                Some(ApprovalDecision::Deny { message }) => {
                    terminal_denied(writer, &create.terminal_id, &message);
                }
                Some(_) => terminal_denied(
                    writer,
                    &create.terminal_id,
                    "approval returned an unrelated decision",
                ),
                None => terminal_denied(
                    writer,
                    &create.terminal_id,
                    "command was not approved in time",
                ),
            }
        }
    }
}

fn start_background(create: &TerminalCreate, writer: &BridgeWriter, lane: &BackgroundLane) {
    let request = LaneRequest {
        terminal_id: create.terminal_id.clone(),
        command: create.command.clone(),
        args: create.args.clone(),
        env: create.env.clone(),
        cwd: create.cwd.clone(),
    };
    match lane.spawn(&request) {
        Ok(()) => terminal_created(writer, &create.terminal_id),
        // A failed spawn must not look like a running terminal.
        Err(reason) => terminal_denied(writer, &create.terminal_id, &reason),
    }
}

/// Raises one approval card and waits for the runtime's decision.
fn request_approval(
    run_id: &str,
    terminal_id: &str,
    command_line: &str,
    reason: &str,
    events: &mpsc::Sender<Result<AgentEvent, AdapterError>>,
    approvals: &mpsc::Receiver<ApprovalResponse>,
    cancelled: &Arc<AtomicBool>,
) -> Option<ApprovalDecision> {
    let request_id = format!("acp-terminal-{terminal_id}");
    let sent = events.send(Ok(AgentEvent::ToolPermissionRequest {
        run_id: run_id.to_string(),
        request_id: request_id.clone(),
        tool_name: "Bash".to_string(),
        tool_input: serde_json::json!({ "command": command_line, "reason": reason }),
        tool_use_id: terminal_id.to_string(),
        hook_requires_approval: false,
        audit_ref: None,
    }));
    if sent.is_err() {
        return None;
    }
    await_decision(&request_id, approvals, cancelled)
}

/// Waits for the card answer that matches `request_id`.
fn await_decision(
    request_id: &str,
    approvals: &mpsc::Receiver<ApprovalResponse>,
    cancelled: &Arc<AtomicBool>,
) -> Option<ApprovalDecision> {
    let deadline = std::time::Instant::now() + APPROVAL_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if cancelled.load(Ordering::SeqCst) {
            return Some(ApprovalDecision::Deny {
                message: "turn cancelled before approval".to_string(),
            });
        }
        match approvals.recv_timeout(APPROVAL_POLL) {
            Ok(response) => {
                // Late answers to a previous card must not decide this one.
                if response.request_id == request_id {
                    return Some(response.decision);
                }
                tracing::debug!(
                    request_id = %response.request_id,
                    "ignoring approval for a different terminal"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
    None
}

/// Raises the shell's approval card for an agent permission request and
/// answers the bridge with the chosen option.
///
/// Approval stays a shell decision: the bridge only relays, so the same card
/// serves ACP permissions and dual-lane command execution.
pub(super) fn handle_permission_request(
    run_id: &str,
    request_id: &str,
    title: &str,
    options: &[(&str, &str)],
    writer: &BridgeWriter,
    events: &mpsc::Sender<Result<AgentEvent, AdapterError>>,
    approvals: &mpsc::Receiver<ApprovalResponse>,
    cancelled: &Arc<AtomicBool>,
) {
    let card_id = format!("acp-permission-{request_id}");
    let sent = events.send(Ok(AgentEvent::ToolPermissionRequest {
        run_id: run_id.to_string(),
        request_id: card_id.clone(),
        tool_name: "Agent".to_string(),
        tool_input: serde_json::json!({ "title": title }),
        tool_use_id: request_id.to_string(),
        hook_requires_approval: false,
        audit_ref: None,
    }));
    if sent.is_err() {
        permission_response(writer, request_id, None);
        return;
    }
    let option_id = match await_decision(&card_id, approvals, cancelled) {
        Some(ApprovalDecision::Allow) => pick_option(options, ALLOW_KINDS),
        Some(ApprovalDecision::Deny { .. }) => pick_option(options, REJECT_KINDS),
        // No answer and host-executed replay are both "the user did not pick
        // one of these options", which ACP models as a cancelled request.
        _ => None,
    };
    permission_response(writer, request_id, option_id);
}

/// Option kinds that mean the agent may proceed.
const ALLOW_KINDS: &[&str] = &["allow_once", "allow_always"];

/// Option kinds that mean the agent must not proceed.
const REJECT_KINDS: &[&str] = &["reject_once", "reject_always"];

/// Picks the first offered option whose kind is acceptable.
fn pick_option(options: &[(&str, &str)], kinds: &[&str]) -> Option<String> {
    options
        .iter()
        .find(|(_, kind)| kinds.contains(kind))
        .map(|(id, _)| (*id).to_string())
}

fn permission_response(writer: &BridgeWriter, request_id: &str, option_id: Option<String>) {
    write_message(
        writer,
        &serde_json::json!({
            "method": "permission_response",
            "request_id": request_id,
            "option_id": option_id,
            "cancelled": option_id.is_none(),
        }),
    );
}

/// Forwards background lane events to the bridge until the run ends.
pub(super) fn pump_lane_events(lane: &BackgroundLane, writer: &BridgeWriter) {
    for event in lane.drain_events() {
        match event {
            LaneEvent::Output { terminal_id, chunk } => {
                terminal_output(writer, &terminal_id, &chunk);
            }
            LaneEvent::Exit {
                terminal_id,
                exit_code,
                signal,
            } => terminal_exit(writer, &terminal_id, exit_code, signal.as_deref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_commands_take_the_background_lane() {
        assert_eq!(choose_lane("ls -la"), Lane::Background);
        assert_eq!(choose_lane("cat /etc/os-release"), Lane::Background);
    }

    #[test]
    fn mutating_commands_need_approval() {
        assert!(matches!(
            choose_lane("rm -rf /tmp/x"),
            Lane::Approval { .. }
        ));
        assert!(matches!(
            choose_lane("systemctl restart nginx"),
            Lane::Approval { .. } | Lane::Denied { .. }
        ));
    }

    #[test]
    fn shell_metacharacters_never_auto_run() {
        // The tokenizer gate must not be bypassed by metacharacters or by
        // Tab/newline separators (see AGENTS.md security heuristics).
        for command in [
            "ls; rm -rf /",
            "ls -la | sh",
            "ls $(rm -rf /tmp/x)",
            "ls\t-la;rm -rf /tmp/x",
            "ls\n rm -rf /tmp/x",
            "ls`rm -rf /tmp/x`",
        ] {
            assert!(
                !matches!(choose_lane(command), Lane::Background),
                "must not auto-run: {command}"
            );
        }
    }

    #[test]
    fn command_line_joins_args_for_assessment() {
        let create = TerminalCreate {
            terminal_id: "t1".to_string(),
            command: "ls".to_string(),
            args: vec!["-la".to_string(), "/tmp".to_string()],
            env: Vec::new(),
            cwd: None,
        };
        assert_eq!(create.command_line(), "ls -la /tmp");
    }
}
