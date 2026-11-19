//! Nori's host tools as tool cards: delegation and messaging between actors.

use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use serde_json::Value;
use serde_json::json;

use super::Ctx;
use super::Run;
use crate::record::Record;
use crate::world::DelegateStep;
use crate::world::HostToolStep;
use crate::world::SendStep;

impl Ctx {
    fn render_json(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.render(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.render_json(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), self.render_json(value)))
                    .collect(),
            ),
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        }
    }

    /// Run host tool `step.name` through Nori, shown as a tool card.
    pub(super) async fn host_tool(&self, step: &HostToolStep) -> Run {
        let id = self.tool_id(step.id.as_deref());
        let arguments = self.render_json(&step.arguments);
        let title = step.title.clone().unwrap_or_else(|| step.name.clone());
        let Some(host) = self.live.host.clone() else {
            self.diverge(format!(
                "{} cannot call host tool {:?}: Nori gave this session no host tools (the agent is served \
                 to an external client, or declares host_tools = false)",
                self.agent(),
                step.name
            ));
            self.emit(SessionUpdate::ToolCall(
                ToolCall::new(id, title)
                    .status(ToolCallStatus::Failed)
                    .raw_input(arguments),
            ))?;
            return Ok(());
        };
        self.emit(SessionUpdate::ToolCall(
            ToolCall::new(id.clone(), title)
                .status(ToolCallStatus::InProgress)
                .raw_input(arguments.clone())
                .meta(host.call_meta(&step.name)),
        ))?;
        let output = self
            .outside(host.call(&step.name, arguments.clone()))
            .await?;
        self.record(Record::HostTool {
            agent: self.agent().to_owned(),
            session: self.session_id().to_owned(),
            name: step.name.clone(),
            arguments,
            output: output.text.clone(),
            is_error: output.is_error,
        });
        if output.is_error != step.expect_error {
            self.diverge(format!(
                "host tool {} {} (the script expected it to {}): {}",
                step.name,
                if output.is_error {
                    "failed"
                } else {
                    "succeeded"
                },
                if step.expect_error { "fail" } else { "succeed" },
                output.text
            ));
        }
        if let Some(bind) = &step.bind {
            let value =
                serde_json::from_str::<Value>(&output.text).unwrap_or_else(|_| json!(output.text));
            self.live.record.state().vars.insert(bind.clone(), value);
        }
        let (content, raw_output) = nori_acp_kit::output_payload(&output);
        self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id,
            ToolCallUpdateFields::new()
                .status(if output.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .content(Some(content))
                .raw_output(Some(raw_output)),
        )))
    }

    /// `delegate_task`.
    pub(super) async fn delegate(&self, step: &DelegateStep) -> Run {
        let mut arguments = serde_json::Map::new();
        arguments.insert("task".to_owned(), json!(self.text(&step.task)));
        for (key, value) in [
            ("agent", &step.agent),
            ("model", &step.model),
            ("title", &step.title),
            ("alias", &step.alias),
            ("role", &step.role),
            ("mode", &step.mode),
            ("isolation", &step.isolation),
        ] {
            if let Some(value) = value {
                arguments.insert(key.to_owned(), json!(value));
            }
        }
        self.host_tool(&HostToolStep {
            name: "delegate_task".to_owned(),
            arguments: Value::Object(arguments),
            title: step
                .title
                .clone()
                .map(|title| format!("delegate_task: {title}")),
            bind: step.bind.clone(),
            ..HostToolStep::default()
        })
        .await
    }

    /// `task_send`.
    pub(super) async fn send(&self, step: &SendStep) -> Run {
        let mut arguments = serde_json::Map::new();
        arguments.insert("to".to_owned(), json!(step.to));
        arguments.insert("message".to_owned(), json!(self.text(&step.message)));
        if step.steer {
            arguments.insert("steer".to_owned(), json!(true));
        }
        if let Some(mode) = &step.mode {
            arguments.insert("mode".to_owned(), json!(mode));
        }
        self.host_tool(&HostToolStep {
            name: "task_send".to_owned(),
            arguments: Value::Object(arguments),
            title: Some(format!("task_send: {}", step.to)),
            bind: step.bind.clone(),
            ..HostToolStep::default()
        })
        .await
    }
}
