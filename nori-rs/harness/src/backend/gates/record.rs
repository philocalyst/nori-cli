//! What every gate decision leaves behind: a journal record, and sometimes one quiet line.

use nori_pkl::kinds::GateEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;

use crate::backend::AcpBackend;
use crate::backend::BackendEvent;

/// The journal command a decision is recorded as.
pub(crate) const GATE_COMMAND: &str = "gate";

/// The longest a subject is shown in a line.
const LINE_SUBJECT_CHARS: usize = 100;

/// One decision of one rule.
pub(crate) struct Decided<'a> {
    pub(crate) event: GateEvent,
    pub(crate) gate: &'a str,
    /// The rule's position in the gate.
    pub(crate) rule: i64,
    /// `allow`, `deny`, `ask`, `rewrite`, `continue` or `pass`.
    pub(crate) decision: &'static str,
    /// What the decision was about: a command, a tool call, the agent's message.
    pub(crate) subject: &'a str,
    /// The reason, the changes made, or the prompt handed over.
    pub(crate) detail: Option<&'a str>,
    /// The one line the person sees, if any.
    pub(crate) line: Option<String>,
}

impl<'a> Decided<'a> {
    pub(crate) fn new(
        event: GateEvent,
        gate: &'a str,
        rule: i64,
        decision: &'static str,
        subject: &'a str,
    ) -> Self {
        Self {
            event,
            gate,
            rule,
            decision,
            subject,
            detail: None,
            line: None,
        }
    }

    /// Adds the reason, the changes or the prompt.
    pub(crate) fn detail(mut self, detail: &'a str) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Adds the line the person sees.
    pub(crate) fn line(mut self, line: String) -> Self {
        self.line = Some(line);
        self
    }
}

/// `text` on one line, cut to a length a notice can carry.
pub(crate) fn brief(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(LINE_SUBJECT_CHARS) {
        Some((end, _)) => format!("{}...", &flat[..end]),
        None => flat,
    }
}

impl AcpBackend {
    /// Journal `decided` and show its line. Every decision is journaled; only the ones the person
    /// would not otherwise notice have a line.
    pub(crate) async fn record_gate(&self, decided: Decided<'_>) {
        if let Some(recorder) = self.transcript_recorder.read().await.clone() {
            let payload = serde_json::json!({
                "gate": decided.gate,
                "rule": decided.rule,
                "event": decided.event.as_str(),
                "decision": decided.decision,
                "subject": decided.subject,
                "detail": decided.detail,
                "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            });
            if let Err(error) = recorder
                .record_command(GATE_COMMAND, payload.to_string())
                .await
            {
                tracing::warn!(%error, "failed to journal a gate decision");
            }
        }
        if let Some(message) = decided.line {
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                    nori_protocol::Notice { message },
                ))))
                .await;
        }
    }
}
