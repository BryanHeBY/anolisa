//! Command execution delegated to the ACP client's terminals.
//!
//! When the client declares the terminal capability the agent must not run
//! commands itself: every command goes through `terminal/create` so it lands
//! on the client's audited execution path (ADR-011 tier 1). The client owns
//! the safety gate, the approval card, and the audit record; the agent only
//! reports what it wants to run and reads the outcome.

use std::path::Path;

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Client, ConnectionTo};
use async_trait::async_trait;

use crate::tool::shell::ShellDelegate;
use crate::tool::ToolResult;

/// Runs agent commands on the client through the ACP terminal methods.
pub(super) struct AcpTerminalDelegate {
    cx: ConnectionTo<Client>,
    session_id: String,
}

impl AcpTerminalDelegate {
    pub(super) fn new(cx: ConnectionTo<Client>, session_id: String) -> Self {
        Self { cx, session_id }
    }
}

#[async_trait]
impl ShellDelegate for AcpTerminalDelegate {
    async fn run(&self, command: &str, cwd: &Path, timeout_ms: u64) -> Result<ToolResult, String> {
        // `sh -c` keeps the agent's single-string command contract; the client
        // applies its own tokenizing safety gate to the same string.
        let create = acp::CreateTerminalRequest::new(self.session_id.clone(), "sh")
            .args(vec!["-c".to_string(), command.to_string()])
            .cwd(cwd.to_path_buf());
        let created = self
            .cx
            .send_request(create)
            .block_task()
            .await
            .map_err(|error| format!("terminal/create failed: {error}"))?;
        let terminal_id = created.terminal_id;

        let wait =
            acp::WaitForTerminalExitRequest::new(self.session_id.clone(), terminal_id.clone());
        let exit = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            self.cx.send_request(wait).block_task(),
        )
        .await;

        // Read output before releasing: the client may drop the buffer on
        // release, and a timed-out command still has partial output worth
        // reporting.
        let output = self
            .cx
            .send_request(acp::TerminalOutputRequest::new(
                self.session_id.clone(),
                terminal_id.clone(),
            ))
            .block_task()
            .await;

        let timed_out = exit.is_err();
        if timed_out {
            let _ = self
                .cx
                .send_request(acp::KillTerminalRequest::new(
                    self.session_id.clone(),
                    terminal_id.clone(),
                ))
                .block_task()
                .await;
        }
        let _ = self
            .cx
            .send_request(acp::ReleaseTerminalRequest::new(
                self.session_id.clone(),
                terminal_id,
            ))
            .block_task()
            .await;

        let text = match output {
            Ok(output) => output.output,
            Err(error) => return Err(format!("terminal/output failed: {error}")),
        };
        if timed_out {
            return Ok(ToolResult::error(format!(
                "Command timed out after {timeout_ms}ms\n{text}"
            )));
        }
        let exit_code = match exit {
            Ok(Ok(exit)) => exit.exit_status.exit_code,
            Ok(Err(error)) => {
                return Err(format!("terminal/wait_for_exit failed: {error}"));
            }
            // Unreachable: the timeout branch returned above.
            Err(_) => None,
        };
        Ok(terminal_result(text, exit_code))
    }
}

/// Builds the tool result from delegated terminal output.
fn terminal_result(output: String, exit_code: Option<u32>) -> ToolResult {
    let failed = exit_code.is_none_or(|code| code != 0);
    let output = if output.is_empty() {
        match exit_code {
            Some(code) => format!("(exit code: {code})"),
            None => "(terminated without an exit code)".to_string(),
        }
    } else {
        output
    };
    ToolResult {
        output,
        is_error: failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_exit_is_a_successful_result() {
        let result = terminal_result("done".to_string(), Some(0));
        assert!(!result.is_error);
        assert_eq!(result.output, "done");
    }

    #[test]
    fn nonzero_exit_is_reported_as_an_error() {
        let result = terminal_result("boom".to_string(), Some(2));
        assert!(result.is_error);
    }

    #[test]
    fn signal_death_without_an_exit_code_is_an_error() {
        let result = terminal_result(String::new(), None);
        assert!(result.is_error);
        assert!(result.output.contains("without an exit code"), "{result:?}");
    }

    #[test]
    fn empty_output_reports_the_exit_code() {
        let result = terminal_result(String::new(), Some(0));
        assert_eq!(result.output, "(exit code: 0)");
    }
}
