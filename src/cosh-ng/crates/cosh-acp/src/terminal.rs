//! Terminal delegation registry: routes agent `terminal/*` requests through
//! the shell (ADR-011 dual-lane executor, background lane bookkeeping).
//!
//! The bridge owns terminal identity and output buffering; execution and
//! approval live shell-side. ACP requests park their responders here until
//! the shell confirms (`terminal_created` / `terminal_denied`) or reports
//! progress (`terminal_output` / `terminal_exit`). Output is capped at the
//! agent-provided `output_byte_limit` (bridge default applies otherwise) by
//! truncating from the beginning at character boundaries, per ACP spec.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::Responder;

use crate::bridge::emit;
use crate::protocol::BridgeMessage;

/// Retained output cap when the agent does not specify one.
const DEFAULT_OUTPUT_BYTE_LIMIT: usize = 128 * 1024;

/// One delegated terminal tracked by the bridge.
#[derive(Default)]
struct TerminalState {
    session_id: String,
    output: String,
    truncated: bool,
    byte_limit: usize,
    exit: Option<acp::TerminalExitStatus>,
    confirmed: bool,
    pending_create: Option<Responder<acp::CreateTerminalResponse>>,
    pending_waits: Vec<Responder<acp::WaitForTerminalExitResponse>>,
}

/// Shared registry; handlers and the session loop both touch it.
#[derive(Default)]
pub(crate) struct TerminalRegistry {
    inner: Mutex<HashMap<String, TerminalState>>,
    next_id: AtomicU64,
}

