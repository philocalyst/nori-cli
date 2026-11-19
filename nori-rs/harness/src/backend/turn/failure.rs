//! Turning a failed `session/prompt` into the words the user reads and the disposition the
//! ledger records.

use super::super::*;

use crate::normalized::session_runtime::QueuedPromptKind;

impl AcpBackend {
    /// Publish a correlated prompt failure and return its disposition for the
    /// reducer-owned completion.
    pub(super) async fn send_prompt_error(
        &self,
        request_id: &nori_protocol::acp::v1::RequestId,
        prompt_kind: QueuedPromptKind,
        err: &anyhow::Error,
    ) -> crate::normalized::TurnFailure {
        let (message, retryable) = match prompt_kind {
            QueuedPromptKind::Compact | QueuedPromptKind::NativeCompact => {
                (format!("Compact failed: {err}"), false)
            }
            QueuedPromptKind::GoalContinuation => {
                (format!("Goal continuation failed: {err}"), false)
            }
            QueuedPromptKind::TaskResults => (
                format!("Delivering delegated task results failed: {err}"),
                false,
            ),
            QueuedPromptKind::MonitorEvents => {
                (format!("Delivering monitor events failed: {err}"), false)
            }
            QueuedPromptKind::WorkflowHook => (format!("Workflow hook failed: {err}"), false),
            QueuedPromptKind::User => {
                let error_string = format!("{err:?}");
                let FailureDetails {
                    class,
                    message: agent_message,
                    detail,
                } = classify_failure(err);
                // Prefer the concise structured ACP message. Falling back to
                // top-level Display avoids dumping structured `data` as JSON.
                // Preserve clean agent-supplied `data.detail` parenthetically,
                // mirroring `enhance_agent_error`.
                let display_error = agent_message.unwrap_or_else(|| format!("{err}"));
                let retryable = class.is_transient();
                warn!(?class, retryable, "ACP user prompt failed: {error_string}");
                let message = match class {
                    FailureClass::Auth => {
                        format!(
                            "Authentication error: {display_error}. Please check your credentials or re-authenticate."
                        )
                    }
                    FailureClass::RateLimited { .. } => {
                        format!("Rate limited: {display_error}")
                    }
                    FailureClass::QuotaExhausted { resets_at } => match resets_at {
                        Some(resets_at) => {
                            format!("Usage exhausted until {resets_at}: {display_error}")
                        }
                        None => format!("Usage exhausted: {display_error}"),
                    },
                    FailureClass::ModelUnavailable => format!(
                        "The model is not available on this route (pick another with /model): {display_error}"
                    ),
                    FailureClass::Capacity => format!(
                        "The provider has no capacity for this model right now: {display_error}"
                    ),
                    FailureClass::ExecutableNotFound => {
                        format!("Agent executable not found: {display_error}")
                    }
                    FailureClass::Initialization => {
                        format!("Agent initialization failed: {display_error}")
                    }
                    FailureClass::PromptTooLong => {
                        "Prompt is too long. Try using /compact to reduce context size, or start a new session."
                            .to_string()
                    }
                    FailureClass::ServerError => {
                        "The API returned a server error. This is usually temporary — please try again."
                            .to_string()
                    }
                    FailureClass::SessionNotFound => format!(
                        "The session no longer exists on the agent — it may have expired or been closed elsewhere: {display_error}"
                    ),
                    FailureClass::SessionNotResumable => format!(
                        "The session can't be reattached — start a new session instead: {display_error}"
                    ),
                    FailureClass::SessionAlreadyActive => format!(
                        "A session is already active on the agent connection — close it before switching: {display_error}"
                    ),
                    FailureClass::NoActiveSession => format!(
                        "No session is active on the agent connection — start a new chat with /new: {display_error}"
                    ),
                    FailureClass::Network => format!(
                        "The agent's backing service is unreachable — check your network and try again: {display_error}"
                    ),
                    FailureClass::Unknown => format!("ACP prompt failed: {display_error}"),
                };
                // The agent-supplied `error.data.detail` is clean, user-facing
                // context (e.g. "connection reset by broker") — keep it verbatim.
                let message = match detail {
                    Some(detail) => format!("{message} ({detail})"),
                    None => message,
                };
                (message, retryable)
            }
        };

        debug!("{message}");
        // A quit ends the connection under the prompt: the ledger has already ended the turn, and
        // the user who quit is not owed an error about the request that was in flight.
        if self
            .is_shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return crate::normalized::TurnFailure::Fatal;
        }
        let (request_failure_kind, turn_failure) = if retryable {
            (
                nori_protocol::RequestFailureKind::Retryable,
                crate::normalized::TurnFailure::Retryable,
            )
        } else {
            (
                nori_protocol::RequestFailureKind::Fatal,
                crate::normalized::TurnFailure::Fatal,
            )
        };
        self.wait_for_prompt_phase().await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::RequestFailed(nori_protocol::RequestFailure {
                    request_id: Some(request_id.clone()),
                    message,
                    kind: request_failure_kind,
                }),
            )))
            .await;
        turn_failure
    }
}
