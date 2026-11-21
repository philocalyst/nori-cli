//! Calls to Nori's delegated-task tools.
//!
//! An agent built into Nori names the tool in the call's `_meta`. Over the `nori-client` MCP
//! server, agents name tools their own way (`mcp__nori-client__delegate_task`,
//! `nori-client.delegate_task`, `delegate_task (nori-client MCP Server)`), so such a call is
//! recognized by the server's name and a tool name appearing in its title. What a recognized call did is
//! shown in words; the child's work itself is shown as a subagent, not under the call.

use serde::Deserialize;
use serde::Serialize;

/// One call to a delegated-task tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum DelegationCall {
    Delegate {
        /// What the new actor is called, when the call names it.
        #[serde(default)]
        alias: Option<String>,
        agent: Option<String>,
        model: Option<String>,
        title: String,
    },
    FollowUp {
        task_id: String,
    },
    Wait {
        task_ids: Vec<String>,
    },
    Read {
        task_id: String,
    },
    Cancel {
        task_id: String,
    },
    List,
    Capabilities,
}

impl DelegationCall {
    /// The call `title` names, with its arguments from `input`, when it is one.
    pub(crate) fn recognize(title: &str, input: Option<&serde_json::Value>) -> Option<Self> {
        let lowered = title.to_ascii_lowercase();
        if !lowered.contains("nori-client") && !lowered.contains("nori_client") {
            return None;
        }
        // `__` separates the parts of Claude's names (`mcp__server__tool`).
        let words = lowered.replace("__", " ");
        let tool = words
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .find(|word| TOOLS.contains(word))?;
        Self::from_tool(tool, input)
    }

