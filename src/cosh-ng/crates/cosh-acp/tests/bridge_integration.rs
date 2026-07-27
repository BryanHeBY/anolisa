//! Integration tests: spawn the real cosh-acp binary with a scripted fake
//! agent and drive the JSONL protocol over stdin/stdout.
//!
//! S1 gate coverage (ADR-011): handshake, version rejection, ACP initialize /
//! session/new / prompt streaming round-trip, not-implemented fallback,
//! shutdown, and agent process custody (no orphans after bridge exit).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

fn bridge_binary() -> &'static str {
    env!("CARGO_BIN_EXE_cosh-acp")
}

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

fn write_agent_script(label: &str, content: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir();
    let pid_file = dir.join(format!("cosh-acp-test-{label}-{}.pid", std::process::id()));
    let script = dir.join(format!("cosh-acp-test-{label}-{}.sh", std::process::id()));
    std::fs::write(&script, content).expect("write fake agent");
    let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).expect("chmod fake agent");
    (script, pid_file)
}

fn initialize_line(version: u32, agent_command: &str, pid_file: &std::path::Path) -> String {
    serde_json::json!({
        "method": "initialize",
        "protocol_version": version,
        "agent": {
            "name": "fake",
            "command": agent_command,
            "args": [],
            "env": { "AGENT_PID_FILE": pid_file.to_string_lossy() },
        },
        "cwd": std::env::temp_dir().to_string_lossy(),
        "mcp_servers": [],
        "capabilities": { "terminal": false },
        "locale": null,
    })
    .to_string()
}

struct BridgeHarness {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl BridgeHarness {
    fn spawn() -> Self {
        let mut child = Command::new(bridge_binary())
            .arg("bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cosh-acp bridge");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("bridge stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("bridge stdin open");
        writeln!(stdin, "{line}").expect("write to bridge");
        stdin.flush().expect("flush bridge stdin");
    }

    fn next_event(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read bridge event");
        assert!(!line.is_empty(), "bridge stream ended unexpectedly");
        serde_json::from_str(&line).expect("bridge event is JSON")
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait_exit(mut self) -> i32 {
        self.close_stdin();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait bridge") {
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "bridge did not exit in time");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn read_agent_pid(pid_file: &std::path::Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(content) = std::fs::read_to_string(pid_file) {
            if let Ok(pid) = content.trim().parse() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "fake agent never reported a pid");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_process_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    loop {
        if !proc_path.exists() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "agent process {pid} is still alive after bridge exit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn acp_session_round_trip_streams_and_completes() {
    let (script, pid_file) = write_acp_agent("roundtrip");
    let mut bridge = BridgeHarness::spawn();

    bridge.send(&initialize_line(1, &script.to_string_lossy(), &pid_file));
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

    // Messages beyond the S1 wiring stay visible as recoverable failures.
    bridge.send(
        &serde_json::json!({
            "method": "session_load",
            "request_id": "rq2",
            "session_id": "sess-1",
        })
        .to_string(),
    );
    let failed = bridge.next_event();
    assert_eq!(failed["event"], "agent_failed");
    assert_eq!(failed["code"], "not_implemented");
    assert_eq!(failed["recoverable"], true);

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

    bridge.send(&initialize_line(99, &script.to_string_lossy(), &pid_file));
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

    bridge.send(&initialize_line(1, &script.to_string_lossy(), &pid_file));
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

    bridge.send(&initialize_line(1, &script.to_string_lossy(), &pid_file));
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
    ));
    let failed = bridge.next_event();
    assert_eq!(failed["event"], "agent_failed");
    assert_eq!(failed["code"], "agent_spawn_failed");
    assert_eq!(failed["recoverable"], true);
    assert_ne!(bridge.wait_exit(), 0);
}
