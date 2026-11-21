//! Session-local selection and model-facing descriptions of host tools.
//!
//! The same selected specifications gate calls and produce the prompt. This keeps an
//! agent from executing a tool that its session did not advertise.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::HostToolFuture;
use crate::HostToolOutput;
use crate::HostToolSpec;
use crate::HostTools;

/// Session-specific changes to the host's registered tool catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSelection {
    /// If present, only these registered tools are enabled. An empty list disables all.
    pub enabled: Option<BTreeSet<String>>,
    /// Names to remove from the enabled set.
    pub disabled: BTreeSet<String>,
    /// Model-facing descriptions, keyed by the registered name.
    pub descriptions: BTreeMap<String, String>,
    /// Additional instructions, appended after the generated list.
    pub instructions: Vec<String>,
}

/// A fixed session view of an underlying registry. Unknown names remain unavailable;
/// unrelated agent and MCP tools are not affected.
pub struct SelectedTools {
    inner: Arc<dyn HostTools>,
    specs: Vec<HostToolSpec>,
    instructions: String,
}

impl SelectedTools {
    pub fn new(inner: Arc<dyn HostTools>, selection: &ToolSelection) -> Self {
        let specs: Vec<_> = inner
            .specs()
            .into_iter()
            .filter(|spec| {
                selection
                    .enabled
                    .as_ref()
                    .is_none_or(|names| names.contains(&spec.name))
                    && !selection.disabled.contains(&spec.name)
            })
            .map(|mut spec| {
                if let Some(description) = selection.descriptions.get(&spec.name) {
                    spec.description.clone_from(description);
                }
                spec
            })
            .collect();
        let mut lines = Vec::new();
        if !specs.is_empty() {
            lines.push("Available Nori tools:".to_owned());
            lines.extend(
                specs
                    .iter()
                    .map(|spec| format!("- {}: {}", spec.name, spec.description)),
            );
        }
        if ["agent_capabilities", "delegate_task"]
            .iter()
            .all(|name| specs.iter().any(|spec| spec.name == *name))
        {
            lines.push(crate::DELEGATION_INSTRUCTIONS.to_owned());
        }
        if specs.iter().any(|spec| spec.name == "monitor_start") {
            lines.push(crate::MONITOR_INSTRUCTIONS.to_owned());
        }
        if specs.iter().any(|spec| spec.name == "job_start") {
            lines.push(crate::JOB_INSTRUCTIONS.to_owned());
        }
        lines.extend(selection.instructions.iter().cloned());
        Self {
            inner,
            specs,
            instructions: lines.join("\n"),
        }
    }
}

impl HostTools for SelectedTools {
    fn bind_session_id(&self, session_id: &str) {
        self.inner.bind_session_id(session_id);
    }

    fn instructions(&self) -> String {
        self.instructions.clone()
    }

    fn specs(&self) -> Vec<HostToolSpec> {
        self.specs.clone()
    }

    fn call(&self, name: &str, arguments: Value) -> HostToolFuture {
        if self.specs.iter().any(|spec| spec.name == name) {
            self.inner.call(name, arguments)
        } else {
            let message = format!("Nori tool {name} is not available in this session");
            Box::pin(async move { HostToolOutput::error(message) })
        }
    }

    fn call_scoped(&self, name: &str, arguments: Value, session_id: &str) -> HostToolFuture {
        if self.specs.iter().any(|spec| spec.name == name) {
            self.inner.call_scoped(name, arguments, session_id)
        } else {
            let message = format!("Nori tool {name} is not available in this session");
            Box::pin(async move { HostToolOutput::error(message) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    struct Registry;
    impl HostTools for Registry {
        fn instructions(&self) -> String {
            "old text".into()
        }
        fn specs(&self) -> Vec<HostToolSpec> {
            ["read", "write"].into_iter().map(|name| HostToolSpec {
                name: name.into(), description: format!("{name} data"),
                input_schema: json!({"type":"object", "properties":{"value":{"type":"string"}}}),
                annotations: None, meta: None,
            }).collect()
        }
        fn call(&self, name: &str, arguments: Value) -> HostToolFuture {
            let text = format!("{name}: {arguments}");
            Box::pin(async move { HostToolOutput::text(text) })
        }
    }

    #[test]
    fn selection_keeps_schema_and_builds_prompt_from_exposed_definitions() {
        let selection = ToolSelection {
            disabled: BTreeSet::from(["write".into()]),
            descriptions: BTreeMap::from([("read".into(), "Read the current data.".into())]),
            instructions: vec!["Use exact values.".into()],
            ..Default::default()
        };
        let tools = SelectedTools::new(Arc::new(Registry), &selection);
        assert_eq!(
            tools.specs(),
            vec![HostToolSpec {
                name: "read".into(),
                description: "Read the current data.".into(),
                input_schema: json!({"type":"object", "properties":{"value":{"type":"string"}}}),
                annotations: None,
                meta: None,
            }]
        );
        assert_eq!(
            tools.instructions(),
            "Available Nori tools:\n- read: Read the current data.\nUse exact values."
        );
    }

    #[tokio::test]
    async fn disabled_calls_do_not_reach_the_executor_and_enabled_calls_keep_arguments() {
        let selection = ToolSelection {
            enabled: Some(BTreeSet::from(["read".into()])),
            ..Default::default()
        };
        let tools = SelectedTools::new(Arc::new(Registry), &selection);
        assert_eq!(
            tools.call("write", json!({"value": "secret"})).await,
            HostToolOutput::error("Nori tool write is not available in this session")
        );
        assert_eq!(
            tools.call("unknown", json!({})).await,
            HostToolOutput::error("Nori tool unknown is not available in this session")
        );
        assert_eq!(
            tools.call("read", json!({"value": "unchanged"})).await,
            HostToolOutput::text(r#"read: {"value":"unchanged"}"#)
        );
    }
}
