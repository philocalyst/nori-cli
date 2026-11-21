//! The model's own task list, shown as the client's plan.

use async_trait::async_trait;
use nori_acp_kit::schema::PlanEntry;
use nori_acp_kit::schema::PlanEntryPriority;
use nori_acp_kit::schema::PlanEntryStatus;
use nori_acp_kit::schema::ToolKind;
use nori_llm::ToolSpec;
use serde_json::Value;
use serde_json::json;

use super::Invocation;
use super::Tool;
use super::ToolContext;
use super::ToolOutput;

/// The arguments a plan tool takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStyle {
    /// OpenCode's `todowrite`: `todos: [{content, status, priority, id}]`.
    TodoWrite,
    /// Codex's `update_plan`: `explanation`, `plan: [{step, status}]`.
    UpdatePlan,
}

pub struct Todo {
    pub style: TodoStyle,
    pub name: String,
    pub description: String,
}

impl Todo {
    pub fn new(style: TodoStyle) -> Self {
        let (name, description) = match style {
            TodoStyle::TodoWrite => (
                "todowrite",
                "Write the task list for the current work: the whole list every time, each item with `content`, `status` (`pending`, `in_progress`, `completed`, `cancelled`) and `priority` (`high`, `medium`, `low`). Use it for work of three or more steps: plan it, keep exactly one item `in_progress`, and mark items `completed` as soon as they are done. Skip it for a single, simple task.",
            ),
            TodoStyle::UpdatePlan => (
                "update_plan",
                "Update the task plan: give the whole plan every time, each step with a `status` of `pending`, `in_progress` or `completed`, at most one `in_progress`. Optionally explain the change in `explanation`. Use it for non-trivial multi-step work; skip it for simple tasks.",
            ),
        };
        Self {
            style,
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    fn entries(&self, input: &Value) -> Result<Vec<PlanEntry>, String> {
        let (list, text_key) = match self.style {
            TodoStyle::TodoWrite => ("todos", "content"),
            TodoStyle::UpdatePlan => ("plan", "step"),
        };
        let items = input[list]
            .as_array()
            .ok_or_else(|| format!("The `{list}` argument must be a list."))?;
        items
            .iter()
            .map(|item| {
                let text = item[text_key]
                    .as_str()
                    .ok_or_else(|| format!("Every item needs a `{text_key}` string."))?;
                let (status, text) = match item["status"].as_str().unwrap_or("pending") {
                    "in_progress" => (PlanEntryStatus::InProgress, text.to_owned()),
                    "completed" => (PlanEntryStatus::Completed, text.to_owned()),
                    "cancelled" => (PlanEntryStatus::Completed, format!("{text} (cancelled)")),
                    _ => (PlanEntryStatus::Pending, text.to_owned()),
                };
                let priority = match item["priority"].as_str() {
                    Some("high") => PlanEntryPriority::High,
                    Some("low") => PlanEntryPriority::Low,
                    _ => PlanEntryPriority::Medium,
                };
                Ok(PlanEntry::new(text, priority, status))
            })
            .collect()
    }
}

#[async_trait]
impl Tool for Todo {
    fn spec(&self) -> ToolSpec {
        let schema = match self.style {
            TodoStyle::TodoWrite => json!({
                "type": "object",
                "properties": {"todos": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                        "priority": {"type": "string", "enum": ["high", "medium", "low"]},
                        "id": {"type": "string"},
                    },
                    "required": ["content", "status"],
                }}},
                "required": ["todos"],
            }),
            TodoStyle::UpdatePlan => json!({
                "type": "object",
                "properties": {
                    "explanation": {"type": "string"},
                    "plan": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "step": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                        },
                        "required": ["step", "status"],
                    }},
                },
                "required": ["plan"],
            }),
        };
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: schema,
        }
    }

    async fn prepare(&self, input: &Value, _context: &ToolContext) -> Result<Invocation, String> {
        self.entries(input)?;
        Ok(Invocation::new("Update plan", ToolKind::Think))
    }

    async fn run(&self, input: Value, _context: &ToolContext) -> ToolOutput {
        match self.entries(&input) {
            Ok(entries) => {
                let open = entries
                    .iter()
                    .filter(|entry| entry.status != PlanEntryStatus::Completed)
                    .count();
                ToolOutput {
                    plan: Some(entries),
                    ..ToolOutput::text(format!("Plan updated: {open} items open."))
                }
            }
            Err(error) => ToolOutput::error(error),
        }
    }
}
