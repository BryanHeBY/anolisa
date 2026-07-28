//! ACP agent-side server mode (`cosh-core --acp`).
//!
//! The turn engine is not reimplemented here. This module serves ACP over
//! stdio and drives the existing headless driver over a private in-process
//! JSONL channel, translating in both directions (ADR-011 S4). Streaming,
//! approval, cancellation, resume, and auth therefore keep exactly one
//! implementation, so the two transports cannot drift.
//!
//! One process serves one session, matching the engine's single-transcript
//! model and the bridge's one-agent-per-turn custody rule.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder, Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;

use crate::cli::CliArgs;
use crate::config::CoreConfig;

mod sink;
mod translate;

use sink::LineSink;

/// Capacity of the private JSONL channel feeding the engine.
const ENGINE_PIPE_BYTES: usize = 256 * 1024;

/// Per-turn approval mode carried in `session/prompt`'s `_meta`.
///
/// Approval mode is a property of the turn, not of the process, so it travels
/// with each prompt instead of the launch arguments (ADR-011).
const APPROVAL_MODE_META_KEY: &str = "_cosh/approval_mode";

/// Approval modes the engine understands; anything else is ignored so a
/// client cannot inject arbitrary config through `_meta`.
const APPROVAL_MODES: &[&str] = &["trust", "auto", "balanced", "strict"];

