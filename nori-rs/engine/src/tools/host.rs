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
        let mut result = vec![ToolResultPart::Text {
            text: output.text.clone(),
        }];
        for block in output
            .as_mcp_result()
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block["type"] == "image"
                && let (Some(media_type), Some(data)) =
                    (block["mimeType"].as_str(), block["data"].as_str())
            {
                result.push(ToolResultPart::Image {
                    media_type: media_type.to_owned(),
                    data: data.to_owned(),
                });
            }
        }
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
