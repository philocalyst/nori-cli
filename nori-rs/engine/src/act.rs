//! Running the tool calls of one model answer: the card, the permission question, the run, the
//! result the model reads.

use nori_acp_kit::PermissionDecision;
use nori_acp_kit::schema::PermissionOption;
use nori_acp_kit::schema::PermissionOptionKind;
use nori_acp_kit::schema::Plan;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallLocation;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_llm::Part;
use nori_llm::ToolCall;
use nori_llm::ToolResult;
use nori_llm::ToolResultPart;

use crate::flavor::Flavor;
use crate::rules::Effect;
use crate::rules::Rule;
use crate::tools::Check;
use crate::tools::Invocation;
use crate::tools::Tool;
use crate::tools::ToolContext;
use crate::tools::ToolOutput;
use crate::turn::TurnEnv;

const ALLOW_ONCE: &str = "allow_once";
const ALLOW_ALWAYS: &str = "allow_always";
const REJECT: &str = "reject_once";

/// The results of a round of tool calls.
pub(crate) struct Acted {
    pub(crate) results: Vec<Part>,
    /// The person refused a call: the turn stops so they can say what to do instead.
    pub(crate) rejected: bool,
}

enum Decision {
    Allowed,
    Denied(String),
    Rejected,
    Cancelled,
}

fn finish(update: ToolCallUpdateFields, output: &ToolOutput) -> ToolCallUpdateFields {
    let display = if output.display.is_empty() {
        output
            .result
            .iter()
            .filter_map(|part| match part {
                ToolResultPart::Text { text } if !text.is_empty() => {
                    Some(ToolCallContent::from(text.clone()))
                }
                ToolResultPart::Text { .. } | ToolResultPart::Image { .. } => None,
            })
            .collect()
    } else {
        output.display.clone()
    };
    update
        .status(if output.is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        })
        .content(display)
        .raw_output(output.raw_output.clone())
}

pub(crate) async fn run_calls<F: Flavor>(
    env: &TurnEnv<'_, F>,
    rules: &[Rule],
    calls: Vec<ToolCall>,
) -> Acted {
    let session = env.session;
    let mut context = session.tool_context.clone();
    context.cancel = env.cancel.clone();
    context.images = session
        .workspace()
        .model(&session.dials().model)
        .is_none_or(|model| model.images);
    let mut results = Vec::new();
    let mut rejected = false;
    for call in calls {
        let id = call.id.clone();
        let stopped = if env.cancel.is_cancelled() {
            Some("Cancelled by the user before it ran.")
        } else if rejected {
            Some("Not run: the user rejected an earlier call of this answer.")
        } else {
            None
        };
        if let Some(reason) = stopped {
            let output = ToolOutput::error(reason);
            let _ = env.sink.tool_update(ToolCallUpdate::new(
                id.clone(),
                finish(
                    ToolCallUpdateFields::new().title(call.name.clone()),
                    &output,
                ),
            ));
            results.push(result(&id, output));
            continue;
        }
        let Some(tool) = session.tool(&call.name) else {
            let names: Vec<String> = session
                .tool_list()
                .iter()
                .map(|tool| tool.spec().name)
                .collect();
            let output = ToolOutput::error(format!(
                "There is no tool named {:?}. The tools are: {}.",
                call.name,
                names.join(", ")
            ));
            let _ = env.sink.tool_update(ToolCallUpdate::new(
                id.clone(),
                finish(
                    ToolCallUpdateFields::new().title(call.name.clone()),
                    &output,
                ),
            ));
            results.push(result(&id, output));
            continue;
        };
        let invocation = match tool.prepare(&call.input, &context).await {
            Ok(invocation) => invocation,
            Err(message) => {
                let output = ToolOutput::error(message);
                let _ = env.sink.tool_update(ToolCallUpdate::new(
                    id.clone(),
                    finish(
                        ToolCallUpdateFields::new()
                            .title(call.name.clone())
                            .raw_input(call.input.clone()),
                        &output,
                    ),
                ));
                results.push(result(&id, output));
                continue;
            }
        };
        let fields = || {
            ToolCallUpdateFields::new()
                .title(invocation.title.clone())
                .kind(invocation.kind)
                .locations(
                    invocation
                        .locations
                        .iter()
                        .map(|path| ToolCallLocation::new(path.clone()))
                        .collect::<Vec<_>>(),
                )
                .raw_input(call.input.clone())
        };
        let _ = env.sink.tool_update(
            ToolCallUpdate::new(id.clone(), fields().content(invocation.preview.clone()))
                .meta(invocation.meta.clone()),
        );
        let output = match authorize(env, rules, &invocation, &id, fields()).await {
            Decision::Allowed => {
                let _ = env.sink.tool_update(ToolCallUpdate::new(
                    id.clone(),
                    ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
                ));
                run_tool(tool.as_ref(), call.input.clone(), &context).await
            }
            Decision::Denied(reason) => ToolOutput::error(reason),
            Decision::Rejected => {
                rejected = true;
                ToolOutput::error(
                    "The user rejected this call. Stop and wait for their instructions.",
                )
            }
            Decision::Cancelled => ToolOutput::error("Cancelled by the user before it ran."),
        };
        if let Some(entries) = output.plan.clone() {
            let _ = env.sink.update(SessionUpdate::Plan(Plan::new(entries)));
        }
        let _ = env.sink.tool_update(
            ToolCallUpdate::new(id.clone(), finish(ToolCallUpdateFields::new(), &output))
                .meta(invocation.meta.clone()),
        );
        results.push(result(&id, output));
    }
    Acted { results, rejected }
}

