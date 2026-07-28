//! Registry of client-bound requests parked while cosh-shell answers them.
//!
//! The bridge answers ACP requests from the agent (permissions, `_cosh/*`
//! extensions) only after the shell replies, so each `Responder` must outlive
//! its handler. Responders are keyed by a bridge-minted request id that also
//! travels on the JSONL protocol.
//!
//! Every parked responder is answered exactly once: either by the matching
//! shell reply or by `fail_all` when the stream ends, so the agent can never
//! be left waiting on a bridge that has gone away.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::Responder;

/// Parked responders for requests the shell must answer.
#[derive(Default)]
pub(crate) struct PendingRequests {
    permissions: Mutex<HashMap<String, Responder<acp::RequestPermissionResponse>>>,
    extensions: Mutex<HashMap<String, Responder<serde_json::Value>>>,
    next_id: AtomicU64,
}

impl PendingRequests {
    fn next_id(&self, prefix: &str) -> String {
        let ordinal = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{ordinal}")
    }

    /// Parks a permission request and returns the id the shell will echo.
    pub(crate) fn park_permission(
        &self,
        responder: Responder<acp::RequestPermissionResponse>,
    ) -> String {
        let request_id = self.next_id("perm");
        if let Ok(mut parked) = self.permissions.lock() {
            parked.insert(request_id.clone(), responder);
        }
        request_id
    }

    /// Answers a parked permission request with the shell's decision.
    pub(crate) fn answer_permission(
        &self,
        request_id: &str,
        option_id: Option<String>,
        cancelled: bool,
    ) {
        let Some(responder) = self
            .permissions
            .lock()
            .ok()
            .and_then(|mut parked| parked.remove(request_id))
        else {
            tracing::warn!(request_id, "no permission request is waiting");
            return;
        };
        let outcome = match option_id {
            Some(option_id) if !cancelled => acp::RequestPermissionOutcome::Selected(
                acp::SelectedPermissionOutcome::new(option_id),
            ),
            _ => acp::RequestPermissionOutcome::Cancelled,
        };
        let _ = responder.respond(acp::RequestPermissionResponse::new(outcome));
    }

    /// Parks an extension request and returns the id the shell will echo.
    pub(crate) fn park_extension(&self, responder: Responder<serde_json::Value>) -> String {
        let request_id = self.next_id("ext");
        if let Ok(mut parked) = self.extensions.lock() {
            parked.insert(request_id.clone(), responder);
        }
        request_id
    }

    /// Answers a parked extension request with an arbitrary result value.
    pub(crate) fn answer_extension(&self, request_id: &str, value: serde_json::Value) {
        let Some(responder) = self
            .extensions
            .lock()
            .ok()
            .and_then(|mut parked| parked.remove(request_id))
        else {
            tracing::warn!(request_id, "no extension request is waiting");
            return;
        };
        let _ = responder.respond(value);
    }

    /// Fails every parked request so the agent stops waiting on a dead bridge.
    pub(crate) fn fail_all(&self, reason: &str) {
        if let Ok(mut parked) = self.permissions.lock() {
            for (_, responder) in parked.drain() {
                let _ = responder.respond(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Cancelled,
                ));
            }
        }
        if let Ok(mut parked) = self.extensions.lock() {
            for (_, responder) in parked.drain() {
                let _ = responder.respond_with_error(
                    agent_client_protocol::Error::internal_error().data(reason.to_string()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_unique_per_kind() {
        let pending = PendingRequests::default();
        let first = pending.next_id("perm");
        let second = pending.next_id("perm");
        assert_ne!(first, second);
        assert!(first.starts_with("perm-"), "{first}");
    }

    #[test]
    fn answering_an_unknown_id_is_ignored() {
        let pending = PendingRequests::default();
        pending.answer_permission("perm-404", Some("allow_once".to_string()), false);
        pending.answer_extension("ext-404", serde_json::json!({}));
    }
}
