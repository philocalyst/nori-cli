//! What the chat widget does with each event the session sends.
//!
//! This module has the entry point (`handle_session_event`) and the error-message helpers; the
//! handlers live in modules by what they handle.

mod acp;
mod client;
mod monitors;
mod nori;
mod notices;
mod session;
mod stream;
mod tools;
mod turn;

use super::*;
use crate::client_tool_cell::ClientToolCell;
use crate::ui_types::PlanItem;
use crate::ui_types::PlanUpdate;
use crate::ui_types::StepStatus;

fn acp_error_detail(error: &nori_protocol::acp::v1::Error) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("detail"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|detail| !detail.is_empty() && *detail != error.message)
}

impl ChatWidget {
    pub(crate) fn acp_error_detail(error: &nori_protocol::acp::v1::Error) -> Option<&str> {
        acp_error_detail(error)
    }

    pub(crate) fn acp_error_message_with_detail(
        error: &nori_protocol::acp::v1::Error,
    ) -> Option<String> {
        acp_error_detail(error).map(|detail| format!("{}: {detail}", error.message))
    }

    pub(crate) fn acp_error_message(error: &nori_protocol::acp::v1::Error) -> String {
        Self::acp_error_message_with_detail(error).unwrap_or_else(|| error.message.clone())
    }

    pub(crate) fn handle_session_event(
        &mut self,
        generation: crate::app_event::SessionGeneration,
        event: nori_protocol::SessionEvent,
    ) {
        if generation != self.session_generation {
            return;
        }
        match event {
            nori_protocol::SessionEvent::Acp(event) => self.handle_acp_event(event),
            nori_protocol::SessionEvent::Nori(event) => self.handle_nori_event(event),
        }
    }
}
