//! Nori's host tools (its `bash`, goals, delegation, monitors, ...) as engine tools: plain
//! function calls into Nori, scoped to the session.

use std::sync::Arc;

use async_trait::async_trait;
use nori_acp_kit::HostBridge;
use nori_acp_kit::schema::ToolKind;
use nori_host_tools::HostToolSpec;
use nori_llm::ToolResultPart;
use nori_llm::ToolSpec;
use serde_json::Value;

use super::Check;
use super::Invocation;
use super::Tool;
use super::ToolContext;
use super::ToolOutput;

pub struct HostTool {
    bridge: HostBridge,
    spec: HostToolSpec,
}

impl HostTool {
    /// Every tool `bridge` offers.
    pub fn all(bridge: &HostBridge) -> Vec<Arc<dyn Tool>> {
        bridge
            .specs()
            .into_iter()
            .map(|spec| {
                Arc::new(Self {
                    bridge: bridge.clone(),
                    spec,
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    fn is_shell(&self) -> bool {
        self.spec.name == "bash"
    }
}

#[async_trait]
impl Tool for HostTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.spec.name.clone(),
            description: self.spec.description.clone(),
            input_schema: self.spec.input_schema.clone(),
        }
    }

    async fn prepare(&self, input: &Value, _context: &ToolContext) -> Result<Invocation, String> {
        let mut invocation = if self.is_shell() {
            let command = input["command"]
                .as_str()
                .ok_or("The `command` argument is required and must be a string.")?;
            let mut invocation = Invocation::new(
                command.lines().next().unwrap_or_default().to_owned(),
                ToolKind::Execute,
            );
            invocation.checks = vec![Check::new("shell", command)];
            invocation
        } else {
            Invocation::new(self.spec.name.clone(), ToolKind::Other)
        };
        invocation.meta = self.bridge.call_meta(&self.spec.name);
        Ok(invocation)
    }

    async fn run(&self, input: Value, _context: &ToolContext) -> ToolOutput {
        let output = self.bridge.call(&self.spec.name, input).await;
        let (display, raw_output) = nori_acp_kit::output_payload(&output);
        let mut result = model_content(&output);
        if let Some(context) = &output.model_context {
            result.push(ToolResultPart::Text {
                text: context.clone(),
            });
        }
        ToolOutput {
            result,
            is_error: output.is_error,
            display,
            raw_output: Some(raw_output),
            plan: None,
        }
    }
}

/// Preserve the complete MCP result for the model, including content types it cannot view.
fn model_content(output: &nori_host_tools::HostToolOutput) -> Vec<ToolResultPart> {
    let Some(canonical) = &output.mcp_result else {
        return vec![ToolResultPart::Text {
            text: output.text.clone(),
        }];
    };
    let mut parts = Vec::new();
    for block in canonical
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") if block.as_object().is_some_and(|object| object.len() == 2) => {
                match block.get("text").and_then(Value::as_str) {
                    Some(text) => parts.push(ToolResultPart::Text {
                        text: text.to_owned(),
                    }),
                    None => parts.push(ToolResultPart::Text {
                        text: block.to_string(),
                    }),
                }
            }
            Some("image") => {
                if let (Some(media_type), Some(data)) = (
                    block.get("mimeType").and_then(Value::as_str),
                    block.get("data").and_then(Value::as_str),
                ) {
                    parts.push(ToolResultPart::Image {
                        media_type: media_type.to_owned(),
                        data: data.to_owned(),
                    });
                    if block.as_object().is_some_and(|object| object.len() > 3) {
                        parts.push(ToolResultPart::Text {
                            text: block.to_string(),
                        });
                    }
                } else {
                    parts.push(ToolResultPart::Text {
                        text: block.to_string(),
                    });
                }
            }
            Some(_) | None => parts.push(ToolResultPart::Text {
                text: block.to_string(),
            }),
        }
    }
    if let Some(object) = canonical.as_object() {
        for (key, value) in object {
            if !matches!(key.as_str(), "content" | "isError") {
                parts.push(ToolResultPart::Text {
                    text: Value::Object(std::iter::once((key.clone(), value.clone())).collect())
                        .to_string(),
                });
            }
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn preserves_rich_and_unknown_host_results_for_the_model() {
        let output = nori_host_tools::HostToolOutput::from_mcp_result(json!({
            "content": [
                {"type":"text", "text":"done"},
                {"type":"image", "mimeType":"image/png", "data":"YWJj"},
                {"type":"resource", "resource":{"uri":"file:///x", "text":"body"}}
            ],
            "structuredContent": {"ok":true}, "_meta": {"trace":"x"}, "isError":false
        }));
        let parts = model_content(&output);
        assert_eq!(
            parts[0],
            ToolResultPart::Text {
                text: "done".into()
            }
        );
        assert_eq!(
            parts[1],
            ToolResultPart::Image {
                media_type: "image/png".into(),
                data: "YWJj".into()
            }
        );
        assert!(parts.iter().any(
            |part| matches!(part, ToolResultPart::Text { text } if text.contains("file:///x"))
        ));
        assert!(parts.iter().any(|part| matches!(part, ToolResultPart::Text { text } if text.contains("structuredContent"))));
        assert!(
            parts.iter().any(
                |part| matches!(part, ToolResultPart::Text { text } if text.contains("_meta"))
            )
        );
    }
}
