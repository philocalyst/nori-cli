//! Nori's host tools, as an in-process agent receives them.
//!
//! When the host runs an agent in its own process, a session request's `_meta` carries a token
//! for the session's tools (see `nori_host_tools`). [`HostBridge`] is what an agent keeps of
//! them: the specs to show the model, the instructions to give it, and a way to run a call
//! that the host scopes to the right session. How the specs reach the provider's model is the
//! agent's own business (Codex dynamic tools, a Claude SDK MCP server, SDK function tools).

use std::collections::HashSet;
use std::sync::Arc;

use nori_host_tools::HostToolOutput;
use nori_host_tools::HostToolSpec;
use nori_host_tools::HostTools;
use serde_json::Value;

use crate::schema::Meta;
use crate::schema::ToolCallContent;
use crate::tools::mcp_content;

/// One session's host tools. Cheap to clone.
#[derive(Clone)]
pub struct HostBridge {
    tools: Arc<dyn HostTools>,
    session_id: String,
    names: Arc<HashSet<String>>,
}

impl std::fmt::Debug for HostBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostBridge")
            .field("session_id", &self.session_id)
            .field("tools", &self.names.len())
            .finish()
    }
}

impl HostBridge {
    /// The tools a session request's `_meta` names, bound to `session_id` so conversation-local
    /// context stays with the right session. `None` when the host sent none.
    pub fn from_session_meta(meta: Option<&Meta>, session_id: &str) -> Option<Self> {
        let tools = nori_host_tools::from_session_meta(meta)?;
        tools.bind_session_id(session_id);
        let names = tools.specs().into_iter().map(|spec| spec.name).collect();
        Some(Self {
            tools,
            session_id: session_id.to_owned(),
            names: Arc::new(names),
        })
    }

    pub fn specs(&self) -> Vec<HostToolSpec> {
        self.tools.specs()
    }

    /// What to tell the model about the tools; `None` when there is nothing to say.
    pub fn instructions(&self) -> Option<String> {
        Some(self.tools.instructions()).filter(|text| !text.trim().is_empty())
    }

    pub fn is_host_tool(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// The `_meta` that marks an ACP tool call as host tool `name`, for tools that are.
    pub fn call_meta(&self, name: &str) -> Option<Meta> {
        self.is_host_tool(name)
            .then(|| nori_host_tools::tool_call_meta(name))
    }

    /// Run host tool `name`. An unknown name or bad arguments come back as an error output.
    pub async fn call(&self, name: &str, arguments: Value) -> HostToolOutput {
        self.tools
            .call_scoped(name, arguments, &self.session_id)
            .await
    }
}

/// A host tool's output as ACP tool-call content and the raw output to attach to the call.
pub fn output_payload(output: &HostToolOutput) -> (Vec<ToolCallContent>, Value) {
    let result = output.as_mcp_result();
    (mcp_content(&result, &output.text), result)
}
