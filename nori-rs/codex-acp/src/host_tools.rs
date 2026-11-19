//! Nori's host tools, given to Codex as dynamic tools.
//!
//! When Nori runs this agent in its own process, a session request's `_meta` carries a token for
//! the session's tools (see `nori_host_tools`). They reach the model as Codex dynamic tools: the
//! specs go in at thread start, their instructions join the thread's instructions, and when the
//! model calls one, core emits `DynamicToolCallRequest` and waits for an
//! `Op::DynamicToolResponse`, which [`answer`] sends after running Nori's code directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_core_api::Instructions;
use codex_core_api::LoadInstructionsFuture;
use codex_core_api::LoadedUserInstructions;
use codex_core_api::ThreadInstructionsProvider;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::Op;
use nori_host_tools::HostTools;
use tracing::warn;

use crate::thread::CodexThreadImpl;

/// The tools as Codex dynamic tools, loaded up front so the model always sees them.
pub(crate) fn dynamic_tools(tools: &dyn HostTools) -> Vec<DynamicToolSpec> {
    tools
        .specs()
        .into_iter()
        .map(|spec| {
            DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
                defer_loading: false,
            })
        })
        .collect()
}

/// The tools' instructions, as the thread's own instructions.
pub(crate) struct HostInstructions(pub(crate) Arc<dyn HostTools>);

impl ThreadInstructionsProvider for HostInstructions {
    fn load_thread_instructions(&self) -> LoadInstructionsFuture<'_> {
        let text = self.0.instructions();
        Box::pin(async move {
            LoadedUserInstructions {
                instructions: (!text.trim().is_empty())
                    .then_some(Instructions { text, source: None }),
                warnings: Vec::new(),
            }
        })
    }
}

/// Codex dynamic tools support text, images, and audio. Preserve other MCP blocks and
/// structured results as JSON text, including unknown future block types.
fn dynamic_output(
    output: &nori_host_tools::HostToolOutput,
) -> Vec<DynamicToolCallOutputContentItem> {
    let result = output.as_mcp_result();
    let mut items = Vec::new();
    if let Some(blocks) = result.get("content").and_then(serde_json::Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(serde_json::Value::as_str) {
                Some("text") if block.as_object().is_some_and(|object| object.len() == 2) => {
                    if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                        items.push(DynamicToolCallOutputContentItem::InputText {
                            text: text.to_owned(),
                        });
                    } else {
                        items.push(DynamicToolCallOutputContentItem::InputText {
                            text: block.to_string(),
                        });
                    }
                }
                Some(media @ ("image" | "audio")) => {
                    if let (Some(mime), Some(data)) = (
                        block.get("mimeType").and_then(serde_json::Value::as_str),
                        block.get("data").and_then(serde_json::Value::as_str),
                    ) {
                        let url = format!("data:{mime};base64,{data}");
                        items.push(if media == "image" {
                            DynamicToolCallOutputContentItem::InputImage { image_url: url }
                        } else {
                            DynamicToolCallOutputContentItem::InputAudio { audio_url: url }
                        });
                        if block.as_object().is_some_and(|object| object.len() > 3) {
                            items.push(DynamicToolCallOutputContentItem::InputText {
                                text: block.to_string(),
                            });
                        }
                    } else {
                        items.push(DynamicToolCallOutputContentItem::InputText {
                            text: block.to_string(),
                        });
                    }
                }
                _ => items.push(DynamicToolCallOutputContentItem::InputText {
                    text: block.to_string(),
                }),
            }
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        items.push(DynamicToolCallOutputContentItem::InputText {
            text: serde_json::json!({"structuredContent": structured}).to_string(),
        });
    }
    if let Some(meta) = result.get("_meta") {
        items.push(DynamicToolCallOutputContentItem::InputText {
            text: serde_json::json!({"_meta": meta}).to_string(),
        });
    }
    if let Some(object) = result.as_object() {
        let extra: serde_json::Map<String, serde_json::Value> = object
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "content" | "structuredContent" | "_meta" | "isError"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !extra.is_empty() {
            items.push(DynamicToolCallOutputContentItem::InputText {
                text: serde_json::Value::Object(extra).to_string(),
            });
        }
    }
    if let Some(context) = &output.model_context {
        items.push(DynamicToolCallOutputContentItem::InputText {
            text: context.clone(),
        });
    }
    items
}

pub(crate) type GoalContexts = Arc<Mutex<HashMap<String, String>>>;

/// Remove only the context actually attached to this call, leaving identical
/// user/tool-provided text elsewhere in the canonical result untouched.
pub(crate) fn remove_goal_context(
    items: &mut Vec<DynamicToolCallOutputContentItem>,
    context: &str,
) -> bool {
    let Some(index) = items.iter().rposition(|item| matches!(item, DynamicToolCallOutputContentItem::InputText { text } if text == context)) else { return false; };
    items.remove(index);
    true
}