/// Serves ACP over stdio until the client disconnects.
///
/// Returns the process exit code.
pub async fn run(args: &CliArgs, config: CoreConfig) -> i32 {
    let (engine_lines_tx, engine_lines_rx) = mpsc::unbounded_channel();
    let state = Arc::new(AcpState::new(args.clone(), config, engine_lines_tx));

    let outcome = Agent
        .builder()
        .name("cosh-core")
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: acp::InitializeRequest,
                            responder: Responder<acp::InitializeResponse>,
                            _cx: ConnectionTo<Client>| {
                    state.record_client_capabilities(&request.client_capabilities);
                    responder.respond(initialize_response(request.protocol_version))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |_request: acp::NewSessionRequest,
                            responder: Responder<acp::NewSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    // The engine mints the session id; the responder is
                    // completed by the translator once it reports one.
                    state
                        .begin_session(SessionResponder::New(responder), None)
                        .await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: acp::LoadSessionRequest,
                            responder: Responder<acp::LoadSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    let resume = request.session_id.to_string();
                    state
                        .begin_session(SessionResponder::Load(responder), Some(resume))
                        .await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: acp::PromptRequest,
                            responder: Responder<acp::PromptResponse>,
                            _cx: ConnectionTo<Client>| {
                    state.begin_prompt(request, responder).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = Arc::clone(&state);
                async move |notification: acp::CancelNotification, _cx: ConnectionTo<Client>| {
                    state.cancel(&notification.session_id.to_string()).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Stdio::new(), {
            let state = Arc::clone(&state);
            async move |cx: ConnectionTo<Client>| {
                let pump = translate::pump(Arc::clone(&state), cx.clone(), engine_lines_rx);
                tokio::select! {
                    () = pump => {}
                    () = cx.incoming_closed() => {}
                }
                state.shutdown().await;
                Ok(())
            }
        })
        .await;

    match outcome {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!("acp connection failed: {error}");
            1
        }
    }
}

/// Capabilities cosh-core actually implements today.
fn initialize_response(version: ProtocolVersion) -> acp::InitializeResponse {
    acp::InitializeResponse::new(version)
        .agent_capabilities(acp::AgentCapabilities::new().load_session(true))
        .agent_info(acp::Implementation::new(
            "cosh-core",
            env!("CARGO_PKG_VERSION"),
        ))
}

/// Responder parked until the engine reports its session id.
enum SessionResponder {
    New(Responder<acp::NewSessionResponse>),
    Load(Responder<acp::LoadSessionResponse>),
}

/// State shared by the ACP handlers and the translation pump.
pub(crate) struct AcpState {
    args: CliArgs,
    config: CoreConfig,
    engine_lines: mpsc::UnboundedSender<String>,
    /// True when the client executes commands for us via `terminal/*`; the
    /// agent must then stop running its own shell (ADR-011 tier 1).
    client_terminal: AtomicBool,
    /// Writer into the engine's JSONL input; `None` before the first session.
    engine: tokio::sync::Mutex<Option<DuplexStream>>,
    session_id: Mutex<Option<String>>,
    pending_session: Mutex<Option<SessionResponder>>,
    pending_prompt: Mutex<Option<Responder<acp::PromptResponse>>>,
    /// Set by `session/cancel`, cleared when the turn result is reported.
    cancelled: AtomicBool,
}

impl AcpState {
    fn new(args: CliArgs, config: CoreConfig, engine_lines: mpsc::UnboundedSender<String>) -> Self {
        Self {
            args,
            config,
            engine_lines,
            client_terminal: AtomicBool::new(false),
            engine: tokio::sync::Mutex::new(None),
            session_id: Mutex::new(None),
            pending_session: Mutex::new(None),
            pending_prompt: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        }
    }

    fn record_client_capabilities(&self, capabilities: &acp::ClientCapabilities) {
        self.client_terminal
            .store(capabilities.terminal, Ordering::Release);
        tracing::info!(
            terminal = capabilities.terminal,
            "acp client capabilities recorded"
        );
    }

    /// True when command execution must be delegated to the client.
    pub(crate) fn client_executes_commands(&self) -> bool {
        self.client_terminal.load(Ordering::Acquire)
    }

    pub(crate) fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok()?.clone()
    }

    /// Starts the engine for a `session/new` or `session/load` request.
    ///
    /// The responder is parked rather than answered here: the engine mints the
    /// session id, so the translator completes it when the id arrives.
    async fn begin_session(&self, responder: SessionResponder, resume: Option<String>) {
        if let Err(error) = self.park_session_responder(responder) {
            tracing::warn!("rejecting session request: {error}");
            return;
        }
        if let Err(error) = self.start_engine(resume).await {
            self.fail_session(error);
        }
    }

    fn park_session_responder(&self, responder: SessionResponder) -> Result<(), String> {
        let mut slot = self
            .pending_session
            .lock()
            .map_err(|_| "session state poisoned".to_string())?;
        if slot.is_some() {
            let error = acp::Error::invalid_request().data("a session request is already pending");
            responder.reject(error);
            return Err("duplicate session request".to_string());
        }
        *slot = Some(responder);
        Ok(())
    }

    fn fail_session(&self, error: acp::Error) {
        if let Some(responder) = self.pending_session.lock().ok().and_then(|mut s| s.take()) {
            responder.reject(error);
        }
    }

    /// Answers the parked session responder with the engine's session id.
    pub(crate) fn complete_session(&self, session_id: &str) {
        if let Ok(mut slot) = self.session_id.lock() {
            *slot = Some(session_id.to_string());
        }
        let Some(responder) = self.pending_session.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        let outcome = match responder {
            SessionResponder::New(responder) => {
                responder.respond(acp::NewSessionResponse::new(session_id.to_string()))
            }
            SessionResponder::Load(responder) => responder.respond(acp::LoadSessionResponse::new()),
        };
        if let Err(error) = outcome {
            tracing::warn!("failed to answer session request: {error}");
        }
    }

    /// Spawns the headless engine over a private JSONL duplex channel.
    async fn start_engine(&self, resume: Option<String>) -> Result<(), acp::Error> {
        let mut slot = self.engine.lock().await;
        if slot.is_some() {
            return Err(
                acp::Error::invalid_request().data("this agent process already serves a session")
            );
        }

        let mut engine_args = self.args.clone();
        engine_args.acp = false;
        engine_args.headless = true;
        // A parked single-shot prompt would race the ACP prompt turn.
        engine_args.prompt = None;
        engine_args.resume = resume;
        // Terminal-output evidence travels on the MCP data plane in ACP mode
        // (ADR-012), so the control-protocol evidence tool stays off.
        engine_args.enable_shell_evidence_tool = false;

        let config = self.config.clone();
        let sink = LineSink::new(self.engine_lines.clone());
        let (engine_side, acp_side) = tokio::io::duplex(ENGINE_PIPE_BYTES);
        tokio::spawn(async move {
            let lines = BufReader::new(engine_side).lines();
            match crate::headless::run_with_io(&engine_args, config, sink, lines).await {
                Ok(code) => tracing::info!(code, "acp engine finished"),
                Err(error) => tracing::error!("acp engine failed: {error}"),
            }
        });
        *slot = Some(acp_side);
        drop(slot);

        self.write_line(&serde_json::json!({
            "type": "control_request",
            "request_id": "acp-initialize",
            "request": { "subtype": "initialize" },
        }))
        .await
    }

    /// Forwards one prompt turn to the engine, parking the responder for the
    /// translator to complete when the turn result arrives.
    async fn begin_prompt(
        &self,
        request: acp::PromptRequest,
        responder: Responder<acp::PromptResponse>,
    ) {
        let session_id = request.session_id.to_string();
        match self.session_id() {
            Some(active) if active == session_id => {}
            Some(_) | None => {
                let _ = responder
                    .respond_with_error(acp::Error::invalid_params().data("unknown session id"));
                return;
            }
        }

        match self.pending_prompt.lock() {
            Ok(mut slot) if slot.is_none() => *slot = Some(responder),
            Ok(_) => {
                let _ = responder.respond_with_error(
                    acp::Error::invalid_request().data("a prompt is already in flight"),
                );
                return;
            }
            Err(_) => {
                let _ = responder
                    .respond_with_error(acp::Error::internal_error().data("prompt state poisoned"));
                return;
            }
        }
        self.cancelled.store(false, Ordering::Release);

        if let Err(error) = self.dispatch_prompt(&request, &session_id).await {
            self.finish_prompt(Err(error));
        }
    }

    async fn dispatch_prompt(
        &self,
        request: &acp::PromptRequest,
        session_id: &str,
    ) -> Result<(), acp::Error> {
        if let Some(mode) = approval_mode(request) {
            self.write_line(&serde_json::json!({
                "type": "control_request",
                "request_id": "acp-approval-mode",
                "request": { "subtype": "config_override", "approval_mode": mode },
            }))
            .await?;
        }
        self.write_line(&serde_json::json!({
            "type": "user",
            "session_id": session_id,
            "message": { "role": "user", "content": prompt_text(&request.prompt) },
        }))
        .await
    }

    /// Completes the in-flight prompt turn, if any.
    pub(crate) fn finish_prompt(&self, outcome: Result<acp::StopReason, acp::Error>) {
        let Some(responder) = self.pending_prompt.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        let outcome = if self.cancelled.swap(false, Ordering::AcqRel) {
            Ok(acp::StopReason::Cancelled)
        } else {
            outcome
        };
        let result = match outcome {
            Ok(reason) => responder.respond(acp::PromptResponse::new(reason)),
            Err(error) => responder.respond_with_error(error),
        };
        if let Err(error) = result {
            tracing::warn!("failed to answer prompt: {error}");
        }
    }

    /// Interrupts the in-flight turn; the engine reports the terminal result.
    async fn cancel(&self, session_id: &str) {
        if self.session_id().as_deref() != Some(session_id) {
            return;
        }
        self.cancelled.store(true, Ordering::Release);
        if let Err(error) = self
            .write_line(&serde_json::json!({
                "type": "control_request",
                "request_id": "acp-interrupt",
                "request": { "subtype": "interrupt" },
            }))
            .await
        {
            tracing::warn!("failed to forward cancellation: {error}");
        }
    }

    /// Asks the engine to shut down and drops the channel.
    async fn shutdown(&self) {
        let _ = self
            .write_line(&serde_json::json!({
                "type": "control_request",
                "request_id": "acp-shutdown",
                "request": { "subtype": "shutdown" },
            }))
            .await;
        self.engine.lock().await.take();
    }

    /// Writes one JSONL message to the engine.
    pub(crate) async fn write_line(&self, message: &serde_json::Value) -> Result<(), acp::Error> {
        let mut line = serde_json::to_string(message)
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        line.push('\n');
        let mut guard = self.engine.lock().await;
        let Some(stream) = guard.as_mut() else {
            return Err(acp::Error::invalid_request().data("no active session"));
        };
        stream
            .write_all(line.as_bytes())
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        stream
            .flush()
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))
    }
}

