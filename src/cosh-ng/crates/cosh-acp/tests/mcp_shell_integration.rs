//! Integration tests: spawn `cosh-acp mcp-shell` against an in-test fake
//! Evidence Service socket and drive MCP JSON-RPC over stdio.
//!
//! S2 gate coverage (ADR-012): missing endpoint config fails closed at
//! startup, wrong token is rejected end to end, socket loss ends the proxy,
//! and the frozen three-tool contract round-trips.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_cosh-acp")
}

const GOOD_TOKEN: &str = "test-token-good";

/// Starts a fake evidence service that answers one request per connection,
/// enforcing the token exactly like the shell side does.
fn start_fake_evidence(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cosh-mcp-test-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create socket dir");
    let socket_path = dir.join("evidence.sock");
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind fake evidence socket");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }
            let request: serde_json::Value = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(_) => continue,
            };
            let response = if request["token"] != GOOD_TOKEN {
                serde_json::json!({
                    "ok": false,
                    "error": { "code": "unauthorized", "message": "invalid token" },
                })
            } else {
                match request["method"].as_str().unwrap_or("") {
                    "list_shell_commands" => serde_json::json!({
                        "ok": true,
                        "data": { "commands": [{ "id": "c1", "command": "echo hi" }], "total": 1 },
                    }),
                    "read_command_output" => serde_json::json!({
                        "ok": true,
                        "data": { "id": request["params"]["id"], "text": "hi", "truncated": false },
                    }),
                    "get_command_context" => serde_json::json!({
                        "ok": true,
                        "data": { "command": { "id": request["params"]["id"] }, "before": [], "after": [] },
                    }),
                    _ => serde_json::json!({
                        "ok": false,
                        "error": { "code": "bad_request", "message": "unknown method" },
                    }),
                }
            };
            let mut stream = stream;
            let _ = writeln!(stream, "{response}");
        }
    });
    socket_path
}

struct McpHarness {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl McpHarness {
    fn spawn(socket: Option<&std::path::Path>, token: Option<&str>) -> Self {
        let mut command = Command::new(binary());
        command
            .arg("mcp-shell")
            .env_remove("COSH_EVIDENCE_SOCKET")
            .env_remove("COSH_EVIDENCE_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(socket) = socket {
            command.env("COSH_EVIDENCE_SOCKET", socket);
        }
        if let Some(token) = token {
            command.env("COSH_EVIDENCE_TOKEN", token);
        }
        let mut child = command.spawn().expect("spawn mcp-shell");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, message: &serde_json::Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write");
        stdin.flush().expect("flush");
    }

    fn next_response(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        assert!(!line.is_empty(), "mcp-shell stream ended unexpectedly");
        serde_json::from_str(&line).expect("response is JSON")
    }

    fn wait_exit(mut self) -> i32 {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "mcp-shell did not exit in time");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn call_tool(id: u64, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

#[test]
fn mcp_round_trip_with_valid_token() {
    let socket = start_fake_evidence("ok");
    let mut mcp = McpHarness::spawn(Some(&socket), Some(GOOD_TOKEN));

    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} },
    }));
    let init = mcp.next_response();
    assert_eq!(init["result"]["serverInfo"]["name"], "cosh-shell");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    }));

    mcp.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/list",
    }));
    let tools = mcp.next_response();
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        [
            "list_shell_commands",
            "read_command_output",
            "get_command_context",
            "cosh_terminal"
        ]
    );

    mcp.send(&call_tool(3, "list_shell_commands", serde_json::json!({})));
    let listed = mcp.next_response();
    assert_eq!(listed["result"]["isError"], false, "{listed}");
    let text = listed["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(text.contains("echo hi"), "{text}");

    mcp.send(&call_tool(
        4,
        "read_command_output",
        serde_json::json!({ "id": "c1" }),
    ));
    let output = mcp.next_response();
    assert_eq!(output["result"]["isError"], false, "{output}");

    assert_eq!(mcp.wait_exit(), 0);
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn missing_endpoint_configuration_fails_closed_at_startup() {
    let socket = start_fake_evidence("noenv");
    // No token at all.
    let mcp = McpHarness::spawn(Some(&socket), None);
    assert_ne!(mcp.wait_exit(), 0, "missing token must refuse to start");
    // No socket either.
    let mcp = McpHarness::spawn(None, Some(GOOD_TOKEN));
    assert_ne!(mcp.wait_exit(), 0, "missing socket must refuse to start");
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn wrong_token_is_rejected_end_to_end() {
    let socket = start_fake_evidence("badtok");
    let mut mcp = McpHarness::spawn(Some(&socket), Some("forged-token"));

    mcp.send(&call_tool(1, "list_shell_commands", serde_json::json!({})));
    let response = mcp.next_response();
    assert_eq!(response["result"]["isError"], true, "{response}");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(text.contains("unauthorized"), "{text}");

    assert_eq!(mcp.wait_exit(), 0);
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn socket_loss_ends_the_proxy() {
    let socket =
        std::env::temp_dir().join(format!("cosh-mcp-test-gone-{}.sock", std::process::id()));
    let mut mcp = McpHarness::spawn(Some(&socket), Some(GOOD_TOKEN));

    mcp.send(&call_tool(1, "list_shell_commands", serde_json::json!({})));
    let response = mcp.next_response();
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_ne!(mcp.wait_exit(), 0, "proxy must exit when the shell is gone");
}
