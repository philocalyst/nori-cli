//! Subagent sessions.
//!
//! Agents that implement the ACP subagents proposal announce each child session on its parent
//! session and then deliver the child's own `session/update`s on the same connection under the
//! child's session id. The pinned ACP schema has no types for the announcements, so they are
//! recognized at the JSON level ([`parse_subagent_update`]) and carried as the types here.
//!
//! Two spellings exist and both map onto [`SubagentUpdate`]:
//!
//! * `subagent_update` (Claude): patch semantics, an omitted field is unchanged and `null`
//!   clears it.
//! * `subagent_spawned` / `subagent_state_update` (Codex): a spawn announces name and task, a
//!   state update carries only the state.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::acp::v1::SessionId;

/// A field of an announcement: unchanged, cleared, or set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "patch", content = "value", rename_all = "snake_case")]
pub enum Patch<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T> Patch<T> {
    /// Apply this patch to `current`.
    pub fn apply_to(self, current: &mut Option<T>) {
        match self {
            Self::Keep => {}
            Self::Clear => *current = None,
            Self::Set(value) => *current = Some(value),
        }
    }
}

/// How a subagent's work ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SubagentOutcome {
    /// The child finished its work (RFD `idle`, Codex `completed`).
    Completed {
        stop_reason: Option<String>,
    },
    Failed {
        reason: Option<String>,
    },
    Cancelled,
    /// The session itself was closed and cannot receive another turn.
    Closed,
}

/// Where a subagent is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubagentState {
    Running,
    /// The current turn finished, but this session can receive more work.
    Idle,
    Ended(SubagentOutcome),
}

impl SubagentState {
    /// Read a state from either spelling: the RFD's `{ "state": "idle", "stopReason": .. }`
    /// object or Codex's plain string.
    fn from_wire(value: &Value) -> Option<Self> {
        let (name, stop_reason) = match value {
            Value::String(name) => (name.as_str(), None),
            Value::Object(object) => (
                object.get("state")?.as_str()?,
                object
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ),
            _ => return None,
        };
        Some(match name {
            "running" | "working" | "pending" => Self::Running,
            "turn_idle" => Self::Idle,
            "failed" | "errored" | "error" | "disconnected" | "timed_out" => {
                Self::Ended(SubagentOutcome::Failed {
                    reason: stop_reason,
                })
            }
            "closed" => Self::Ended(SubagentOutcome::Closed),
            "cancelled" | "canceled" | "stopped" => Self::Ended(SubagentOutcome::Cancelled),
            _ => Self::Ended(SubagentOutcome::Completed { stop_reason }),
        })
    }
}

/// An announcement of a child session, or a change to one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentUpdate {
    /// The session the child was announced on.
    pub parent_session_id: SessionId,
    /// The child's own session id.
    pub session_id: SessionId,
    /// Canonical provider slug, when known by the owning backend.
    #[serde(default)]
    pub agent: Patch<String>,
    /// Provider/model identifier reported by the child backend, when known.
    #[serde(default)]
    pub model: Patch<String>,
    pub title: Patch<String>,
    pub description: Patch<String>,
    pub state: Patch<SubagentState>,
}

/// How far a peer message got. Providers' own session messages carry no state (`None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDelivery {
    /// Accepted into the recipient's mailbox; no turn of the recipient has read it.
    Queued,
    /// A turn of the recipient started with the message in its input.
    Delivered,
    /// The recipient's turn that contained it ended, or the recipient replied to the sender.
    Acknowledged,
    /// It will never be delivered: its task failed or was cancelled before a turn read it, or the
    /// recipient ended first. Terminal; `SubagentMessage::failure` says why.
    Undeliverable,
}

/// A message exchanged between sessions, e.g. the prompt a parent sent to a child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentMessage {
    /// The session the message was delivered on.
    pub session_id: SessionId,
    /// Correlates chunks of one message. For a parent's prompt to a child this is the id of the
    /// parent's tool call that launched the child.
    pub message_id: Option<String>,
    pub sender_session_id: Option<SessionId>,
    pub recipient_session_id: Option<SessionId>,
    pub text: String,
    /// The text continues an earlier message with the same id.
    pub chunk: bool,
    /// Display names of the two parties (actor alias, or agent name), when the sender of the
    /// message knows them.
    #[serde(default)]
    pub sender_label: Option<String>,
    #[serde(default)]
    pub recipient_label: Option<String>,
    /// Delivery state of a Nori peer message; the same `message_id` is re-announced as it
    /// advances.
    #[serde(default)]
    pub delivery: Option<MessageDelivery>,
    /// Why the message is `Undeliverable`.
    #[serde(default)]
    pub failure: Option<String>,
}

/// What the subagent-related `session/update` kinds decode to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentWire {
    Update(SubagentUpdate),
    Message(SubagentMessage),
}

