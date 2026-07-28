//! Adapter that talks to the cosh-acp bridge over the internal JSONL protocol.
//!
//! cosh-shell has no internal crate dependencies, so the wire format is
//! mirrored here by hand; `crates/cosh-acp/src/protocol.rs` is the source of
//! truth for the contract (protocol v1, ADR-011).
//!
//! Scope: handshake, prompt dispatch, event mapping, cancellation, bridge
//! process custody, and dual-lane terminal delegation (see `acp/terminal.rs`).
//! Permissions beyond command approval and auth land with S4.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::command::BackgroundLane;
use crate::types::{AgentEvent, AgentRequest, CoshApprovalMode};

mod run;
mod terminal;
mod wire;

use self::run::start_bridge_run;
use super::claude::{send_agent_event, terminate_process};
use super::prompt_from_request;
use super::{
    control_protocol, start_threaded_adapter_run, AdapterError, AdapterInstance, AgentAdapter,
    AgentBackendCapabilities, AgentRunHandle, ProviderCancellationArtifactStore,
};

/// Internal JSONL protocol version this adapter speaks.
pub(super) const ACP_BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// Adapter that delegates Agent turns to an ACP agent via the cosh-acp bridge.
#[derive(Debug, Clone)]
pub struct AcpAdapter {
    /// cosh-acp executable path.
    pub program: String,
    /// Configured agent name (used for diagnostics and trust tiering).
    pub agent_name: String,
    /// Agent launch command forwarded to the bridge.
    pub agent_command: String,
    /// Agent launch arguments forwarded to the bridge.
    pub agent_args: Vec<String>,
    /// Environment injected into the agent at spawn time; may hold secrets.
    pub agent_env: BTreeMap<String, String>,
    /// Whether the agent is trusted to keep its own execution tools.
    pub agent_trusted: bool,
    /// Whether this adapter may start real bridge/agent processes.
    pub allow_spawn: bool,
    /// Agent session id committed by a completed turn.
    ///
    /// The shell reloads it on the next turn so the conversation continues,
    /// even though each turn gets a fresh bridge process: continuity lives in
    /// the agent's session store, not in a long-lived process (ADR-011).
    pub session_id: Arc<Mutex<Option<String>>>,
}

impl Default for AcpAdapter {
    fn default() -> Self {
        Self {
            program: discover_binary("COSH_ACP_PATH", "cosh-acp"),
            agent_name: "cosh-core".to_string(),
            agent_command: discover_binary("COSH_CORE_PATH", "cosh-core"),
            agent_args: vec!["--acp".to_string()],
            agent_env: BTreeMap::new(),
            agent_trusted: false,
            allow_spawn: false,
            session_id: Arc::new(Mutex::new(None)),
        }
    }
}

/// Resolves a companion binary: env override, then sibling, then PATH.
fn discover_binary(env_var: &str, name: &str) -> String {
    if let Ok(path) = std::env::var(env_var) {
        if !path.is_empty() {
            return path;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling.to_string_lossy().into_owned();
            }
        }
    }
    name.to_string()
}

impl AcpAdapter {
    /// Creates an adapter for explicit bridge and agent executables.
    pub fn new(program: impl Into<String>, allow_spawn: bool) -> Self {
        Self {
            program: program.into(),
            allow_spawn,
            ..Self::default()
        }
    }

    /// Applies `[acp]` configuration and enables real bridge spawning.
    ///
    /// An unknown or unset agent name keeps the built-in cosh-core defaults,
    /// so a typo degrades to the shipped agent instead of failing startup;
    /// the mismatch is reported by diagnostics.
    pub fn with_config(mut self, config: &crate::config::AcpConfig) -> Self {
        self.allow_spawn = true;
        if config.agent.is_empty() {
            return self;
        }
        let Some(agent) = config.agents.get(&config.agent) else {
            tracing::warn!(
                agent = %config.agent,
                "acp.agent names no configured agent; using the built-in one"
            );
            return self;
        };
        self.agent_name = config.agent.clone();
        self.agent_command = agent.command.clone();
        self.agent_args = agent.args.clone();
        // Values are copied without logging: they may be credentials.
        self.agent_env = agent.env.clone();
        self.agent_trusted = agent.trusted;
        self
    }

    fn initialize_line(&self) -> String {
        let cwd = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let params = serde_json::json!({
            "method": "initialize",
            "protocol_version": ACP_BRIDGE_PROTOCOL_VERSION,
            "agent": {
                "name": self.agent_name,
                "command": self.agent_command,
                "args": self.agent_args,
                "env": self.agent_env,
            },
            "cwd": cwd,
            "mcp_servers": [],
            // Terminal delegation is withheld only from trusted agents, which
            // keep their own tools; everything else executes through the
            // shell's dual-lane executor and stays on the audited path
            // (ADR-011 trust tiers).
            "capabilities": { "terminal": !self.agent_trusted },
            // Only third-party agents are watched: cosh-core spawns hooks,
            // extensions, and the compactor as a matter of course, and its own
            // audit already records the commands it runs (ADR-011 tiers).
            "sentinel": !self.agent_is_cosh_core(),
            "locale": serde_json::Value::Null,
        });
        params.to_string()
    }