/// Run the host tool the model called and hand core its answer. Core waits for the answer, so
/// one is always sent: an error when the tool fails, or when this session has no host tools
/// (a thread saved with them, resumed by a client that does not give them).
#[expect(clippy::too_many_arguments)]
pub(crate) fn answer(
    tools: Option<Arc<dyn HostTools>>,
    thread: Arc<dyn CodexThreadImpl>,
    call_id: String,
    tool: String,
    arguments: serde_json::Value,
    session_id: String,
    goal_contexts: GoalContexts,
    cancellation: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let output = match tools {
            Some(tools) => tokio::select! {
                biased;
                _ = cancellation.cancelled() => nori_host_tools::HostToolOutput::cancelled(&call_id, &tool),
                output = tools.call_scoped(&tool, arguments, &session_id) => output,
            },
            None => nori_host_tools::HostToolOutput::error(format!(
                "`{tool}` is not available in this session"
            )),
        };
        if let Some(context) = &output.model_context {
            goal_contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(call_id.clone(), context.clone());
        }
        let op = Op::DynamicToolResponse {
            id: call_id.clone(),
            response: DynamicToolResponse {
                content_items: dynamic_output(&output),
                success: !output.is_error,
            },
        };
        if let Err(error) = thread.submit(op).await {
            goal_contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&call_id);
            warn!("Could not answer the call to host tool {tool}: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use nori_host_tools::HostToolFuture;
    use nori_host_tools::HostToolOutput;
    use nori_host_tools::HostToolSpec;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    struct Tools;

    impl HostTools for Tools {
        fn instructions(&self) -> String {
            "Commit in small batches.".to_owned()
        }

        fn specs(&self) -> Vec<HostToolSpec> {
            vec![HostToolSpec {
                name: "git_commit".to_owned(),
                description: "Commit some paths.".to_owned(),
                input_schema: json!({"type": "object", "required": ["paths"]}),
                annotations: Some(json!({"destructiveHint": false})),
                meta: None,
            }]
        }

        fn call(&self, _name: &str, _arguments: serde_json::Value) -> HostToolFuture {
            Box::pin(async { HostToolOutput::error("unused") })
        }
    }

    struct WaitingTools(Arc<tokio::sync::Notify>);

    impl HostTools for WaitingTools {
        fn instructions(&self) -> String {
            String::new()
        }
        fn specs(&self) -> Vec<HostToolSpec> {
            Vec::new()
        }
        fn call(&self, _name: &str, _arguments: serde_json::Value) -> HostToolFuture {
            let started = Arc::clone(&self.0);
            Box::pin(async move {
                started.notify_one();
                std::future::pending::<HostToolOutput>().await
            })
        }
    }

    struct RecordingThread(tokio::sync::mpsc::UnboundedSender<Op>);

    impl CodexThreadImpl for RecordingThread {
        fn submit(
            &self,
            op: Op,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<String, codex_protocol::error::CodexErr>>
                    + Send
                    + '_,
            >,
        > {
            drop(self.0.send(op));
            Box::pin(async { Ok("submitted".to_owned()) })
        }
        fn next_event(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            codex_protocol::protocol::Event,
                            codex_protocol::error::CodexErr,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(std::future::pending())
        }
    }

    struct ScopedTools(tokio::sync::mpsc::UnboundedSender<String>);

    impl HostTools for ScopedTools {
        fn instructions(&self) -> String {
            String::new()
        }
        fn specs(&self) -> Vec<HostToolSpec> {
            Vec::new()
        }
        fn call(&self, _name: &str, _arguments: serde_json::Value) -> HostToolFuture {
            Box::pin(async { HostToolOutput::error("missing session scope") })
        }
        fn call_scoped(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
            session_id: &str,
        ) -> HostToolFuture {
            drop(self.0.send(session_id.to_owned()));
            Box::pin(async { HostToolOutput::text("done") })
        }
    }

    #[tokio::test]
    async fn dynamic_host_call_carries_trusted_session_id() {
        let (scope_tx, mut scope_rx) = tokio::sync::mpsc::unbounded_channel();
        let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
        answer(
            Some(Arc::new(ScopedTools(scope_tx))),
            Arc::new(RecordingThread(result_tx)),
            "call".into(),
            "tool".into(),
            json!({}),
            "root-session".into(),
            Arc::default(),
            tokio_util::sync::CancellationToken::new(),
        );
        assert_eq!(scope_rx.recv().await.as_deref(), Some("root-session"));
        let Op::DynamicToolResponse { response, .. } = result_rx.recv().await.unwrap() else {
            panic!("expected dynamic response")
        };
        assert!(response.success);
    }

    #[test]
    fn goal_context_stays_model_visible_but_is_hidden_from_nori_tool_ui() {
        let context = "<goal_context>updated</goal_context>";
        let canonical =
            json!({"content":[{"type":"text","text":context}],"structuredContent":{"keep":true}});
        let mut output = HostToolOutput::from_mcp_result(canonical.clone());
        output.model_context = Some(context.to_owned());
        let mut items = dynamic_output(&output);
        assert_eq!(items.iter().filter(|item| matches!(item, DynamicToolCallOutputContentItem::InputText { text } if text == context)).count(), 2);
        assert!(remove_goal_context(&mut items, context));
        assert_eq!(items.iter().filter(|item| matches!(item, DynamicToolCallOutputContentItem::InputText { text } if text == context)).count(), 1);
        assert_eq!(output.as_mcp_result(), canonical);
    }

    #[tokio::test]
    async fn cancelled_call_is_answered_with_its_correlated_structured_receipt() {
        let started = Arc::new(tokio::sync::Notify::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancellation = tokio_util::sync::CancellationToken::new();
        answer(
            Some(Arc::new(WaitingTools(Arc::clone(&started)))),
            Arc::new(RecordingThread(tx)),
            "call-42".into(),
            "slow".into(),
            json!({}),
            "session".into(),
            Arc::default(),
            cancellation.clone(),
        );
        started.notified().await;
        cancellation.cancel();
        let op = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let Op::DynamicToolResponse { id, response } = op else {
            panic!("expected dynamic response");
        };
        assert_eq!(id, "call-42");
        assert!(!response.success);
        assert!(response.content_items.iter().any(|item| match item {
            DynamicToolCallOutputContentItem::InputText { text } =>
                text.contains(r#""status":"cancelled""#) && text.contains("call-42"),
            DynamicToolCallOutputContentItem::InputImage { .. }
            | DynamicToolCallOutputContentItem::InputAudio { .. } => false,
        }));
    }

    #[test]
    fn rich_results_map_images_and_preserve_unknown_content_as_json() {
        let result = json!({
            "content": [
                {"type":"text", "text":"done"},
                {"type":"image", "mimeType":"image/png", "data":"YWJj"},
                {"type":"audio", "mimeType":"audio/wav", "data":"YWJj"},
                {"type":"resource", "resource":{"uri":"file:///x", "text":"body"}},
                {"type":"future", "value":42}
            ],
            "structuredContent":{"ok":true}, "_meta":{"trace":"x"},
            "futureField":{"x":1}, "isError":false
        });
        let output = HostToolOutput::from_mcp_result(result);
        let items = dynamic_output(&output);
        assert_eq!(
            items,
            vec![
                DynamicToolCallOutputContentItem::InputText {
                    text: "done".into()
                },
                DynamicToolCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,YWJj".into()
                },
                DynamicToolCallOutputContentItem::InputAudio {
                    audio_url: "data:audio/wav;base64,YWJj".into()
                },
                DynamicToolCallOutputContentItem::InputText {
                    text: json!({"type":"resource", "resource":{"uri":"file:///x", "text":"body"}})
                        .to_string()
                },
                DynamicToolCallOutputContentItem::InputText {
                    text: json!({"type":"future", "value":42}).to_string()
                },
                DynamicToolCallOutputContentItem::InputText {
                    text: json!({"structuredContent":{"ok":true}}).to_string()
                },
                DynamicToolCallOutputContentItem::InputText {
                    text: json!({"_meta":{"trace":"x"}}).to_string()
                },
                DynamicToolCallOutputContentItem::InputText {
                    text: json!({"futureField":{"x":1}}).to_string()
                },
            ]
        );
    }

    #[test]
    fn specs_become_function_tools_the_model_always_sees() {
        let specs = dynamic_tools(&Tools);
        let [DynamicToolSpec::Function(spec)] = specs.as_slice() else {
            panic!("one function tool: {specs:?}");
        };
        assert_eq!(
            spec,
            &DynamicToolFunctionSpec {
                name: "git_commit".to_owned(),
                description: "Commit some paths.".to_owned(),
                input_schema: json!({"type": "object", "required": ["paths"]}),
                defer_loading: false,
            }
        );
    }

    #[tokio::test]
    async fn instructions_reach_the_thread() {
        let loaded = HostInstructions(Arc::new(Tools))
            .load_thread_instructions()
            .await;
        assert_eq!(
            loaded.instructions.map(|instructions| instructions.text),
            Some("Commit in small batches.".to_owned())
        );
    }
}