/// Decode the `params` of a `session/update` notification when it is a subagent announcement.
///
/// Only the envelope is read; the typed notification parser never sees these updates, and every
/// other update is left untouched (`None`).
pub fn parse_subagent_update(params: &Value) -> Option<SubagentWire> {
    let delivered_on = params.get("sessionId")?.as_str()?;
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate")?.as_str()?;
    let text_field = |name: &str| -> Patch<String> {
        match update.get(name) {
            None => Patch::Keep,
            Some(Value::Null) => Patch::Clear,
            Some(Value::String(text)) => Patch::Set(text.clone()),
            Some(_) => Patch::Keep,
        }
    };
    let state_field = || -> Patch<SubagentState> {
        match update.get("state") {
            None | Some(Value::Null) => Patch::Keep,
            Some(value) => SubagentState::from_wire(value).map_or(Patch::Keep, Patch::Set),
        }
    };
    let session_field = |name: &str| -> Option<SessionId> {
        update.get(name).and_then(Value::as_str).map(SessionId::new)
    };
    match kind {
        "subagent_update" => Some(SubagentWire::Update(SubagentUpdate {
            parent_session_id: SessionId::new(delivered_on),
            session_id: session_field("sessionId")?,
            agent: Patch::Keep,
            model: text_field("model"),
            title: text_field("title"),
            description: text_field("description"),
            state: state_field(),
        })),
        "subagent_spawned" => Some(SubagentWire::Update(SubagentUpdate {
            parent_session_id: SessionId::new(delivered_on),
            session_id: session_field("subagentSessionId")?,
            agent: Patch::Keep,
            model: text_field("model"),
            title: text_field("name"),
            description: text_field("task"),
            state: Patch::Set(SubagentState::Running),
        })),
        "subagent_state_update" => Some(SubagentWire::Update(SubagentUpdate {
            parent_session_id: SessionId::new(delivered_on),
            session_id: session_field("subagentSessionId")?,
            agent: Patch::Keep,
            model: text_field("model"),
            title: Patch::Keep,
            description: Patch::Keep,
            state: state_field(),
        })),
        "session_message" | "session_message_chunk" => {
            Some(SubagentWire::Message(SubagentMessage {
                session_id: SessionId::new(delivered_on),
                message_id: update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                sender_session_id: session_field("senderSessionId"),
                recipient_session_id: session_field("recipientSessionId"),
                text: update.get("content").map(message_text).unwrap_or_default(),
                chunk: kind == "session_message_chunk",
                sender_label: None,
                recipient_label: None,
                delivery: None,
                failure: None,
            }))
        }
        _ => None,
    }
}

/// The text of a message's content: a content block, or an array of them.
fn message_text(content: &Value) -> String {
    match content {
        Value::Array(blocks) => blocks
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(block) => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Value::String(text) => text.clone(),
        _ => String::new(),
    }
}

/// What the Claude adapter's `_meta.claudeCode` says about a root-session update, for agents that
/// show subagents without announcing them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeToolMeta<'a> {
    /// The tool call that launched the subagent this update belongs to (`parentToolUseId`).
    pub parent_tool_use_id: Option<&'a str>,
    /// The call launches a subagent (`toolName` is `Agent` or `Task`).
    pub launches_subagent: bool,
    /// The call reported completion at launch while the subagent keeps working
    /// (`toolResponse.status == "async_launched"`).
    pub async_launch: bool,
}

