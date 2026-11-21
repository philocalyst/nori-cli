//! A child session the agent reports itself, as Claude Code and Codex do: announced on the
//! parent, its own activity on its own session id, its state at the end. A launcher shows the
//! parent's tool call that started it (`Task`, `spawn_agent`).

use nori_acp_kit::Sink;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;
use serde_json::Value;
use serde_json::json;

use super::Ctx;
use super::Lane;
use super::Run;
use crate::world::Subagent;
use crate::world::SubagentEnd;
use crate::world::SubagentSpelling;

impl Ctx {
    pub(super) async fn subagent(&self, subagent: &Subagent) -> Run {
        let child = match &subagent.id {
            Some(id) => self.render(id),
            None => {
                let mut state = self.live.record.state();
                state.subagents += 1;
                format!("{}/sub-{}", self.session_id(), state.subagents)
            }
        };
        let child_id = SessionId::new(child.clone());
        let parent = self.lane.id.clone();
        let description = subagent.description.as_ref().map(|text| self.render(text));
        let launched = match &subagent.launcher {
            Some(launcher) => {
                let id = self.tool_id(launcher.id.as_deref());
                let title = match &launcher.title {
                    Some(title) => self.render(title),
                    None => description
                        .clone()
                        .unwrap_or_else(|| self.render(&subagent.title)),
                };
                let input = json!({
                    "description": description,
                    "prompt": subagent.prompt.as_ref().map(|prompt| self.text(prompt)),
                    "subagent_type": self.render(&subagent.title),
                });
                self.emit(SessionUpdate::ToolCall(
                    ToolCall::new(id.clone(), title)
                        .kind(ToolKind::Other)
                        .status(ToolCallStatus::InProgress)
                        .raw_input(Some(input)),
                ))?;
                Some((id, launcher.background))
            }
            None => None,
        };
        let model = subagent.model.as_ref().map(|model| self.render(model));
        let name = subagent.name.as_ref().map(|name| self.render(name));
        let title = self.render(&subagent.title);
        let announcement = match subagent.wire {
            SubagentSpelling::Claude => {
                let mut update = json!({
                    "sessionUpdate": "subagent_update",
                    "sessionId": child,
                    "title": title,
                    "state": {"state": "running"},
                });
                if let Some(name) = &name {
                    update["name"] = json!(name);
                }
                if let Some(description) = &description {
                    update["description"] = json!(description);
                }
                update
            }
            SubagentSpelling::Codex => {
                let mut update = json!({
                    "sessionUpdate": "subagent_spawned",
                    "subagentSessionId": child,
                    "name": title,
                    "state": "running",
                });
                if let Some(name) = &name {
                    update["nickname"] = json!(name);
                }
                if let Some(description) = &description {
                    update["task"] = json!(description);
                }
                update
            }
        };
        let mut announcement = announcement;
        if let Some(model) = &model {
            announcement["model"] = json!(model);
        }
        self.emit_raw(&parent, announcement)?;
        if let Some(prompt) = &subagent.prompt {
            let mut message = json!({
                "sessionUpdate": "session_message",
                "senderSessionId": parent,
                "recipientSessionId": child_id,
                "content": {"type": "text", "text": self.text(prompt)},
            });
            if let Some((id, _)) = &launched {
                message["messageId"] = json!(id);
            }
            self.emit_raw(&child_id, message)?;
        }
        if let Some((id, true)) = &launched {
            self.finish_launcher(
                id,
                ToolCallStatus::Completed,
                json!({"status": "async_launched", "agentId": child}),
            )?;
        }
        let lane = Self {
            lane: Lane {
                sink: Sink::new(self.lane.sink.cx(), child_id.clone()),
                id: child_id.clone(),
                recorded: false,
            },
            ..self.clone()
        };
        lane.run(&subagent.steps).await?;
        let state: Option<Value> = match (subagent.wire, subagent.ends) {
            (_, SubagentEnd::Running) => None,
            (SubagentSpelling::Claude, SubagentEnd::Idle) => {
                Some(json!({"state": "turn_idle", "stopReason": "end_turn"}))
            }
            (SubagentSpelling::Claude, SubagentEnd::Completed) => {
                Some(json!({"state": "completed", "stopReason": "end_turn"}))
            }
            (SubagentSpelling::Claude, SubagentEnd::Failed) => {
                Some(json!({"state": "failed", "stopReason": subagent.reason}))
            }
            (SubagentSpelling::Claude, SubagentEnd::Cancelled) => {
                Some(json!({"state": "cancelled"}))
            }
            (SubagentSpelling::Claude, SubagentEnd::Closed) => Some(json!({"state": "closed"})),
            (SubagentSpelling::Codex, SubagentEnd::Idle | SubagentEnd::Completed) => {
                Some(json!("completed"))
            }
            (SubagentSpelling::Codex, SubagentEnd::Failed) => Some(json!("failed")),
            (SubagentSpelling::Codex, SubagentEnd::Cancelled) => Some(json!("cancelled")),
            (SubagentSpelling::Codex, SubagentEnd::Closed) => Some(json!("closed")),
        };
        if let Some(state) = state {
            let update = match subagent.wire {
                SubagentSpelling::Claude => {
                    json!({"sessionUpdate": "subagent_update", "sessionId": child, "state": state})
                }
                SubagentSpelling::Codex => json!({
                    "sessionUpdate": "subagent_state_update",
                    "subagentSessionId": child,
                    "state": state,
                }),
            };
            self.emit_raw(&parent, update)?;
        }
        if let Some((id, false)) = &launched
            && subagent.ends != SubagentEnd::Running
        {
            let status = match subagent.ends {
                SubagentEnd::Failed | SubagentEnd::Cancelled => ToolCallStatus::Failed,
                SubagentEnd::Running
                | SubagentEnd::Idle
                | SubagentEnd::Completed
                | SubagentEnd::Closed => ToolCallStatus::Completed,
            };
            self.finish_launcher(
                id,
                status,
                json!({"agentId": child, "reason": subagent.reason}),
            )?;
        }
        Ok(())
    }

    fn finish_launcher(&self, id: &str, status: ToolCallStatus, output: Value) -> Run {
        self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id.to_owned(),
            ToolCallUpdateFields::new()
                .status(status)
                .raw_output(Some(output)),
        )))
    }
}
