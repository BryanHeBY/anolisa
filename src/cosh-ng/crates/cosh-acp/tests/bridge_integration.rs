//! Integration tests: spawn the real cosh-acp binary with a scripted fake
//! agent and drive the JSONL protocol over stdin/stdout.
//!
//! S1 gate coverage (ADR-011): handshake, version rejection, ACP initialize /
//! session/new / prompt streaming round-trip, not-implemented fallback,
//! shutdown, and agent process custody (no orphans after bridge exit).

mod common;

use common::{
    assert_process_gone, initialize_line, read_agent_pid, write_agent_script, BridgeHarness,
};
use std::path::PathBuf;

/// Writes a fake agent script that records its PID and then blocks without
/// ever answering ACP; used for custody tests around a hung agent.
fn write_hung_agent(label: &str) -> (PathBuf, PathBuf) {
    write_agent_script(
        label,
        "#!/bin/sh\necho $$ > \"$AGENT_PID_FILE\"\nexec sleep 30\n",
    )
}

/// Writes a scripted ACP agent: line-delimited JSON-RPC over stdio answering
/// initialize, session/new, and session/prompt (with one streamed update).
fn write_acp_agent(label: &str) -> (PathBuf, PathBuf) {
    let script = r#"#!/bin/sh
echo $$ > "$AGENT_PID_FILE"
while IFS= read -r line; do
  # JSON-RPC ids from the SDK are string UUIDs; echo the id back verbatim.
  id=$(printf '%s' "$line" | sed -n 's/.*[{,]"id":\("[^"]*"\|[0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"authMethods":[{"id":"oauth","name":"OAuth login"}]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-1"}}\n' "$id"
      ;;
    *'"method":"session/load"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello back"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
    *) : ;;
  esac
done
"#;
    write_agent_script(label, script)
}

#[test]
fn acp_session_round_trip_streams_and_completes() {
    let (script, pid_file) = write_acp_agent("roundtrip");
    let mut bridge = BridgeHarness::spawn();

    bridge.send(&initialize_line(
        1,
        &script.to_string_lossy(),
        &pid_file,
        false,
    ));
    let initialized = bridge.next_event();
    assert_eq!(initialized["event"], "initialized");
    assert_eq!(initialized["protocol_version"], 1);
    // Capabilities come from the real ACP initialize response now.
    assert_eq!(initialized["agent_capabilities"]["load_session"], true);
    assert_eq!(initialized["auth_methods"][0]["id"], "oauth");
    assert_eq!(initialized["auth_methods"][0]["label"], "OAuth login");

    let agent_pid = read_agent_pid(&pid_file);

    // A prompt before session_new must be rejected, not silently dropped.
    bridge.send(
        &serde_json::json!({
            "method": "prompt",
            "request_id": "r0",
            "session_id": "sess-1",
            "text": "too early",
            "approval_mode": "strict",
        })
        .to_string(),
    );
    let rejected = bridge.next_event();
    assert_eq!(rejected["event"], "agent_failed");
    assert_eq!(rejected["code"], "prompt_rejected");
    assert_eq!(rejected["recoverable"], true);

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
    assert_eq!(created["request_id"], "rq1");
    assert_eq!(created["session_id"], "sess-1");

    bridge.send(
        &serde_json::json!({
            "method": "prompt",
            "request_id": "r1",
            "session_id": "sess-1",
            "text": "hello",
            "approval_mode": "strict",
        })
        .to_string(),
    );
    let delta = bridge.next_event();
    assert_eq!(delta["event"], "text_delta");
    assert_eq!(delta["session_id"], "sess-1");
    assert_eq!(delta["text"], "hello back");
    let completed = bridge.next_event();
    assert_eq!(completed["event"], "prompt_completed");
    assert_eq!(completed["request_id"], "r1");
    assert_eq!(completed["stop_reason"], "end_turn");

    // Reloading the committed session is how the shell keeps context across
    // turns without holding a bridge process open.
    bridge.send(
        &serde_json::json!({
            "method": "session_load",
            "request_id": "rq2",
            "session_id": "sess-1",
        })
        .to_string(),
    );
    let loaded = bridge.next_event();
    assert_eq!(loaded["event"], "session_loaded");
    assert_eq!(loaded["request_id"], "rq2");
    assert_eq!(loaded["session_id"], "sess-1");

    bridge.send(&serde_json::json!({ "method": "shutdown" }).to_string());
    assert_eq!(bridge.wait_exit(), 0);
    assert_process_gone(agent_pid);
    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(pid_file);
}

#[test]
fn version_mismatch_fails_closed() {
    let (script, pid_file) = write_hung_agent("version");
    let mut bridge = BridgeHarness::spawn();

    bridge.send(&initialize_line(
        99,
        &script.to_string_lossy(),
        &pid_file,
        false,
    ));
    let error = bridge.next_event();
    assert_eq!(error["event"], "agent_failed");
    assert_eq!(error["code"], "protocol_error");
    assert_eq!(error["recoverable"], false);
    assert_ne!(bridge.wait_exit(), 0);
    // The agent must never have been spawned on a failed handshake.
    assert!(
        !pid_file.exists(),
        "agent spawned despite handshake failure"
    );
    let _ = std::fs::remove_file(script);
}

#[test]
fn stdin_eof_takes_the_agent_down() {
    let (script, pid_file) = write_acp_agent("eof");
    let mut bridge = BridgeHarness::spawn();

    bridge.send(&initialize_line(
        1,
        &script.to_string_lossy(),
        &pid_file,
        false,
    ));
    let initialized = bridge.next_event();
    assert_eq!(initialized["event"], "initialized");
    let agent_pid = read_agent_pid(&pid_file);

    // Simulates a cosh-shell crash: closing stdin must stop bridge and agent.
    assert_eq!(bridge.wait_exit(), 0);
    assert_process_gone(agent_pid);
    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(pid_file);
}

#[test]
fn stdin_eof_during_acp_initialize_takes_hung_agent_down() {
    let (script, pid_file) = write_hung_agent("hung");
    let mut bridge = BridgeHarness::spawn();

    bridge.send(&initialize_line(
        1,
        &script.to_string_lossy(),
        &pid_file,
        false,
    ));
    // The agent never answers ACP initialize; the shell going away must still
    // take bridge and agent down instead of hanging on the pending request.
    let agent_pid = read_agent_pid(&pid_file);
    assert_eq!(bridge.wait_exit(), 0);
    assert_process_gone(agent_pid);
    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(pid_file);
}

#[test]
fn missing_agent_binary_reports_recoverable_failure() {
    let pid_file = std::env::temp_dir().join("cosh-acp-test-missing.pid");
    let mut bridge = BridgeHarness::spawn();

    bridge.send(&initialize_line(
        1,
        "/nonexistent/cosh-acp-test-agent",
        &pid_file,
        false,
    ));
    let failed = bridge.next_event();
    assert_eq!(failed["event"], "agent_failed");
    assert_eq!(failed["code"], "agent_spawn_failed");
    assert_eq!(failed["recoverable"], true);
    assert_ne!(bridge.wait_exit(), 0);
}
