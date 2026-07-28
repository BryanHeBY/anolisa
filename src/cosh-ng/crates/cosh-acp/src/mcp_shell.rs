//! `mcp-shell` form: stateless stdio MCP server proxying the shell-side
//! Evidence Service (ADR-012 data plane).
//!
//! Speaks JSON-RPC 2.0 MCP on stdin/stdout and the evidence socket protocol
//! on a per-call unix connection. Holds no state beyond the endpoint handed
//! over in `COSH_EVIDENCE_SOCKET` / `COSH_EVIDENCE_TOKEN`; redaction happens
//! shell-side, so this proxy never sees unredacted data. Exits on stdin EOF
//! or when the shell socket becomes unreachable (lifetime bound to both
//! parents). Missing endpoint configuration fails closed at startup.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{json, Value};

/// MCP server name as declared to agents (ADR-012 frozen contract).
const MCP_SERVER_NAME: &str = "cosh-shell";
/// MCP protocol revision answered when the client does not send one.
const DEFAULT_MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Evidence socket wire version; must match the shell-side service.
const EVIDENCE_WIRE_VERSION: u64 = 1;

const SOCKET_ENV: &str = "COSH_EVIDENCE_SOCKET";
const TOKEN_ENV: &str = "COSH_EVIDENCE_TOKEN";
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs the proxy until stdin EOF or the shell socket goes away.
pub fn run() -> i32 {
    let Ok(socket_path) = std::env::var(SOCKET_ENV) else {
        tracing::error!("{SOCKET_ENV} is not set; refusing to start");
        return 2;
    };
    let Ok(token) = std::env::var(TOKEN_ENV) else {
        tracing::error!("{TOKEN_ENV} is not set; refusing to start");
        return 2;
    };
    if socket_path.is_empty() || token.is_empty() {
        tracing::error!("evidence endpoint configuration is empty; refusing to start");
        return 2;
    }

    let stdin = std::io::stdin();
    for line in BufReader::new(stdin.lock()).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!("stdin read failed: {error}");
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!("ignoring malformed MCP message: {error}");
                continue;
            }
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

        match method {
            "initialize" => {
                let protocol_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_MCP_PROTOCOL_VERSION);
                respond_result(
                    id,
                    json!({
                        "protocolVersion": protocol_version,
                        "capabilities": { "tools": {} },
                        "serverInfo": {
                            "name": MCP_SERVER_NAME,
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    }),
                );
            }
            "notifications/initialized" | "notifications/cancelled" => {}
            "ping" => respond_result(id, json!({})),
            "tools/list" => respond_result(id, json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let (response, shell_gone) = call_tool(&socket_path, &token, &params);
                respond_result(id, response);
                if shell_gone {
                    tracing::info!("evidence socket unreachable; exiting with the shell");
                    return 1;
                }
            }
            _ if id.is_some() => respond_error(id, -32601, "method not found"),
            other => tracing::debug!("ignoring notification {other}"),
        }
    }
    0
}

/// Tool schemas are the external contract (ADR-012): additive changes only.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_shell_commands",
            "description": "List recent commands from the user's shell session with redacted metadata (command text, exit code, cwd, timing).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "description": "Maximum number of most recent commands to return (default 20).",
                    },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "read_command_output",
            "description": "Read the redacted captured terminal output of one shell command.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Command id from list_shell_commands." },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 65536,
                        "description": "Byte cap for the returned text (default 12288).",
                    },
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "get_command_context",
            "description": "Get redacted details for one shell command plus its neighboring commands.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Command id from list_shell_commands." },
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
    ])
}

/// Proxies one `tools/call` to the evidence socket.
///
/// Returns the MCP tool result plus a flag that is true when the shell side
/// is gone and the proxy must exit.
fn call_tool(socket_path: &str, token: &str, params: &Value) -> (Value, bool) {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        name,
        "list_shell_commands" | "read_command_output" | "get_command_context"
    ) {
        return (tool_error(format!("unknown tool: {name}")), false);
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let request = json!({
        "v": EVIDENCE_WIRE_VERSION,
        "token": token,
        "method": name,
        "params": arguments,
    });

    let response = match evidence_round_trip(socket_path, &request) {
        Ok(response) => response,
        Err(error) => {
            return (
                tool_error(format!("evidence service unreachable: {error}")),
                true,
            );
        }
    };
    if response.get("ok") == Some(&Value::Bool(true)) {
        let data = response.get("data").cloned().unwrap_or_else(|| json!({}));
        let text = serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string());
        (
            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
            false,
        )
    } else {
        let code = response["error"]["code"].as_str().unwrap_or("error");
        let message = response["error"]["message"].as_str().unwrap_or("");
        (tool_error(format!("{code}: {message}")), false)
    }
}

fn evidence_round_trip(socket_path: &str, request: &Value) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT))?;
    writeln!(stream, "{request}")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "evidence service closed the connection without a response",
        ));
    }
    serde_json::from_str(&line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

fn tool_error(message: String) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn respond_result(id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    emit_line(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn respond_error(id: Option<Value>, code: i64, message: &str) {
    let Some(id) = id else { return };
    emit_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }));
}

fn emit_line(message: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_expose_the_frozen_contract() {
        let tools = tool_definitions();
        let names: Vec<&str> = tools
            .as_array()
            .expect("array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            [
                "list_shell_commands",
                "read_command_output",
                "get_command_context"
            ]
        );
        for tool in tools.as_array().expect("array") {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
    }

    #[test]
    fn unknown_tool_is_an_error_without_touching_the_socket() {
        let (response, shell_gone) = call_tool(
            "/nonexistent/evidence.sock",
            "tok",
            &json!({ "name": "run_command", "arguments": {} }),
        );
        assert_eq!(response["isError"], true, "{response}");
        assert!(!shell_gone);
    }

    #[test]
    fn unreachable_socket_reports_error_and_requests_exit() {
        let (response, shell_gone) = call_tool(
            "/nonexistent/evidence.sock",
            "tok",
            &json!({ "name": "list_shell_commands", "arguments": {} }),
        );
        assert_eq!(response["isError"], true, "{response}");
        assert!(shell_gone, "socket loss must end the proxy");
    }
}