/// Tools that own cancellation must finish their protected work before the turn can end.
async fn run_tool(tool: &dyn Tool, input: serde_json::Value, context: &ToolContext) -> ToolOutput {
    if tool.handles_cancellation() {
        tool.run(input, context).await
    } else {
        tokio::select! {
            output = tool.run(input, context) => output,
            () = context.cancel.cancelled() => ToolOutput::error("Cancelled by the user while it ran."),
        }
    }
}

fn result(id: &str, output: ToolOutput) -> Part {
    Part::ToolResult(ToolResult {
        id: id.to_owned(),
        content: output.result,
        is_error: output.is_error,
    })
}

/// Decide whether a call may run: the rules first, then the person for what the rules ask about.
async fn authorize<F: Flavor>(
    env: &TurnEnv<'_, F>,
    rules: &[Rule],
    invocation: &Invocation,
    id: &str,
    fields: ToolCallUpdateFields,
) -> Decision {
    let mut asks: Vec<&Check> = Vec::new();
    for check in &invocation.checks {
        let mut effect =
            crate::rules::evaluate(rules, &check.action, &check.resource, Effect::Allow);
        // A command known only to read is decided by the rules that name that action, when a
        // mode has any (Codex's presets allow it while other commands ask).
        if effect == Effect::Ask
            && check.action == "shell"
            && crate::safe::reads_only(&check.resource)
        {
            let readers: Vec<Rule> = rules
                .iter()
                .filter(|rule| rule.action == crate::safe::READ_ONLY_SHELL)
                .cloned()
                .collect();
            effect = crate::rules::evaluate(
                &readers,
                crate::safe::READ_ONLY_SHELL,
                &check.resource,
                effect,
            );
        }
        match effect {
            Effect::Allow => {}
            Effect::Ask => asks.push(check),
            Effect::Deny => {
                return Decision::Denied(format!(
                    "The permission rules deny `{}` on {}. Do not retry it; tell the user if it is needed.",
                    check.action, check.resource
                ));
            }
        }
    }
    if asks.is_empty() {
        return Decision::Allowed;
    }
    let options = vec![
        PermissionOption::new(ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            ALLOW_ALWAYS,
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(REJECT, "Reject", PermissionOptionKind::RejectOnce),
    ];
    let question = ToolCallUpdate::new(id.to_owned(), fields.content(invocation.preview.clone()))
        .meta(invocation.meta.clone());
    let answer = tokio::select! {
        answer = env.sink.ask_permission(question, options) => answer,
        () = env.cancel.cancelled() => return Decision::Cancelled,
    };
    match answer {
        Ok(PermissionDecision::Selected(option)) => match option.to_string().as_str() {
            ALLOW_ONCE => Decision::Allowed,
            ALLOW_ALWAYS => {
                env.session
                    .grant(asks.iter().map(|check| always(check)).collect());
                Decision::Allowed
            }
            _ => Decision::Rejected,
        },
        Ok(PermissionDecision::Cancelled) | Err(_) => Decision::Cancelled,
    }
}

/// The rule an "always allow" answer adds: the command's leading words for a shell command,
/// every file for an edit, the directory and below for an outside directory.
fn always(check: &Check) -> Rule {
    let resource = match check.action.as_str() {
        "shell" => {
            let words: Vec<&str> = check.resource.split_whitespace().take(2).collect();
            match words.as_slice() {
                [program, sub] if !sub.starts_with('-') && !sub.contains(['/', '.', '"', '\'']) => {
                    format!("{program} {sub}*")
                }
                [program, ..] => format!("{program}*"),
                [] => check.resource.clone(),
            }
        }
        "edit" => "*".to_owned(),
        "external_directory" => format!("{}*", check.resource),
        _ => check.resource.clone(),
    };
    Rule::new(&check.action, &resource, Effect::Allow)
}

#[cfg(test)]
mod cancellation_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use super::*;

    struct ProtectedTool {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for ProtectedTool {
        fn spec(&self) -> nori_llm::ToolSpec {
            nori_llm::ToolSpec {
                name: "protected".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            }
        }

        async fn prepare(
            &self,
            _: &serde_json::Value,
            _: &ToolContext,
        ) -> Result<Invocation, String> {
            Ok(Invocation::new(
                "protected",
                nori_acp_kit::schema::ToolKind::Edit,
            ))
        }

        fn handles_cancellation(&self) -> bool {
            true
        }

        async fn run(&self, _: serde_json::Value, _: &ToolContext) -> ToolOutput {
            self.started.notify_one();
            self.release.notified().await;
            ToolOutput::text("finished")
        }
    }

    #[tokio::test]
    async fn cancellation_waits_for_a_tool_that_owns_its_commit() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let tool = ProtectedTool {
            started: started.clone(),
            release: release.clone(),
        };
        let context = ToolContext::new(
            std::path::PathBuf::from("/"),
            vec![],
            nori_tools::WorkspaceConfig::default(),
        );
        let cancel = context.cancel.clone();
        let task =
            tokio::spawn(async move { run_tool(&tool, serde_json::json!({}), &context).await });
        started.notified().await;
        cancel.cancel();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        release.notify_one();
        let output = task.await.unwrap();
        assert!(!output.is_error);
        pretty_assertions::assert_eq!(
            output.result,
            vec![ToolResultPart::Text {
                text: "finished".into()
            }]
        );
    }
}
