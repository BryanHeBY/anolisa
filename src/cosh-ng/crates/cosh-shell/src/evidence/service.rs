//! Shell-side Evidence Service: read-only forensics over a unix socket.
//!
//! Serves the `cosh-acp mcp-shell` proxy (ADR-012 data plane). Security
//! contract: socket directory 0700 and socket 0600, per-session one-shot
//! token, SO_PEERCRED same-uid check, and exit redaction applied here — the
//! proxy and the agent are both untrusted. Connections without a valid token
//! fail closed with `unauthorized` and no data.
//!
//! Wire protocol (internal contract, versioned independently from MCP):
//! one JSONL request per connection
//! `{"v":1,"token":"...","method":"...","params":{...}}` answered by one
//! JSONL response `{"ok":true,"data":...}` or
//! `{"ok":false,"error":{"code":"...","message":"..."}}`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use super::output_text::{clean_terminal_control_sequences, redact_sensitive_output};
use super::redaction::{provider_safe_command_facts, redact_provider_command_text};
use crate::types::CommandBlock;

/// Version tag every request must carry.
pub(crate) const EVIDENCE_WIRE_VERSION: u64 = 1;

/// Environment variable names used to hand the endpoint to `mcp-shell`.
pub(crate) const EVIDENCE_SOCKET_ENV: &str = "COSH_EVIDENCE_SOCKET";
pub(crate) const EVIDENCE_TOKEN_ENV: &str = "COSH_EVIDENCE_TOKEN";

const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_LIST_LIMIT: usize = 20;
const MAX_LIST_LIMIT: usize = 100;
const DEFAULT_OUTPUT_BYTES: usize = 12 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const CONTEXT_NEIGHBOR_COUNT: usize = 2;

/// Handler for `run_command` requests (the `cosh_terminal` MCP tool).
///
/// Installed per agent turn by the ACP adapter; it owns lane choice, the
/// safety gate, the approval card, and audit (ADR-012 execution exception).
/// Takes the request params and returns a complete wire response.
pub(crate) type RunCommandExecutor = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

/// Running evidence service; dropping it stops the listener and removes the
/// socket, which also revokes the session token (ADR-012).
pub(crate) struct EvidenceService {
    socket_path: PathBuf,
    token: String,
    blocks: Arc<Mutex<Vec<CommandBlock>>>,
    executor: Arc<Mutex<Option<(String, RunCommandExecutor)>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EvidenceService {
    /// Starts the service for a shell session under
    /// `$XDG_RUNTIME_DIR/cosh/<session>/evidence.sock` (temp dir fallback).
    ///
    /// # Errors
    ///
    /// Fails when the runtime directory or socket cannot be created with the
    /// required restrictive permissions.
    pub(crate) fn start(shell_session_id: &str) -> std::io::Result<Self> {
        let base = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self::start_in_dir(&base.join("cosh").join(shell_session_id))
    }

    /// Starts the service with the socket inside `dir` (created 0700).
    pub(crate) fn start_in_dir(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let socket_path = dir.join("evidence.sock");
        // A stale socket from a crashed session would make bind fail.
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let token = generate_token()?;
        let blocks = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("cosh-evidence".to_string())
            .spawn({
                let token = token.clone();
                let blocks = Arc::clone(&blocks);
                let executor = Arc::clone(&executor);
                let shutdown = Arc::clone(&shutdown);
                move || accept_loop(&listener, &token, blocks, executor, &shutdown)
            })?;

        Ok(Self {
            socket_path,
            token,
            blocks,
            executor,
            shutdown,
            thread: Some(thread),
        })
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// One-shot session token; inject through `COSH_EVIDENCE_TOKEN` only.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    /// Replaces the command-history snapshot served to forensics clients.
    pub(crate) fn update_blocks(&self, blocks: Vec<CommandBlock>) {
        if let Ok(mut guard) = self.blocks.lock() {
            *guard = blocks;
        }
    }

    /// Returns a handle to the internal blocks list so the `run_command`
    /// executor can append blocks for commands it executes (ADR-012).
    pub(crate) fn blocks_handle(&self) -> Arc<Mutex<Vec<CommandBlock>>> {
        Arc::clone(&self.blocks)
    }

    /// Installs the executor backing `run_command` for `run_id`.
    pub(crate) fn set_executor(&self, run_id: &str, executor: RunCommandExecutor) {
        if let Ok(mut guard) = self.executor.lock() {
            *guard = Some((run_id.to_string(), executor));
        }
    }

    /// Clears the executor while `run_id` still owns it, so `run_command` fails
    /// closed once that turn ends. A cancelled turn tears its bridge down after
    /// the shell already started the follow-up turn, so an unconditional clear
    /// would strip the live turn's executor.
    pub(crate) fn clear_executor(&self, run_id: &str) {
        if let Ok(mut guard) = self.executor.lock() {
            if guard.as_ref().is_some_and(|(owner, _)| owner == run_id) {
                *guard = None;
            }
        }
    }
}

impl Drop for EvidenceService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// 32 random bytes from the OS as lowercase hex.
fn generate_token() -> std::io::Result<String> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn accept_loop(
    listener: &UnixListener,
    token: &str,
    blocks: Arc<Mutex<Vec<CommandBlock>>>,
    executor: Arc<Mutex<Option<(String, RunCommandExecutor)>>>,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let token = token.to_string();
                let blocks = Arc::clone(&blocks);
                let executor = Arc::clone(&executor);
                std::thread::Builder::new()
                    .name("cosh-evidence-conn".to_string())
                    .spawn(move || handle_connection(stream, &token, &blocks, &executor))
                    .ok();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => {
                tracing::warn!("evidence service accept failed: {error}");
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        }
    }
}

fn handle_connection(
    stream: UnixStream,
    token: &str,
    blocks: &Mutex<Vec<CommandBlock>>,
    executor: &Mutex<Option<(String, RunCommandExecutor)>>,
) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    // Same-uid check runs before any byte is parsed (fail closed).
    if !peer_is_same_uid(&stream) {
        respond(
            &stream,
            &error_response("unauthorized", "peer not permitted"),
        );
        return;
    }