    /// The call to Nori's tool `tool`, with its arguments from `input`, when it is one of these.
    pub(crate) fn from_tool(tool: &str, input: Option<&serde_json::Value>) -> Option<Self> {
        let text = |key: &str| {
            input
                .and_then(|input| input.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        // The canonical argument is `target`; the others are accepted as synonyms.
        let target = || {
            ["target", "to", "actor", "recipient", "task_id", "id"]
                .iter()
                .find_map(|key| text(key))
        };
        Some(match tool {
            "delegate_task" => Self::Delegate {
                alias: text("alias").filter(|alias| !alias.trim().is_empty()),
                agent: text("agent"),
                model: text("model"),
                title: text("title")
                    .or_else(|| {
                        text("task").map(|task| {
                            task.lines()
                                .map(str::trim)
                                .find(|line| !line.is_empty())
                                .unwrap_or_default()
                                .to_owned()
                        })
                    })
                    .unwrap_or_default(),
            },
            "task_send" => Self::FollowUp {
                task_id: target().unwrap_or_default(),
            },
            "task_wait" => Self::Wait {
                task_ids: input
                    .and_then(|input| input.get("targets").or_else(|| input.get("task_ids")))
                    .and_then(serde_json::Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            "task_status" => Self::Read {
                task_id: target().unwrap_or_default(),
            },
            "task_cancel" => Self::Cancel {
                task_id: target().unwrap_or_default(),
            },
            "task_list" => Self::List,
            "agent_capabilities" => Self::Capabilities,
            _ => return None,
        })
    }

    /// What the call does, in the tense of `active` (still running) or done.
    pub(crate) fn header(&self, active: bool) -> String {
        let (now, done) = match self {
            Self::Delegate {
                alias,
                agent,
                model,
                title,
            } => {
                let route = match (agent, model) {
                    (Some(agent), Some(model)) => Some(format!("{agent} · {model}")),
                    (Some(agent), None) => Some(agent.clone()),
                    (None, Some(model)) => Some(model.clone()),
                    (None, None) => None,
                };
                // The actor is named the way every other view names it: by its alias, with what
                // runs it beside.
                let target = match (alias, route) {
                    (Some(alias), Some(route)) => format!(" to {alias} ({route})"),
                    (Some(alias), None) => format!(" to {alias}"),
                    (None, Some(route)) => format!(" to {route}"),
                    (None, None) => String::new(),
                };
                let what = if title.is_empty() {
                    String::new()
                } else {
                    format!(": {title}")
                };
                return format!(
                    "{}{target}{what}",
                    if active { "Delegating" } else { "Delegated" }
                );
            }
            Self::FollowUp { task_id } => (
                format!("Messaging {task_id}"),
                format!("Messaged {task_id}"),
            ),
            Self::Wait { task_ids } => {
                let ids = task_ids.join(", ");
                (format!("Waiting for {ids}"), format!("Waited for {ids}"))
            }
            Self::Read { task_id } => (format!("Checking {task_id}"), format!("Checked {task_id}")),
            Self::Cancel { task_id } => (
                format!("Cancelling {task_id}"),
                format!("Cancelled {task_id}"),
            ),
            Self::List => (
                "Listing delegated tasks".to_owned(),
                "Listed delegated tasks".to_owned(),
            ),
            Self::Capabilities => (
                "Listing agents to delegate to".to_owned(),
                "Listed agents to delegate to".to_owned(),
            ),
        };
        if active { now } else { done }
    }
}

const TOOLS: [&str; 7] = [
    "delegate_task",
    "task_send",
    "task_wait",
    "task_status",
    "task_cancel",
    "task_list",
    "agent_capabilities",
];

/// The one-line outcome of a finished call, from the JSON the tool returned: each task's id and
/// status, or the tool's error as it is.
pub(crate) fn outcome(result: &str) -> Option<String> {
    let value: serde_json::Value = match serde_json::from_str(result.trim()) {
        Ok(value) => value,
        Err(_) => {
            let line = result
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())?;
            return Some(line.to_owned());
        }
    };
    // What happened, not the tool's internal task ids: those mean nothing to the reader.
    let describe = |task: &serde_json::Value| {
        let status = task.get("status")?.as_str()?;
        Some(
            match status {
                "starting" | "queued" | "pending" => "starting",
                "running" => "running",
                other => other,
            }
            .to_owned(),
        )
    };
    if let Some(tasks) = value.get("tasks").and_then(serde_json::Value::as_array) {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for status in tasks.iter().filter_map(describe) {
            match counts.iter_mut().find(|(seen, _)| *seen == status) {
                Some((_, count)) => *count += 1,
                None => counts.push((status, 1)),
            }
        }
        return Some(if counts.is_empty() {
            "no tasks".to_owned()
        } else {
            counts
                .into_iter()
                .map(|(status, count)| format!("{count} {status}"))
                .collect::<Vec<_>>()
                .join(" · ")
        });
    }
    if let Some(agents) = value.get("agents").and_then(serde_json::Value::as_array) {
        return Some(format!("{} agents", agents.len()));
    }
    describe(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn calls_are_recognized_however_the_agent_names_them() {
        let input = serde_json::json!({"task": "Read a.txt\nthen report", "agent": "codex"});
        for title in [
            "mcp__nori-client__delegate_task",
            "nori-client.delegate_task",
            "nori-client/delegate_task",
            "delegate_task (nori-client MCP Server)",
        ] {
            assert_eq!(
                DelegationCall::recognize(title, Some(&input)),
                Some(DelegationCall::Delegate {
                    alias: None,
                    agent: Some("codex".to_owned()),
                    model: None,
                    title: "Read a.txt".to_owned(),
                }),
                "{title}"
            );
        }
        assert_eq!(
            DelegationCall::recognize("delegate_task", Some(&input)),
            None
        );
        assert_eq!(
            DelegationCall::recognize("mcp__other__task_list", None),
            None
        );
    }

    #[test]
    fn headers_and_outcomes_read_as_sentences() {
        let call = DelegationCall::Delegate {
            alias: None,
            agent: Some("codex".to_owned()),
            model: Some("gpt-5.1".to_owned()),
            title: "Read a.txt".to_owned(),
        };
        assert_eq!(
            call.header(true),
            "Delegating to codex · gpt-5.1: Read a.txt"
        );
        assert_eq!(
            call.header(false),
            "Delegated to codex · gpt-5.1: Read a.txt"
        );
        assert_eq!(
            outcome(r#"{"task_id":"task-1","status":"running","agent":"codex"}"#).as_deref(),
            Some("running")
        );
        assert_eq!(
            outcome(r#"{"tasks":[{"task_id":"task-1","status":"completed"},{"task_id":"task-2","status":"failed"}]}"#)
                .as_deref(),
            Some("1 completed · 1 failed")
        );
        assert_eq!(
            outcome("this agent has no model `x`\nmore").as_deref(),
            Some("this agent has no model `x`")
        );
    }
}