    /// Builds the prompt message for the session the bridge just created.
    ///
    /// The session id comes from the agent, not from the shell: the shell's
    /// own session id names a terminal session, not an agent transcript.
    fn prompt_line(request: &AgentRequest, session_id: &str, mode: CoshApprovalMode) -> String {
        let approval_mode = match mode {
            CoshApprovalMode::Recommend => "strict",
            CoshApprovalMode::Auto => "auto",
            CoshApprovalMode::Trust => "trust",
        };
        serde_json::json!({
            "method": "prompt",
            "request_id": request.id,
            "session_id": session_id,
            "text": prompt_from_request(request),
            "approval_mode": approval_mode,
        })
        .to_string()
    }

    /// True when the configured agent is the built-in cosh-core agent.
    ///
    /// It serves the registry control plane over its own side channel
    /// (ADR-012) and records the commands it runs, so it needs neither the
    /// registry fallback nor the process-tree sentinel.
    pub(super) fn agent_is_cosh_core(&self) -> bool {
        self.agent_args.iter().any(|arg| arg == "--acp")
            && std::path::Path::new(&self.agent_command)
                .file_name()
                .is_some_and(|name| name == "cosh-core")
    }

    /// True when the registry control plane is reachable for this agent.
    pub(super) fn serves_registry(&self) -> bool {
        self.agent_is_cosh_core()
    }

    /// Detaches from the committed session so the next turn starts fresh.
    pub(super) fn start_fresh_session(&self) -> super::FreshSessionOutcome {
        super::detach_committed_session(&self.session_id)
    }

    /// Starts a cancellable bridge turn.
    pub fn start_cancellable(
        &self,
        request: AgentRequest,
        mode: CoshApprovalMode,
    ) -> AgentRunHandle {
        if !self.allow_spawn {
            let adapter = AdapterInstance::Acp(self.clone());
            return start_threaded_adapter_run(adapter, request);
        }
        start_bridge_run(self.clone(), request, mode)
    }
}