    let mut line = String::new();
    let mut reader = BufReader::new(&stream);
    match reader
        .by_ref()
        .take(MAX_REQUEST_LINE_BYTES as u64 + 1)
        .read_line(&mut line)
    {
        Ok(0) => return,
        Ok(read) if read > MAX_REQUEST_LINE_BYTES => {
            respond(&stream, &error_response("bad_request", "request too large"));
            return;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::debug!("evidence request read failed: {error}");
            return;
        }
    }

    let snapshot = blocks.lock().map(|guard| guard.clone()).unwrap_or_default();
    let executor_ref = executor
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|(_, executor)| Arc::clone(executor)));
    let response = handle_request_line(&line, token, &snapshot, executor_ref.as_ref());
    respond(&stream, &response);
}

fn peer_is_same_uid(stream: &UnixStream) -> bool {
    match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials) {
        Ok(credentials) => credentials.uid() == nix::unistd::getuid().as_raw(),
        Err(error) => {
            tracing::warn!("SO_PEERCRED lookup failed: {error}");
            false
        }
    }
}

fn respond(mut stream: &UnixStream, response: &Value) {
    if let Ok(line) = serde_json::to_string(response) {
        let _ = writeln!(stream, "{line}");
        let _ = stream.flush();
    }
}

pub(crate) fn error_response(code: &str, message: &str) -> Value {
    json!({ "ok": false, "error": { "code": code, "message": message } })
}

pub(crate) fn data_response(data: Value) -> Value {
    json!({ "ok": true, "data": data })
}

/// Runs `run_command` through the installed executor, or fails closed when
/// no turn is active.
pub(crate) fn run_command_with(params: &Value, executor: Option<&RunCommandExecutor>) -> Value {
    let Some(executor) = executor else {
        return error_response("run_command_unavailable", "no agent turn is active");
    };
    executor(params)
}

