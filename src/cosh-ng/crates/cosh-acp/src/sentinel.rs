//! Process-tree sentinel over the agent's children.
//!
//! A Tier 2/3 agent that runs commands itself instead of through
//! `terminal/create` leaves the audited path, and the shell has no other way
//! to notice. The bridge therefore watches the agent's direct children and
//! reports anything that is not one of the MCP servers it was told about
//! (ADR-011 trust tiers, ADR-012 audit ownership).
//!
//! Detection is observational: it never blocks or kills the child, because a
//! false positive must not break a working agent.

use std::collections::HashSet;
use std::path::Path;

use crate::protocol::McpServerSpec;

/// One unexpected child process observed under the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalExec {
    pub(crate) pid: u32,
    /// Executable as reported by the kernel; arguments are excluded because
    /// they routinely carry paths and secrets.
    pub(crate) command: String,
}

/// Watches the agent's direct children for unaudited execution.
pub(crate) struct ProcessTreeSentinel {
    agent_pid: u32,
    /// Executable basenames the agent is allowed to spawn.
    allowed: HashSet<String>,
    /// Pids already reported, so one long-lived child is reported once.
    reported: HashSet<u32>,
}

impl ProcessTreeSentinel {
    pub(crate) fn new(agent_pid: u32, mcp_servers: &[McpServerSpec]) -> Self {
        let allowed = mcp_servers
            .iter()
            .map(|server| basename(&server.command))
            .collect();
        Self {
            agent_pid,
            allowed,
            reported: HashSet::new(),
        }
    }

    /// Returns children seen for the first time that the agent should not have.
    pub(crate) fn scan(&mut self) -> Vec<LocalExec> {
        self.scan_root(Path::new("/proc"))
    }

    fn scan_root(&mut self, proc_root: &Path) -> Vec<LocalExec> {
        let Ok(entries) = std::fs::read_dir(proc_root) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == self.agent_pid || self.reported.contains(&pid) {
                continue;
            }
            let Some(parent) = parent_pid(&entry.path()) else {
                continue;
            };
            if parent != self.agent_pid {
                continue;
            }
            let command = executable_name(&entry.path());
            // Record before filtering so an allowed child is not re-examined
            // on every tick.
            self.reported.insert(pid);
            if self.allowed.contains(&command) {
                continue;
            }
            found.push(LocalExec { pid, command });
        }
        found
    }
}

/// Reads the parent pid from `/proc/<pid>/stat`.
///
/// The comm field is parenthesized and may itself contain spaces and
/// parentheses, so parsing starts after the last `)`.
fn parent_pid(proc_entry: &Path) -> Option<u32> {
    let stat = std::fs::read_to_string(proc_entry.join("stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    tail.split_whitespace().nth(1)?.parse().ok()
}

/// Reads the executable name, preferring the resolved binary over `comm`,
/// which the process can rename.
fn executable_name(proc_entry: &Path) -> String {
    if let Ok(exe) = std::fs::read_link(proc_entry.join("exe")) {
        return basename(&exe.to_string_lossy());
    }
    std::fs::read_to_string(proc_entry.join("comm"))
        .map(|comm| comm.trim().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

fn basename(command: &str) -> String {
    Path::new(command)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| command.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(command: &str) -> McpServerSpec {
        McpServerSpec {
            name: "evidence".to_string(),
            command: command.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Builds a fake `/proc` so the scan is testable without real processes.
    fn fake_proc(entries: &[(u32, u32, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp proc");
        for (pid, ppid, comm) in entries {
            let dir = root.path().join(pid.to_string());
            std::fs::create_dir_all(&dir).expect("create pid dir");
            // Mirror the kernel's parenthesized comm field, including a name
            // that itself contains a space and a bracket.
            std::fs::write(dir.join("stat"), format!("{pid} (od d) S {ppid} 0 0"))
                .expect("write stat");
            std::fs::write(dir.join("comm"), format!("{comm}\n")).expect("write comm");
        }
        root
    }

    #[test]
    fn reports_an_unexpected_child_once() {
        let proc_root = fake_proc(&[(100, 1, "cosh-core"), (101, 100, "bash")]);
        let mut sentinel = ProcessTreeSentinel::new(100, &[]);
        let found = sentinel.scan_root(proc_root.path());
        assert_eq!(
            found,
            vec![LocalExec {
                pid: 101,
                command: "bash".to_string()
            }]
        );
        assert!(
            sentinel.scan_root(proc_root.path()).is_empty(),
            "one child must not be reported twice"
        );
    }

    #[test]
    fn declared_mcp_servers_are_expected() {
        let proc_root = fake_proc(&[(100, 1, "cosh-core"), (102, 100, "cosh-acp")]);
        let mut sentinel = ProcessTreeSentinel::new(100, &[spec("/usr/bin/cosh-acp")]);
        assert!(sentinel.scan_root(proc_root.path()).is_empty());
    }

    #[test]
    fn unrelated_processes_are_ignored() {
        let proc_root = fake_proc(&[(100, 1, "cosh-core"), (200, 7, "sshd")]);
        let mut sentinel = ProcessTreeSentinel::new(100, &[]);
        assert!(sentinel.scan_root(proc_root.path()).is_empty());
    }

    #[test]
    fn parent_pid_survives_a_comm_containing_spaces_and_brackets() {
        let proc_root = fake_proc(&[(100, 1, "cosh-core"), (101, 100, "bash")]);
        assert_eq!(parent_pid(&proc_root.path().join("101")), Some(100));
    }
}