impl AgentAdapter for AcpAdapter {
    fn name(&self) -> &'static str {
        "acp"
    }

    fn capabilities(&self) -> AgentBackendCapabilities {
        AgentBackendCapabilities {
            text_stream: true,
            thinking_stream: true,
            session_resume: false,
            tool_intent: false,
            user_question: false,
            cancellable: true,
            control_protocol: false,
            registry: self.serves_registry(),
        }
    }

    fn run(&self, request: &AgentRequest) -> Result<Vec<AgentEvent>, AdapterError> {
        // Dry-run mode: report the prepared invocation without spawning.
        Ok(vec![AgentEvent::StatusChanged {
            run_id: request.id.clone(),
            phase: "prepared".to_string(),
            message: format!(
                "cosh-acp bridge prepared: {} bridge (agent: {} {})",
                self.program,
                self.agent_command,
                self.agent_args.join(" ")
            ),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::wire::{map_bridge_event, BridgeEvent};
    use super::*;
    use crate::types::{AgentMode, CommandBlock, CommandStatus, OutputRefs};

    fn request() -> AgentRequest {
        AgentRequest {
            id: "run-1".to_string(),
            session_id: "session-1".to_string(),
            command_block: CommandBlock {
                id: "blk".to_string(),
                session_id: "session-1".to_string(),
                command: "echo test".to_string(),
                origin: Default::default(),
                cwd: "/tmp".to_string(),
                end_cwd: "/tmp".to_string(),
                started_at_ms: 0,
                ended_at_ms: 0,
                duration_ms: 0,
                exit_code: 1,
                status: CommandStatus::Failed,
                output: OutputRefs {
                    terminal_output_ref: None,
                    terminal_output_bytes: 0,
                },
                shell_environment_generation: None,
                audit_identity: None,
            },
            context_blocks: Vec::new(),
            context_hints: Vec::new(),
            user_input: Some("why did it fail".to_string()),
            findings: Vec::new(),
            mode: AgentMode::RecommendOnly,
            user_confirmed: false,
            hook_finding: None,
            recommended_skill: None,
        }
    }

    #[test]
    fn initialize_line_declares_protocol_v1() {
        let adapter = AcpAdapter::default();
        let line = adapter.initialize_line();
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["agent"]["args"][0], "--acp");
    }

    #[test]
    fn config_selects_the_named_agent_and_enables_spawning() {
        let mut config = crate::config::AcpConfig {
            agent: "claude".to_string(),
            ..Default::default()
        };
        config.agents.insert(
            "claude".to_string(),
            crate::config::AcpAgentConfig {
                command: "claude".to_string(),
                args: vec!["--acp".to_string()],
                env: BTreeMap::from([("ANTHROPIC_API_KEY".to_string(), "secret".to_string())]),
                trusted: true,
            },
        );
        let adapter = AcpAdapter::default().with_config(&config);
        assert!(adapter.allow_spawn);
        assert_eq!(adapter.agent_command, "claude");
        assert!(adapter.agent_trusted);
        let value: serde_json::Value =
            serde_json::from_str(&adapter.initialize_line()).expect("json");
        // A trusted agent keeps its own tools, so terminals are not offered.
        assert_eq!(value["capabilities"]["terminal"], false);
        assert_eq!(value["agent"]["env"]["ANTHROPIC_API_KEY"], "secret");
    }

    #[test]
    fn builtin_agent_is_not_watched_but_a_third_party_one_is() {
        let builtin: serde_json::Value =
            serde_json::from_str(&AcpAdapter::default().initialize_line()).expect("json");
        assert_eq!(builtin["sentinel"], false);

        let mut config = crate::config::AcpConfig {
            agent: "other".to_string(),
            ..Default::default()
        };
        config.agents.insert(
            "other".to_string(),
            crate::config::AcpAgentConfig {
                command: "some-agent".to_string(),
                ..Default::default()
            },
        );
        let third_party: serde_json::Value =
            serde_json::from_str(&AcpAdapter::default().with_config(&config).initialize_line())
                .expect("json");
        assert_eq!(third_party["sentinel"], true);
    }

    #[test]
    fn unknown_agent_name_falls_back_to_the_builtin_agent() {
        let config = crate::config::AcpConfig {
            agent: "typo".to_string(),
            ..Default::default()
        };
        let adapter = AcpAdapter::default().with_config(&config);
        assert!(adapter.allow_spawn);
        assert_eq!(adapter.agent_name, "cosh-core");
    }

    #[test]
    fn committed_session_is_reloaded_and_detachable() {
        let adapter = AcpAdapter::default();
        assert_eq!(adapter.session_id.lock().expect("lock").clone(), None);
        *adapter.session_id.lock().expect("lock") = Some("agent-session".to_string());

        let instance = AdapterInstance::Acp(adapter.clone());
        assert_eq!(
            instance.committed_session_id(),
            Some("agent-session".to_string())
        );
        // Detaching must not delete the transcript, only stop reloading it.
        adapter.start_fresh_session();
        assert_eq!(instance.committed_session_id(), None);
    }

    #[test]
    fn session_load_targets_the_committed_session() {
        let line = super::wire::session_load_line("agent-session");
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["method"], "session_load");
        assert_eq!(value["session_id"], "agent-session");
    }

    #[test]
    fn prompt_line_carries_approval_mode() {
        let line = AcpAdapter::prompt_line(&request(), "agent-session", CoshApprovalMode::Trust);
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["method"], "prompt");
        assert_eq!(value["approval_mode"], "trust");
        assert_eq!(value["request_id"], "run-1");
    }

    #[test]
    fn text_delta_maps_to_agent_event() {
        let mut terminal_seen = false;
        let event = map_bridge_event(
            "run-1",
            BridgeEvent::TextDelta {
                text: "hello".to_string(),
            },
            &mut terminal_seen,
        );
        assert_eq!(
            event,
            Some(AgentEvent::TextDelta {
                run_id: "run-1".to_string(),
                text: "hello".to_string(),
            })
        );
        assert!(!terminal_seen);
    }

    #[test]
    fn agent_failed_is_terminal_and_keeps_code() {
        let mut terminal_seen = false;
        let event = map_bridge_event(
            "run-1",
            BridgeEvent::AgentFailed {
                code: "not_implemented".to_string(),
                message: "no wiring".to_string(),
                recoverable: true,
                hint: None,
            },
            &mut terminal_seen,
        );
        assert!(terminal_seen);
        let Some(AgentEvent::AgentFailed { error, .. }) = event else {
            panic!("expected failure event");
        };
        assert!(error.contains("not_implemented"), "{error}");
        assert!(error.contains("recoverable"), "{error}");
    }

    #[test]
    fn unknown_bridge_events_are_ignored() {
        let parsed: BridgeEvent =
            serde_json::from_str("{\"event\":\"future_thing\",\"x\":1}").expect("parse");
        let mut terminal_seen = false;
        assert_eq!(map_bridge_event("run-1", parsed, &mut terminal_seen), None);
        assert!(!terminal_seen);
    }

    #[test]
    fn dry_run_reports_prepared_invocation() {
        let adapter = AcpAdapter::default();
        let events = adapter.run(&request()).expect("dry run");
        assert_eq!(events.len(), 1);
        let AgentEvent::StatusChanged { phase, message, .. } = &events[0] else {
            panic!("expected status event");
        };
        assert_eq!(phase, "prepared");
        assert!(message.contains("bridge"), "{message}");
    }
}