/// Parses and answers one request line. Pure so tests can cover the
/// authorization and redaction contract without a socket.
pub(crate) fn handle_request_line(
    line: &str,
    token: &str,
    blocks: &[CommandBlock],
    executor: Option<&RunCommandExecutor>,
) -> Value {
    let Ok(request) = serde_json::from_str::<Value>(line) else {
        return error_response("bad_request", "request is not valid JSON");
    };
    if request.get("v").and_then(Value::as_u64) != Some(EVIDENCE_WIRE_VERSION) {
        return error_response("bad_request", "unsupported wire version");
    }
    // Token check happens before the method is even looked at; the error
    // never distinguishes a missing from a wrong token.
    let presented = request.get("token").and_then(Value::as_str).unwrap_or("");
    if !constant_time_eq(presented.as_bytes(), token.as_bytes()) {
        return error_response("unauthorized", "invalid token");
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "list_shell_commands" => list_shell_commands(&params, blocks),
        "read_command_output" => read_command_output(&params, blocks),
        "get_command_context" => get_command_context(&params, blocks),
        "run_command" => run_command_with(&params, executor),
        _ => error_response("bad_request", "unknown method"),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn command_summary(block: &CommandBlock) -> Value {
    let facts = provider_safe_command_facts(block);
    json!({
        "id": facts.id,
        "command": facts.command,
        "cwd": facts.cwd,
        "end_cwd": facts.end_cwd,
        "status": facts.status,
        "exit_code": facts.exit_code,
        "started_at_ms": block.started_at_ms,
        "duration_ms": facts.duration_ms,
        "output_bytes": facts.output_bytes,
        "output_available": block.output.terminal_output_ref.is_some(),
    })
}

fn list_shell_commands(params: &Value, blocks: &[CommandBlock]) -> Value {
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let start = blocks.len().saturating_sub(limit);
    let commands: Vec<Value> = blocks[start..].iter().map(command_summary).collect();
    data_response(json!({ "commands": commands, "total": blocks.len() }))
}

fn read_command_output(params: &Value, blocks: &[CommandBlock]) -> Value {
    let Some(id) = params.get("id").and_then(Value::as_str) else {
        return error_response("bad_request", "missing command id");
    };
    let Some(block) = blocks.iter().find(|block| block.id == id) else {
        return error_response("not_found", "unknown command id");
    };
    let Some(output_ref) = block.output.terminal_output_ref.as_deref() else {
        return error_response("output_unavailable", "no captured output for this command");
    };
    let raw = match std::fs::read_to_string(output_ref) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!("evidence output read failed for {id}: {error}");
            return error_response("output_unavailable", "captured output cannot be read");
        }
    };
    let max_bytes = params
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_OUTPUT_BYTES)
        .clamp(1, MAX_OUTPUT_BYTES);
    // Exit redaction: control-sequence cleanup, secret masking, then a UTF-8
    // safe byte cap; the proxy never sees unredacted output.
    let cleaned = clean_terminal_control_sequences(&raw);
    let (redacted, was_redacted) = redact_sensitive_output(&cleaned);
    let (text, truncated) = truncate_utf8(&redacted, max_bytes);
    data_response(json!({
        "id": id,
        "text": text,
        "truncated": truncated,
        "redacted": was_redacted,
    }))
}

pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

fn get_command_context(params: &Value, blocks: &[CommandBlock]) -> Value {
    let Some(id) = params.get("id").and_then(Value::as_str) else {
        return error_response("bad_request", "missing command id");
    };
    let Some(position) = blocks.iter().position(|block| block.id == id) else {
        return error_response("not_found", "unknown command id");
    };
    let before_start = position.saturating_sub(CONTEXT_NEIGHBOR_COUNT);
    let before: Vec<Value> = blocks[before_start..position]
        .iter()
        .map(neighbor_summary)
        .collect();
    let after: Vec<Value> = blocks
        .iter()
        .skip(position + 1)
        .take(CONTEXT_NEIGHBOR_COUNT)
        .map(neighbor_summary)
        .collect();
    data_response(json!({
        "command": command_summary(&blocks[position]),
        "before": before,
        "after": after,
    }))
}