impl<'a> ClaudeToolMeta<'a> {
    pub fn read(meta: Option<&'a serde_json::Map<String, Value>>) -> Self {
        let Some(code) = meta.and_then(|meta| meta.get("claudeCode")) else {
            return Self::default();
        };
        let response = code.get("toolResponse");
        Self {
            parent_tool_use_id: code.get("parentToolUseId").and_then(Value::as_str),
            launches_subagent: matches!(
                code.get("toolName").and_then(Value::as_str),
                Some("Agent" | "Task")
            ),
            async_launch: response.is_some_and(|response| {
                response.get("status").and_then(Value::as_str) == Some("async_launched")
                    || response.get("isAsync").and_then(Value::as_bool) == Some(true)
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn older_announcements_deserialize_without_provider() {
        let update: SubagentUpdate = serde_json::from_value(json!({
            "parent_session_id": "parent",
            "session_id": "child",
            "title": {"patch": "keep"},
            "description": {"patch": "keep"},
            "state": {"patch": "keep"}
        }))
        .expect("older announcement");
        assert_eq!(update.agent, Patch::Keep);
        assert_eq!(update.model, Patch::Keep);
    }

    #[test]
    fn claude_announcement_with_patch_semantics() {
        let announce = json!({
            "sessionId": "root",
            "update": {
                "sessionUpdate": "subagent_update",
                "sessionId": "child",
                "model": "anthropic/claude-sonnet-4",
                "title": "general-purpose",
                "description": "Read a.txt contents",
                "capabilities": {"cancel": {}},
                "state": {"state": "running"}
            }
        });
        assert_eq!(
            parse_subagent_update(&announce),
            Some(SubagentWire::Update(SubagentUpdate {
                parent_session_id: SessionId::new("root"),
                session_id: SessionId::new("child"),
                agent: Patch::Keep,
                model: Patch::Set("anthropic/claude-sonnet-4".into()),
                title: Patch::Set("general-purpose".into()),
                description: Patch::Set("Read a.txt contents".into()),
                state: Patch::Set(SubagentState::Running),
            }))
        );

        let finished = json!({
            "sessionId": "root",
            "update": {
                "sessionUpdate": "subagent_update",
                "sessionId": "child",
                "description": null,
                "state": {"state": "idle", "stopReason": "end_turn"}
            }
        });
        let Some(SubagentWire::Update(update)) = parse_subagent_update(&finished) else {
            panic!("not an update");
        };
        assert_eq!(update.title, Patch::Keep);
        assert_eq!(update.description, Patch::Clear);
        assert_eq!(
            update.state,
            Patch::Set(SubagentState::Ended(SubagentOutcome::Completed {
                stop_reason: Some("end_turn".into())
            }))
        );
    }

    #[test]
    fn codex_spellings() {
        let spawned = json!({
            "sessionId": "root",
            "update": {
                "sessionUpdate": "subagent_spawned",
                "subagentSessionId": "thread:generation:2",
                "name": "explorer",
                "task": "look around",
                "capabilities": {}
            }
        });
        let Some(SubagentWire::Update(update)) = parse_subagent_update(&spawned) else {
            panic!("not an update");
        };
        assert_eq!(update.session_id, SessionId::new("thread:generation:2"));
        assert_eq!(update.title, Patch::Set("explorer".into()));
        assert_eq!(update.description, Patch::Set("look around".into()));
        assert_eq!(update.state, Patch::Set(SubagentState::Running));

        for (wire, expected) in [
            (
                "completed",
                SubagentOutcome::Completed { stop_reason: None },
            ),
            ("failed", SubagentOutcome::Failed { reason: None }),
            ("cancelled", SubagentOutcome::Cancelled),
            ("closed", SubagentOutcome::Closed),
            ("disconnected", SubagentOutcome::Failed { reason: None }),
        ] {
            let state = json!({
                "sessionId": "root",
                "update": {
                    "sessionUpdate": "subagent_state_update",
                    "subagentSessionId": "c",
                    "state": wire
                }
            });
            let Some(SubagentWire::Update(update)) = parse_subagent_update(&state) else {
                panic!("not an update");
            };
            assert_eq!(
                update.state,
                Patch::Set(SubagentState::Ended(expected)),
                "{wire}"
            );
        }
    }

    #[test]
    fn session_messages_carry_their_link() {
        let message = json!({
            "sessionId": "child",
            "update": {
                "sessionUpdate": "session_message",
                "messageId": "toolu_1",
                "senderSessionId": "root",
                "recipientSessionId": "child",
                "content": [{"type": "text", "text": "Read the file"}]
            }
        });
        assert_eq!(
            parse_subagent_update(&message),
            Some(SubagentWire::Message(SubagentMessage {
                session_id: SessionId::new("child"),
                message_id: Some("toolu_1".into()),
                sender_session_id: Some(SessionId::new("root")),
                recipient_session_id: Some(SessionId::new("child")),
                text: "Read the file".into(),
                chunk: false,
                sender_label: None,
                recipient_label: None,
                delivery: None,
                failure: None,
            }))
        );
    }

    #[test]
    fn legacy_opencode_tool_metadata_is_not_a_lifecycle_event() {
        for kind in ["tool_call", "tool_call_update"] {
            let update = json!({"sessionId":"root", "update":{
                "sessionUpdate":kind, "toolCallId":"call", "status":"completed",
                "_meta":{"opencode/child-session":{"id":"child","parentID":"root"}}
            }});
            assert_eq!(parse_subagent_update(&update), None);
        }
    }

    #[test]
    fn other_updates_are_left_alone() {
        let chunk = json!({
            "sessionId": "root",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}
        });
        assert_eq!(parse_subagent_update(&chunk), None);
        assert_eq!(parse_subagent_update(&json!({"sessionId": "root"})), None);
    }

    #[test]
    fn claude_tool_meta() {
        let meta = json!({
            "claudeCode": {
                "parentToolUseId": "toolu_1",
                "toolName": "Task",
                "toolResponse": {"status": "async_launched"}
            }
        });
        assert_eq!(
            ClaudeToolMeta::read(meta.as_object()),
            ClaudeToolMeta {
                parent_tool_use_id: Some("toolu_1"),
                launches_subagent: true,
                async_launch: true,
            }
        );
        let read = json!({"claudeCode": {"toolName": "Read"}});
        assert_eq!(
            ClaudeToolMeta::read(read.as_object()),
            ClaudeToolMeta::default()
        );
        assert_eq!(ClaudeToolMeta::read(None), ClaudeToolMeta::default());
    }
}
