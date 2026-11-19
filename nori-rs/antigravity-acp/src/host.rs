//! Nori's host tools, given to the SDK as function tools.

use std::sync::Arc;

use antigravity_sdk_rust::tools::DynTool;
use antigravity_sdk_rust::tools::Tool;
use antigravity_sdk_rust::types::AppendedSystemInstructions;
use antigravity_sdk_rust::types::SystemInstructionSection;
use antigravity_sdk_rust::types::SystemInstructions;
use nori_acp_kit::HostBridge;
use nori_host_tools::HostToolOutput;
use nori_host_tools::HostToolSpec;
use serde_json::Value;

/// The tools as SDK tools, which the SDK's own tool runner calls.
pub(crate) fn sdk_tools(host: &HostBridge) -> Vec<Arc<dyn DynTool>> {
    host.specs()
        .into_iter()
        .map(|spec| {
            let schema_json =
                serde_json::to_string(&spec.input_schema).unwrap_or_else(|_| "{}".to_owned());
            Arc::new(HostToolBridge {
                spec,
                schema_json,
                host: host.clone(),
            }) as Arc<dyn DynTool>
        })
        .collect()
}

/// The host tools' instructions as an appended system-instruction section.
pub(crate) fn instruction_sections(host: Option<&HostBridge>) -> Vec<SystemInstructionSection> {
    host.and_then(HostBridge::instructions)
        .map(|content| SystemInstructionSection {
            title: "Nori Host Tools".to_owned(),
            content,
        })
        .into_iter()
        .collect()
}

pub(crate) fn appended(sections: Vec<SystemInstructionSection>) -> Option<SystemInstructions> {
    (!sections.is_empty()).then_some(SystemInstructions::Appended(AppendedSystemInstructions {
        custom_identity: None,
        appended_sections: sections,
    }))
}

fn output_value(output: &HostToolOutput) -> Value {
    let mut value = output.as_mcp_result();
    if let Some(context) = &output.model_context
        && let Some(object) = value.as_object_mut()
    {
        object.insert("modelContext".to_owned(), Value::String(context.clone()));
    }
    value
}

fn error_text(output: &HostToolOutput) -> String {
    if output.text.trim().is_empty() {
        output.as_mcp_result().to_string()
    } else {
        output.text.clone()
    }
}

struct HostToolBridge {
    spec: HostToolSpec,
    schema_json: String,
    host: HostBridge,
}

impl Tool for HostToolBridge {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn parameters_json_schema(&self) -> &str {
        &self.schema_json
    }

    async fn call(&self, args: Value) -> Result<Value, anyhow::Error> {
        let output = self.host.call(&self.spec.name, args).await;
        if output.is_error {
            Err(anyhow::anyhow!("{}", error_text(&output)))
        } else {
            Ok(output_value(&output))
        }
    }
}