fn neighbor_summary(block: &CommandBlock) -> Value {
    json!({
        "id": block.id,
        "command": redact_provider_command_text(&block.command),
        "exit_code": block.exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CommandOrigin, CommandStatus, OutputRefs};
    use std::io::{BufRead, BufReader, Write};

    fn test_block(id: &str, command: &str, output_ref: Option<String>) -> CommandBlock {
        CommandBlock {
            id: id.to_string(),
            session_id: "shell-1".to_string(),
            command: command.to_string(),
            origin: CommandOrigin::UserInteractive,
            cwd: "/work".to_string(),
            end_cwd: "/work".to_string(),
            started_at_ms: 1_000,
            ended_at_ms: 2_000,
            duration_ms: 1_000,
            exit_code: 0,
            status: CommandStatus::Completed,
            output: OutputRefs {
                terminal_output_bytes: output_ref.is_some() as u64 * 64,
                terminal_output_ref: output_ref,
            },
            shell_environment_generation: None,
            audit_identity: None,
        }
    }

    fn request_line(token: &str, method: &str, params: Value) -> String {
        json!({ "v": EVIDENCE_WIRE_VERSION, "token": token, "method": method, "params": params })
            .to_string()
    }

    #[test]
    fn missing_or_wrong_token_fails_closed() {
        let blocks = vec![test_block("c1", "echo hi", None)];
        for line in [
            request_line("wrong-token", "list_shell_commands", json!({})),
            json!({ "v": EVIDENCE_WIRE_VERSION, "method": "list_shell_commands" }).to_string(),
        ] {
            let response = handle_request_line(&line, "good-token", &blocks, None);
            assert_eq!(response["ok"], false, "{response}");
            assert_eq!(response["error"]["code"], "unauthorized", "{response}");
            assert!(response.get("data").is_none(), "{response}");
        }
    }

    #[test]
    fn wrong_wire_version_is_rejected_before_token_check() {
        let line =
            json!({ "v": 99, "token": "good-token", "method": "list_shell_commands" }).to_string();
        let response = handle_request_line(&line, "good-token", &[], None);
        assert_eq!(response["error"]["code"], "bad_request", "{response}");
    }

    #[test]
    fn malformed_and_unknown_requests_are_bad_requests() {
        let response = handle_request_line("not json", "good-token", &[], None);
        assert_eq!(response["error"]["code"], "bad_request", "{response}");
        let line = request_line("good-token", "run_shell", json!({}));
        let response = handle_request_line(&line, "good-token", &[], None);
        assert_eq!(response["error"]["code"], "bad_request", "{response}");
    }

    #[test]
    fn run_command_fails_closed_without_an_executor() {
        let line = request_line("tok", "run_command", json!({ "command": "echo hi" }));
        let response = handle_request_line(&line, "tok", &[], None);
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(
            response["error"]["code"], "run_command_unavailable",
            "{response}"
        );
    }

    #[test]
    fn stale_turn_teardown_keeps_the_live_executor() {
        let dir = std::env::temp_dir().join(format!("cosh-evidence-exec-{}", std::process::id()));
        let service = EvidenceService::start_in_dir(&dir).expect("start service");
        let held = || {
            service
                .executor
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|(_, exec)| Arc::clone(exec)))
        };
        service.set_executor("run-2", Arc::new(|_| data_response(json!({ "ran": true }))));
        // run-1 was cancelled and only tears down after run-2 installed its own.
        service.clear_executor("run-1");
        let line = request_line(service.token(), "run_command", json!({}));
        let answer = handle_request_line(&line, service.token(), &[], held().as_ref());
        assert_eq!(answer["data"]["ran"], true, "{answer}");
        service.clear_executor("run-2");
        assert!(held().is_none(), "owning turn must retract its executor");
        drop(service);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn list_redacts_command_text() {
        let blocks = vec![test_block(
            "c1",
            "curl -H 'Authorization: Bearer super-secret-value' https://api.test",
            None,
        )];
        let line = request_line("tok", "list_shell_commands", json!({}));
        let response = handle_request_line(&line, "tok", &blocks, None);
        assert_eq!(response["ok"], true, "{response}");
        let command = response["data"]["commands"][0]["command"]
            .as_str()
            .expect("command");
        assert!(!command.contains("super-secret-value"), "{command}");
        assert!(command.contains("<redacted>"), "{command}");
        assert_eq!(response["data"]["total"], 1);
    }

    #[test]
    fn list_honors_limit_and_returns_most_recent() {
        let blocks: Vec<CommandBlock> = (0..5)
            .map(|index| test_block(&format!("c{index}"), &format!("echo {index}"), None))
            .collect();
        let line = request_line("tok", "list_shell_commands", json!({ "limit": 2 }));
        let response = handle_request_line(&line, "tok", &blocks, None);
        let commands = response["data"]["commands"].as_array().expect("commands");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0]["id"], "c3");
        assert_eq!(commands[1]["id"], "c4");
    }

    #[test]
    fn read_output_applies_exit_redaction_and_truncation() {
        let dir = std::env::temp_dir().join(format!("cosh-evidence-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let output_path = dir.join("c1.out");
        std::fs::write(&output_path, "token=super-secret-value\nplain line\n")
            .expect("write output");

        let blocks = vec![test_block(
            "c1",
            "env",
            Some(output_path.to_string_lossy().to_string()),
        )];
        let line = request_line("tok", "read_command_output", json!({ "id": "c1" }));
        let response = handle_request_line(&line, "tok", &blocks, None);
        let text = response["data"]["text"].as_str().expect("text");
        assert!(!text.contains("super-secret-value"), "{text}");
        assert!(text.contains("plain line"), "{text}");
        assert_eq!(response["data"]["redacted"], true);

        let line = request_line(
            "tok",
            "read_command_output",
            json!({ "id": "c1", "max_bytes": 4 }),
        );
        let response = handle_request_line(&line, "tok", &blocks, None);
        assert_eq!(response["data"]["truncated"], true);

        let _ = std::fs::remove_file(&output_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn read_output_without_capture_reports_unavailable() {
        let blocks = vec![test_block("c1", "echo hi", None)];
        let line = request_line("tok", "read_command_output", json!({ "id": "c1" }));
        let response = handle_request_line(&line, "tok", &blocks, None);
        assert_eq!(
            response["error"]["code"], "output_unavailable",
            "{response}"
        );
        let line = request_line("tok", "read_command_output", json!({ "id": "nope" }));
        let response = handle_request_line(&line, "tok", &blocks, None);
        assert_eq!(response["error"]["code"], "not_found", "{response}");
    }

    #[test]
    fn context_returns_neighbors() {
        let blocks: Vec<CommandBlock> = (0..5)
            .map(|index| test_block(&format!("c{index}"), &format!("echo {index}"), None))
            .collect();
        let line = request_line("tok", "get_command_context", json!({ "id": "c2" }));
        let response = handle_request_line(&line, "tok", &blocks, None);
        assert_eq!(response["data"]["command"]["id"], "c2");
        let before = response["data"]["before"].as_array().expect("before");
        let after = response["data"]["after"].as_array().expect("after");
        assert_eq!(before.len(), 2);
        assert_eq!(before[1]["id"], "c1");
        assert_eq!(after.len(), 2);
        assert_eq!(after[0]["id"], "c3");
    }

    #[test]
    fn socket_service_round_trip_and_permissions() {
        let dir = std::env::temp_dir().join(format!("cosh-evidence-sock-{}", std::process::id()));
        let service = EvidenceService::start_in_dir(&dir).expect("start service");
        service.update_blocks(vec![test_block("c1", "echo hi", None)]);

        let dir_mode = std::fs::metadata(&dir)
            .expect("dir meta")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "dir mode {dir_mode:o}");
        let socket_mode = std::fs::metadata(service.socket_path())
            .expect("socket meta")
            .permissions()
            .mode();
        assert_eq!(socket_mode & 0o777, 0o600, "socket mode {socket_mode:o}");

        // Valid token round trip.
        let mut stream = UnixStream::connect(service.socket_path()).expect("connect");
        writeln!(
            stream,
            "{}",
            request_line(service.token(), "list_shell_commands", json!({}))
        )
        .expect("write");
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("read");
        let response: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["data"]["commands"][0]["id"], "c1");

        // Wrong token over the real socket fails closed (S2 gate PoC).
        let mut stream = UnixStream::connect(service.socket_path()).expect("connect");
        writeln!(
            stream,
            "{}",
            request_line("forged-token", "list_shell_commands", json!({}))
        )
        .expect("write");
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("read");
        let response: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(response["error"]["code"], "unauthorized", "{response}");

        let socket_path = service.socket_path().to_path_buf();
        drop(service);
        assert!(!socket_path.exists(), "socket must be removed on shutdown");
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn oversized_request_line_is_rejected() {
        let dir = std::env::temp_dir().join(format!("cosh-evidence-big-{}", std::process::id()));
        let service = EvidenceService::start_in_dir(&dir).expect("start service");
        let mut stream = UnixStream::connect(service.socket_path()).expect("connect");
        let huge = "x".repeat(MAX_REQUEST_LINE_BYTES + 10);
        writeln!(stream, "{huge}").expect("write");
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("read");
        let response: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(response["error"]["code"], "bad_request", "{response}");
        drop(service);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn tokens_are_unique_and_long() {
        let first = generate_token().expect("token");
        let second = generate_token().expect("token");
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
    }
}
