//! Translating a provider's tool activity into ACP tool calls.
//!
//! Providers announce tools in their own ways (steps, events, items), and each announces the
//! same tool several times as it progresses. The [`ToolLedger`] turns that into what ACP wants:
//! one `tool_call` when a call first appears, then updates, and exactly one terminal update.

use std::collections::HashSet;

use serde_json::Value;

use crate::schema::ContentBlock;
use crate::schema::Meta;
use crate::schema::SessionUpdate;
use crate::schema::ToolCall;
use crate::schema::ToolCallContent;
use crate::schema::ToolCallStatus;
use crate::schema::ToolCallUpdate;
use crate::schema::ToolCallUpdateFields;

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
}

impl Outcome {
    fn status(self) -> ToolCallStatus {
        match self {
            Self::Completed => ToolCallStatus::Completed,
            Self::Failed => ToolCallStatus::Failed,
        }
    }
}

/// Which calls of one turn have been announced and which have ended.
#[derive(Debug, Default)]
pub struct ToolLedger {
    started: HashSet<String>,
    ended: HashSet<String>,
}

impl ToolLedger {
    /// Announce `call`. Yields the `tool_call` update the first time the id is seen, and
    /// nothing after, so a provider that repeats itself does not repeat a card.
    pub fn open(&mut self, call: ToolCall) -> Option<SessionUpdate> {
        self.started
            .insert(call.tool_call_id.to_string())
            .then_some(SessionUpdate::ToolCall(call))
    }

    /// Report progress on a call that was announced.
    pub fn progress(&mut self, update: ToolCallUpdate) -> Option<SessionUpdate> {
        let id = update.tool_call_id.to_string();
        (self.started.contains(&id) && !self.ended.contains(&id))
            .then_some(SessionUpdate::ToolCallUpdate(update))
    }

    /// End `call`. If it was never announced, `call` is announced first, so the client never
    /// sees an update for a call it does not know. Ending twice yields nothing the second time.
    pub fn close(
        &mut self,
        call: ToolCall,
        outcome: Outcome,
        content: Vec<ToolCallContent>,
        raw_output: Option<Value>,
        meta: Option<Meta>,
    ) -> Vec<SessionUpdate> {
        let id = call.tool_call_id.to_string();
        if !self.ended.insert(id.clone()) {
            return Vec::new();
        }
        let mut updates = Vec::new();
        let update = ToolCallUpdate::new(
            call.tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .status(outcome.status())
                .content(content)
                .raw_output(raw_output),
        )
        .meta(meta);
        if self.started.insert(id) {
            updates.push(SessionUpdate::ToolCall(
                call.status(ToolCallStatus::InProgress),
            ));
        }
        updates.push(SessionUpdate::ToolCallUpdate(update));
        updates
    }

    /// Whether the call has ended.
    pub fn is_ended(&self, id: &str) -> bool {
        self.ended.contains(id)
    }

    /// Record that a call was announced and ended elsewhere, such as a host tool the agent
    /// answered itself.
    pub fn mark_ended(&mut self, id: &str) {
        self.started.insert(id.to_owned());
        self.ended.insert(id.to_owned());
    }

    /// The calls that were announced and never ended, for a turn that is ending without them.
    pub fn unfinished(&self) -> Vec<String> {
        self.started.difference(&self.ended).cloned().collect()
    }
}

/// An MCP tool result's `content` array as ACP tool-call content. A block ACP cannot represent
/// is kept as its JSON text rather than dropped; with no blocks at all, `fallback` is the text.
pub fn mcp_content(result: &Value, fallback: &str) -> Vec<ToolCallContent> {
    let mut blocks: Vec<ToolCallContent> = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(
            |item| match serde_json::from_value::<ContentBlock>(item.clone()) {
                Ok(block) => ToolCallContent::from(block),
                Err(_) => ToolCallContent::from(item.to_string()),
            },
        )
        .collect();
    if blocks.is_empty() && !fallback.is_empty() {
        blocks.push(ToolCallContent::from(fallback.to_owned()));
    }
    blocks
}

/// A tool's result of unknown shape as ACP content: an MCP-shaped result as its blocks, a string as
/// text, anything else as `fallback` or JSON text.
pub fn value_content(value: &Value, fallback: &str) -> Vec<ToolCallContent> {
    if value.is_null() {
        return if fallback.is_empty() {
            Vec::new()
        } else {
            vec![ToolCallContent::from(fallback.to_owned())]
        };
    }
    if value.get("content").is_some_and(Value::is_array) {
        return mcp_content(value, fallback);
    }
    if let Some(text) = value.as_str() {
        return vec![ToolCallContent::from(text.to_owned())];
    }
    let text = if fallback.is_empty() {
        value.to_string()
    } else {
        fallback.to_owned()
    };
    vec![ToolCallContent::from(text)]
}
