//! Integration tests for terminal delegation through the bridge
//! (ADR-011 dual-lane executor, bridge-side bookkeeping).
//!
//! A scripted ACP agent drives terminal/create → wait_for_exit → output while
//! the test plays the shell side of the JSONL protocol.

mod common;

use common::{initialize_line, write_agent_script, BridgeHarness};
use std::path::PathBuf;

/// Agent that runs one delegated terminal command per prompt turn and
/// reports `TERM code=<exit> out=<output>` back as a message chunk.
fn write_terminal_agent(label: &str) -> (PathBuf, PathBuf) {
    let script = r#"#!/bin/sh
echo $$ > "$AGENT_PID_FILE"
TERM_ID=""
PROMPT_ID=""
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*[{,]"id":\("[^"]*"\|[0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      PROMPT_ID=$id
      printf '{"jsonrpc":"2.0","id":"t-create","method":"terminal/create","params":{"sessionId":"sess-1","command":"echo","args":["hello"],"env":[]}}\n'
      ;;
    *'"method":"session/cancel"'*)
      if [ -n "$PROMPT_ID" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"cancelled"}}\n' "$PROMPT_ID"
        PROMPT_ID=""
      fi
      ;;
    *'"id":"t-create"'*)
      TERM_ID=$(printf '%s' "$line" | sed -n 's/.*"terminalId":"\([^"]*\)".*/\1/p')
      if [ -n "$TERM_ID" ]; then
        printf '{"jsonrpc":"2.0","id":"t-wait","method":"terminal/wait_for_exit","params":{"sessionId":"sess-1","terminalId":"%s"}}\n' "$TERM_ID"
      fi
      ;;
    *'"id":"t-wait"'*)
      CODE=$(printf '%s' "$line" | sed -n 's/.*"exitCode":\([0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":"t-out","method":"terminal/output","params":{"sessionId":"sess-1","terminalId":"%s"}}\n' "$TERM_ID"
      echo "$CODE" > "${AGENT_PID_FILE}.code"
      ;;
    *'"id":"t-out"'*)
      OUT=$(printf '%s' "$line" | sed -n 's/.*"output":"\([^"]*\)".*/\1/p')
      CODE=$(cat "${AGENT_PID_FILE}.code" 2>/dev/null)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"TERM code=%s out=%s"}}}}\n' "$CODE" "$OUT"
      if [ -n "$PROMPT_ID" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$PROMPT_ID"
        PROMPT_ID=""
      fi
      ;;
    *) : ;;
  esac
done
"#;
    write_agent_script(label, script)
}

fn open_session(bridge: &mut BridgeHarness, script: &std::path::Path, pid_file: &std::path::Path) {
    bridge.send(&initialize_line(
        1,
        &script.to_string_lossy(),
        pid_file,
        true,
    ));
    assert_eq!(bridge.next_event()["event"], "initialized");
    bridge.send(
        &serde_json::json!({
            "method": "session_new",
            "request_id": "rq1",
            "cwd": std::env::temp_dir().to_string_lossy(),
        })
        .to_string(),
    );
    let created = bridge.next_event();
    assert_eq!(created["event"], "session_created");
    assert_eq!(created["session_id"], "sess-1");
}

#[test]
fn terminal_create_wait_output_round_trip() {
    let (script, pid_file) = write_terminal_agent("term");
    let mut bridge = BridgeHarness::spawn();
    open_session(&mut bridge, &script, &pid_file);

    bridge.send(
        &serde_json::json!({
            "method": "prompt",
            "request_id": "r1",
            "session_id": "sess-1",
            "text": "run it",
            "approval_mode": "strict",
        })
        .to_string(),
    );

    // The agent's terminal/create surfaces as a bridge event for the shell.
    let create = bridge.next_event();
    assert_eq!(create["event"], "terminal_create", "{create}");
    assert_eq!(create["session_id"], "sess-1");
    assert_eq!(create["command"], "echo");
    assert_eq!(create["args"][0], "hello");
    let terminal_id = create["terminal_id"].as_str().expect("terminal id");

    // Shell side: confirm, stream output, then report the exit.
    bridge.send(
        &serde_json::json!({ "method": "terminal_created", "terminal_id": terminal_id })
            .to_string(),
    );
    bridge.send(
        &serde_json::json!({
            "method": "terminal_output",
            "terminal_id": terminal_id,
            "chunk": "hello",
            "truncated": false,
        })
        .to_string(),
    );
    bridge.send(
        &serde_json::json!({
            "method": "terminal_exit",
            "terminal_id": terminal_id,
            "exit_code": 0,
            "signal": null,
        })
        .to_string(),
    );

    // The agent saw exit code and buffered output through ACP terminal/*.
    let delta = bridge.next_event();
    assert_eq!(delta["event"], "text_delta", "{delta}");
    assert_eq!(delta["text"], "TERM code=0 out=hello");
    let completed = bridge.next_event();
    assert_eq!(completed["event"], "prompt_completed");
    assert_eq!(completed["stop_reason"], "end_turn");

    bridge.send(&serde_json::json!({ "method": "shutdown" }).to_string());
    assert_eq!(bridge.wait_exit(), 0);
    let _ = std::fs::remove_file(&script);
    let code_file = pid_file.with_extension("pid.code");
    let _ = std::fs::remove_file(code_file);
    let _ = std::fs::remove_file(pid_file);
}

#[test]
fn cancel_kills_active_terminals() {
    let (script, pid_file) = write_terminal_agent("cancel");
    let mut bridge = BridgeHarness::spawn();
    open_session(&mut bridge, &script, &pid_file);

    bridge.send(
        &serde_json::json!({
            "method": "prompt",
            "request_id": "r1",
            "session_id": "sess-1",
            "text": "run it",
            "approval_mode": "strict",
        })
        .to_string(),
    );
    let create = bridge.next_event();
    assert_eq!(create["event"], "terminal_create", "{create}");
    let terminal_id = create["terminal_id"].as_str().expect("terminal id");
    bridge.send(
        &serde_json::json!({ "method": "terminal_created", "terminal_id": terminal_id })
            .to_string(),
    );

    // Cancel while the terminal is still running: the bridge must ask the
    // shell to kill it (stage 2 of the three-stage cancellation).
    bridge.send(&serde_json::json!({ "method": "cancel", "session_id": "sess-1" }).to_string());
    let kill = bridge.next_event();
    assert_eq!(kill["event"], "terminal_kill", "{kill}");
    assert_eq!(kill["terminal_id"], terminal_id);

    // Shell reports the killed terminal; the agent then ends the turn.
    bridge.send(
        &serde_json::json!({
            "method": "terminal_exit",
            "terminal_id": terminal_id,
            "exit_code": null,
            "signal": "SIGTERM",
        })
        .to_string(),
    );
    let completed = bridge.next_event();
    assert_eq!(completed["event"], "prompt_completed", "{completed}");
    assert_eq!(completed["stop_reason"], "cancelled");

    bridge.send(&serde_json::json!({ "method": "shutdown" }).to_string());
    assert_eq!(bridge.wait_exit(), 0);
    let _ = std::fs::remove_file(&script);
    let _ = std::fs::remove_file(pid_file);
}
