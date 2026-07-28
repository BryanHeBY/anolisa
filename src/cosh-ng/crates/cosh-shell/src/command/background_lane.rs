//! Background lane of the dual-lane command executor (ADR-011).
//!
//! Runs agent-delegated commands out of sight of the user's PTY: many can be
//! in flight at once, output streams back under a byte cap, and the full
//! terminal lifecycle (create/output/kill/exit) is observable. Commands that
//! need a real TTY are not this lane's business — `assess_shell_command`
//! routes those to the foreground handoff lane instead.
//!
//! Each command leads its own process group so a kill reaps grandchildren,
//! and the environment is allowlisted: `COSH_*` internals and unrelated user
//! secrets never reach an agent-requested command (ADR-012).

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Environment variables passed through to delegated commands. Everything
/// else — notably `COSH_*` — is dropped (ADR-012 environment allowlist).
const ENV_ALLOWLIST: &[&str] = &[
    "HOME", "LANG", "LC_ALL", "LC_CTYPE", "PATH", "PWD", "SHELL", "TERM", "TMPDIR", "TZ", "USER",
];

/// How long a kill request waits for the group to die before SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(500);

/// One event from a background command, consumed by the ACP adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaneEvent {
    /// A chunk of interleaved stdout/stderr text.
    Output { terminal_id: String, chunk: String },
    /// The command finished; `signal` is set when it died from one.
    Exit {
        terminal_id: String,
        exit_code: Option<i32>,
        signal: Option<String>,
    },
}

/// Spawn request for one delegated command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaneRequest {
    pub(crate) terminal_id: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) cwd: Option<String>,
}

/// Live background commands keyed by terminal id.
pub(crate) struct BackgroundLane {
    running: Arc<Mutex<HashMap<String, Child>>>,
    events_tx: Sender<LaneEvent>,
    events_rx: Receiver<LaneEvent>,
}

impl Default for BackgroundLane {
    fn default() -> Self {
        let (events_tx, events_rx) = channel();
        Self {
            running: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            events_rx,
        }
    }
}

impl BackgroundLane {
    /// Spawns one command in the background lane.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when the process cannot be spawned;
    /// callers report it as a terminal denial so no phantom terminal exists.
    pub(crate) fn spawn(&self, request: &LaneRequest) -> Result<(), String> {
        let mut command = Command::new(&request.command);
        command
            .args(&request.args)
            .env_clear()
            .envs(inherited_env())
            .envs(request.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so killing reaps the whole tree.
            .process_group(0);
        if let Some(cwd) = request.cwd.as_deref() {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start '{}': {error}", request.command))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        self.running
            .lock()
            .expect("background lane poisoned")
            .insert(request.terminal_id.clone(), child);

        for stream in [stdout.map(Readable::Out), stderr.map(Readable::Err)] {
            let Some(stream) = stream else { continue };
            spawn_reader(stream, request.terminal_id.clone(), self.events_tx.clone());
        }
        spawn_reaper(
            Arc::clone(&self.running),
            request.terminal_id.clone(),
            self.events_tx.clone(),
        );
        Ok(())
    }

    /// Kills the process group of one terminal; the exit still arrives as a
    /// `LaneEvent::Exit` from the reaper.
    pub(crate) fn kill(&self, terminal_id: &str) {
        let pid = self
            .running
            .lock()
            .expect("background lane poisoned")
            .get(terminal_id)
            .map(|child| child.id());
        let Some(pid) = pid else { return };
        signal_group(pid, nix::sys::signal::Signal::SIGTERM);
        // Escalate in the background so the caller never blocks on a
        // command that ignores SIGTERM.
        std::thread::spawn(move || {
            std::thread::sleep(KILL_GRACE);
            signal_group(pid, nix::sys::signal::Signal::SIGKILL);
        });
    }

    /// Kills every live terminal; used by cancellation and session teardown.
    pub(crate) fn kill_all(&self) {
        let ids: Vec<String> = self
            .running
            .lock()
            .expect("background lane poisoned")
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.kill(&id);
        }
    }

    /// Drains the events produced since the last call.
    pub(crate) fn drain_events(&self) -> Vec<LaneEvent> {
        self.events_rx.try_iter().collect()
    }

    /// True while at least one delegated command is running.
    pub(crate) fn is_busy(&self) -> bool {
        !self
            .running
            .lock()
            .expect("background lane poisoned")
            .is_empty()
    }
}

impl Drop for BackgroundLane {
    fn drop(&mut self) {
        self.kill_all();
    }
}

enum Readable {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Read for Readable {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Out(stream) => stream.read(buffer),
            Self::Err(stream) => stream.read(buffer),
        }
    }
}