impl SessionResponder {
    fn reject(self, error: acp::Error) {
        let outcome = match self {
            SessionResponder::New(responder) => responder.respond_with_error(error),
            SessionResponder::Load(responder) => responder.respond_with_error(error),
        };
        if let Err(error) = outcome {
            tracing::warn!("failed to reject session request: {error}");
        }
    }
}

/// Reads the per-turn approval mode, ignoring values the engine rejects.
fn approval_mode(request: &acp::PromptRequest) -> Option<String> {
    let mode = request
        .meta
        .as_ref()?
        .get(APPROVAL_MODE_META_KEY)?
        .as_str()?;
    if APPROVAL_MODES.contains(&mode) {
        Some(mode.to_string())
    } else {
        tracing::warn!(mode, "ignoring unknown approval mode from _meta");
        None
    }
}

/// Flattens the prompt into the engine's single-string user message.
fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_with_meta(mode: &str) -> acp::PromptRequest {
        let mut meta = serde_json::Map::new();
        meta.insert(APPROVAL_MODE_META_KEY.to_string(), mode.into());
        acp::PromptRequest::new("session-1", vec!["hello".into()]).meta(meta)
    }

    #[test]
    fn approval_mode_is_read_from_meta() {
        assert_eq!(
            approval_mode(&prompt_with_meta("trust")),
            Some("trust".to_string())
        );
    }

    #[test]
    fn unknown_approval_mode_is_ignored() {
        assert_eq!(approval_mode(&prompt_with_meta("yolo")), None);
    }

    #[test]
    fn missing_meta_leaves_the_configured_mode() {
        let request = acp::PromptRequest::new("session-1", vec!["hello".into()]);
        assert_eq!(approval_mode(&request), None);
    }

    #[test]
    fn prompt_text_joins_text_blocks_and_skips_others() {
        let blocks = vec![
            acp::ContentBlock::from("first"),
            acp::ContentBlock::from("second"),
        ];
        assert_eq!(prompt_text(&blocks), "first\nsecond");
    }

    #[test]
    fn initialize_declares_session_loading() {
        let response = initialize_response(ProtocolVersion::V1);
        assert!(response.agent_capabilities.load_session);
    }
}
