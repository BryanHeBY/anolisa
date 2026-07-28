//! Shared harness for cosh-acp integration tests: spawns the real bridge
//! binary and drives the JSONL protocol over stdin/stdout.
//!
//! Each test target uses a subset of these helpers, so unused ones are
//! expected per target.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

pub fn bridge_binary() -> &'static str {
    env!("CARGO_BIN_EXE_cosh-acp")
}

/// Writes an executable fake-agent script and returns (script, pid_file).
pub fn write_agent_script(label: &str, content: &str) -> (PathBuf, PathBuf) {
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

pub fn initialize_line(
    version: u32,
    agent_command: &str,
    pid_file: &std::path::Path,
    terminal: bool,
) -> String {
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
        "capabilities": { "terminal": terminal },
        "locale": null,
    })
    .to_string()
}

pub struct BridgeHarness {
    pub child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl BridgeHarness {
    pub fn spawn() -> Self {
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

    pub fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("bridge stdin open");
        writeln!(stdin, "{line}").expect("write to bridge");
        stdin.flush().expect("flush bridge stdin");
    }

    pub fn next_event(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read bridge event");
        assert!(!line.is_empty(), "bridge stream ended unexpectedly");
        serde_json::from_str(&line).expect("bridge event is JSON")
    }

    /// Drains every event the bridge emits until its stream ends.
    ///
    /// Used to assert that something was *not* emitted, which a single
    /// `next_event` cannot show once the expected event arrives first.
    pub fn drain_events(mut self) -> Vec<serde_json::Value> {
        self.stdin.take();
        let mut events = Vec::new();
        loop {
            let mut line = String::new();
            match self.stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(event) = serde_json::from_str(line.trim()) {
                        events.push(event);
                    }
                }
            }
        }
        events
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    pub fn wait_exit(mut self) -> i32 {
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

pub fn read_agent_pid(pid_file: &std::path::Path) -> i32 {
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

pub fn assert_process_gone(pid: i32) {
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