/// Streams one pipe as UTF-8-lossy chunks until EOF.
fn spawn_reader(mut stream: Readable, terminal_id: String, events: Sender<LaneEvent>) {
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let chunk = String::from_utf8_lossy(&buffer[..read]).to_string();
                    if events
                        .send(LaneEvent::Output {
                            terminal_id: terminal_id.clone(),
                            chunk,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

/// Waits for one command and reports its exit exactly once.
fn spawn_reaper(
    running: Arc<Mutex<HashMap<String, Child>>>,
    terminal_id: String,
    events: Sender<LaneEvent>,
) {
    std::thread::spawn(move || {
        loop {
            let status = {
                let mut guard = running.lock().expect("background lane poisoned");
                let Some(child) = guard.get_mut(&terminal_id) else {
                    // Released or reaped elsewhere; nothing left to report.
                    return;
                };
                child.try_wait()
            };
            match status {
                Ok(Some(status)) => {
                    running
                        .lock()
                        .expect("background lane poisoned")
                        .remove(&terminal_id);
                    let _ = events.send(LaneEvent::Exit {
                        terminal_id,
                        exit_code: status.code(),
                        signal: exit_signal_name(&status),
                    });
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    running
                        .lock()
                        .expect("background lane poisoned")
                        .remove(&terminal_id);
                    let _ = events.send(LaneEvent::Exit {
                        terminal_id,
                        exit_code: None,
                        signal: None,
                    });
                    return;
                }
            }
        }
    });
}

fn exit_signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| {
        nix::sys::signal::Signal::try_from(signal)
            .map(|signal| signal.as_str().to_string())
            .unwrap_or_else(|_| format!("SIG{signal}"))
    })
}

fn signal_group(pid: u32, signal: nix::sys::signal::Signal) {
    let Ok(pid) = i32::try_from(pid) else { return };
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
}

/// Allowlisted slice of the shell's environment.
fn inherited_env() -> Vec<(String, String)> {
    ENV_ALLOWLIST
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_string(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn request(terminal_id: &str, command: &str, args: &[&str]) -> LaneRequest {
        LaneRequest {
            terminal_id: terminal_id.to_string(),
            command: command.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// Accumulates lane events for every terminal, so waiting on one never
    /// discards another's output.
    #[derive(Default)]
    struct Collector {
        output: HashMap<String, String>,
        exits: HashMap<String, LaneEvent>,
    }

    impl Collector {
        fn pump(&mut self, lane: &BackgroundLane) {
            for event in lane.drain_events() {
                match event {
                    LaneEvent::Output { terminal_id, chunk } => {
                        self.output.entry(terminal_id).or_default().push_str(&chunk);
                    }
                    LaneEvent::Exit {
                        ref terminal_id, ..
                    } => {
                        self.exits.insert(terminal_id.clone(), event);
                    }
                }
            }
        }

        /// Waits until `terminal_id` exits, returning its output and exit.
        fn wait_exit(
            &mut self,
            lane: &BackgroundLane,
            terminal_id: &str,
        ) -> (String, Option<LaneEvent>) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                self.pump(lane);
                if self.exits.contains_key(terminal_id) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // Drain once more: output can land in the same tick as the exit.
            self.pump(lane);
            (
                self.output.get(terminal_id).cloned().unwrap_or_default(),
                self.exits.get(terminal_id).cloned(),
            )
        }
    }

    #[test]
    fn streams_output_and_reports_exit_code() {
        let lane = BackgroundLane::default();
        lane.spawn(&request(
            "t1",
            "/bin/sh",
            &["-c", "echo out; echo err >&2; exit 3"],
        ))
        .expect("spawn");
        let (output, exit) = Collector::default().wait_exit(&lane, "t1");
        assert!(output.contains("out"), "{output}");
        assert!(output.contains("err"), "{output}");
        assert_eq!(
            exit,
            Some(LaneEvent::Exit {
                terminal_id: "t1".to_string(),
                exit_code: Some(3),
                signal: None,
            })
        );
        assert!(!lane.is_busy());
    }

    #[test]
    fn runs_several_terminals_concurrently() {
        let lane = BackgroundLane::default();
        for index in 0..4 {
            lane.spawn(&request(
                &format!("t{index}"),
                "/bin/sh",
                &["-c", "echo ready"],
            ))
            .expect("spawn");
        }
        let mut collector = Collector::default();
        for index in 0..4 {
            let (output, exit) = collector.wait_exit(&lane, &format!("t{index}"));
            assert!(output.contains("ready"), "terminal {index}: {output}");
            assert!(matches!(
                exit,
                Some(LaneEvent::Exit {
                    exit_code: Some(0),
                    ..
                })
            ));
        }
    }

    #[test]
    fn kill_terminates_the_whole_group() {
        let lane = BackgroundLane::default();
        // The inner sleep is a grandchild: killing only the direct child
        // would leave it running.
        lane.spawn(&request("t1", "/bin/sh", &["-c", "sleep 30 & wait"]))
            .expect("spawn");
        lane.kill("t1");
        let (_, exit) = Collector::default().wait_exit(&lane, "t1");
        let Some(LaneEvent::Exit {
            exit_code, signal, ..
        }) = exit
        else {
            panic!("killed terminal must report an exit");
        };
        assert!(
            signal.is_some() || exit_code.is_some(),
            "kill must produce a terminal status"
        );
        assert!(!lane.is_busy());
    }

    #[test]
    fn spawn_failure_is_reported_not_silent() {
        let lane = BackgroundLane::default();
        let error = lane
            .spawn(&request("t1", "/nonexistent/cosh-lane-test", &[]))
            .expect_err("spawn must fail");
        assert!(error.contains("failed to start"), "{error}");
        assert!(!lane.is_busy());
    }

    #[test]
    fn environment_is_allowlisted() {
        // A COSH_* internal and an unrelated secret must not survive.
        let _guard = crate::diagnostics::test_env::env_guard();
        std::env::set_var("COSH_LANE_TEST_INTERNAL", "internal");
        std::env::set_var("MY_LANE_TEST_SECRET", "secret");
        let lane = BackgroundLane::default();
        lane.spawn(&request("t1", "/bin/sh", &["-c", "env"]))
            .expect("spawn");
        let (output, _) = Collector::default().wait_exit(&lane, "t1");
        assert!(!output.contains("COSH_LANE_TEST_INTERNAL"), "{output}");
        assert!(!output.contains("MY_LANE_TEST_SECRET"), "{output}");
        assert!(output.contains("PATH="), "{output}");
        std::env::remove_var("COSH_LANE_TEST_INTERNAL");
        std::env::remove_var("MY_LANE_TEST_SECRET");
    }

    #[test]
    fn request_env_reaches_the_command() {
        let lane = BackgroundLane::default();
        let mut request = request("t1", "/bin/sh", &["-c", "echo $LANE_FROM_AGENT"]);
        request.env = vec![("LANE_FROM_AGENT".to_string(), "visible".to_string())];
        lane.spawn(&request).expect("spawn");
        let (output, _) = Collector::default().wait_exit(&lane, "t1");
        assert!(output.contains("visible"), "{output}");
    }

    #[test]
    fn cwd_is_honored() {
        let dir = std::env::temp_dir();
        let lane = BackgroundLane::default();
        let mut request = request("t1", "/bin/sh", &["-c", "pwd"]);
        request.cwd = Some(dir.to_string_lossy().to_string());
        lane.spawn(&request).expect("spawn");
        let (output, _) = Collector::default().wait_exit(&lane, "t1");
        let canonical = std::fs::canonicalize(&dir).unwrap_or(dir);
        assert!(
            output.trim().ends_with(
                canonical
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .as_str()
            ),
            "{output}"
        );
    }
}