impl TerminalRegistry {
    /// Handles `terminal/create`: allocates the id, parks the responder, and
    /// forwards the request to the shell for assessment and execution.
    pub(crate) fn create(
        &self,
        request: acp::CreateTerminalRequest,
        responder: Responder<acp::CreateTerminalResponse>,
    ) {
        let terminal_id = format!("term-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let byte_limit = request
            .output_byte_limit
            .map(|limit| limit as usize)
            .unwrap_or(DEFAULT_OUTPUT_BYTE_LIMIT)
            .max(1);
        let session_id = request.session_id.to_string();
        {
            let mut inner = self.inner.lock().expect("terminal registry poisoned");
            inner.insert(
                terminal_id.clone(),
                TerminalState {
                    session_id: session_id.clone(),
                    byte_limit,
                    pending_create: Some(responder),
                    ..TerminalState::default()
                },
            );
        }
        emit(&BridgeMessage::TerminalCreate {
            session_id,
            terminal_id,
            command: request.command,
            args: request.args,
            env: request
                .env
                .into_iter()
                .map(|variable| (variable.name, variable.value))
                .collect(),
            cwd: request.cwd.map(|cwd| cwd.to_string_lossy().to_string()),
            output_byte_limit: Some(byte_limit as u64),
        });
    }

    /// Handles `terminal/output`: answers immediately from the buffer.
    pub(crate) fn output(
        &self,
        request: &acp::TerminalOutputRequest,
        responder: Responder<acp::TerminalOutputResponse>,
    ) {
        let inner = self.inner.lock().expect("terminal registry poisoned");
        let Some(state) = inner.get(request.terminal_id.0.as_ref()) else {
            drop(inner);
            let _ = responder.respond_with_internal_error("unknown terminal id");
            return;
        };
        let response = acp::TerminalOutputResponse::new(state.output.clone(), state.truncated)
            .exit_status(state.exit.clone());
        drop(inner);
        let _ = responder.respond(response);
    }

    /// Handles `terminal/wait_for_exit`: parks the responder until the shell
    /// reports the exit, or answers immediately when it already has.
    pub(crate) fn wait_for_exit(
        &self,
        request: &acp::WaitForTerminalExitRequest,
        responder: Responder<acp::WaitForTerminalExitResponse>,
    ) {
        let mut inner = self.inner.lock().expect("terminal registry poisoned");
        let Some(state) = inner.get_mut(request.terminal_id.0.as_ref()) else {
            drop(inner);
            let _ = responder.respond_with_internal_error("unknown terminal id");
            return;
        };
        if let Some(exit) = state.exit.clone() {
            drop(inner);
            let _ = responder.respond(acp::WaitForTerminalExitResponse::new(exit));
        } else {
            state.pending_waits.push(responder);
        }
    }

    /// Handles `terminal/kill`: forwards to the shell; the exit itself still
    /// arrives through `terminal_exit` (three-stage cancellation, ADR-011).
    pub(crate) fn kill(
        &self,
        request: &acp::KillTerminalRequest,
        responder: Responder<acp::KillTerminalResponse>,
    ) {
        let known = self
            .inner
            .lock()
            .expect("terminal registry poisoned")
            .contains_key(request.terminal_id.0.as_ref());
        if !known {
            let _ = responder.respond_with_internal_error("unknown terminal id");
            return;
        }
        emit(&BridgeMessage::TerminalKill {
            terminal_id: request.terminal_id.to_string(),
        });
        let _ = responder.respond(acp::KillTerminalResponse::new());
    }

    /// Handles `terminal/release`: drops all bridge state and tells the shell
    /// to reap whatever is still running.
    pub(crate) fn release(
        &self,
        request: &acp::ReleaseTerminalRequest,
        responder: Responder<acp::ReleaseTerminalResponse>,
    ) {
        let removed = self
            .inner
            .lock()
            .expect("terminal registry poisoned")
            .remove(request.terminal_id.0.as_ref());
        match removed {
            Some(state) => {
                fail_parked_responders(state, "terminal released");
                emit(&BridgeMessage::TerminalRelease {
                    terminal_id: request.terminal_id.to_string(),
                });
                let _ = responder.respond(acp::ReleaseTerminalResponse::new());
            }
            None => {
                let _ = responder.respond_with_internal_error("unknown terminal id");
            }
        }
    }

    /// Shell confirmed the terminal: answer the parked `terminal/create`.
    pub(crate) fn confirm_created(&self, terminal_id: &str) {
        let responder = {
            let mut inner = self.inner.lock().expect("terminal registry poisoned");
            let Some(state) = inner.get_mut(terminal_id) else {
                tracing::warn!("terminal_created for unknown terminal {terminal_id}");
                return;
            };
            state.confirmed = true;
            state.pending_create.take()
        };
        if let Some(responder) = responder {
            let _ = responder.respond(acp::CreateTerminalResponse::new(terminal_id.to_string()));
        }
    }

    /// Shell denied the terminal (safety gate or user): fail the create and
    /// drop the state so later lookups answer `unknown terminal id`.
    pub(crate) fn deny(&self, terminal_id: &str, reason: &str) {
        let removed = self
            .inner
            .lock()
            .expect("terminal registry poisoned")
            .remove(terminal_id);
        match removed {
            Some(state) => fail_parked_responders(state, reason),
            None => tracing::warn!("terminal_denied for unknown terminal {terminal_id}"),
        }
    }

    /// Shell streamed an output chunk: append under the byte cap, truncating
    /// from the beginning at a character boundary (ACP contract).
    pub(crate) fn append_output(&self, terminal_id: &str, chunk: &str, truncated: bool) {
        let mut inner = self.inner.lock().expect("terminal registry poisoned");
        let Some(state) = inner.get_mut(terminal_id) else {
            tracing::warn!("terminal_output for unknown terminal {terminal_id}");
            return;
        };
        state.output.push_str(chunk);
        state.truncated |= truncated;
        if state.output.len() > state.byte_limit {
            let overflow = state.output.len() - state.byte_limit;
            let mut cut = overflow;
            while cut < state.output.len() && !state.output.is_char_boundary(cut) {
                cut += 1;
            }
            state.output.drain(..cut);
            state.truncated = true;
        }
    }

    /// Shell reported the exit: record it and flush parked waiters.
    pub(crate) fn record_exit(
        &self,
        terminal_id: &str,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) {
        let (exit, waiters) = {
            let mut inner = self.inner.lock().expect("terminal registry poisoned");
            let Some(state) = inner.get_mut(terminal_id) else {
                tracing::warn!("terminal_exit for unknown terminal {terminal_id}");
                return;
            };
            let exit = acp::TerminalExitStatus::new()
                .exit_code(exit_code.and_then(|code| u32::try_from(code).ok()))
                .signal(signal.map(str::to_owned));
            state.exit = Some(exit.clone());
            (exit, std::mem::take(&mut state.pending_waits))
        };
        for responder in waiters {
            let _ = responder.respond(acp::WaitForTerminalExitResponse::new(exit.clone()));
        }
    }

    /// Session ids of terminals that are still running (no exit recorded).
    /// Used by `session/cancel` to kill the whole active set shell-side.
    pub(crate) fn active_terminal_ids(&self, session_id: &str) -> Vec<String> {
        let inner = self.inner.lock().expect("terminal registry poisoned");
        inner
            .iter()
            .filter(|(_, state)| state.session_id == session_id && state.exit.is_none())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

fn fail_parked_responders(state: TerminalState, reason: &str) {
    if let Some(responder) = state.pending_create {
        let _ = responder.respond_with_internal_error(reason);
    }
    for responder in state.pending_waits {
        let _ = responder.respond_with_internal_error(reason);
    }
}
