//! The language server tools of the `nori-client` server (`lsp_diagnostics`, `lsp_definition`,
//! `lsp_references`, `lsp_hover`, `lsp_symbols`, `lsp_rename`, `lsp_call_hierarchy`, from
//! [`nori_lsp::tools`]). They are in the default catalog: no agent has them natively, and a
//! session that never calls one never starts a server.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use serde_json::Value;

use super::NoriClientService;
use super::file_tools::Group;
use super::file_tools::dynamic_route;
use super::file_tools::paths_named_by;
use super::tool_error;

impl NoriClientService {
    pub(super) fn lsp_tool_router() -> ToolRouter<Self> {
        let mut router = ToolRouter::new();
        for spec in nori_lsp::tools::specs() {
            router.add_route(dynamic_route(
                Group::Lsp,
                spec.name,
                spec.description,
                spec.input_schema,
                // Even `lsp_rename` only previews: nothing is written.
                true,
            ));
        }
        router
    }

    pub(in crate::backend) async fn call_lsp_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> CallToolResult {
        let Some(cwd) = self.shared_cwd() else {
            return tool_error("this session has no working directory");
        };
        if let Err(error) = self.enforce_path_sandbox(&cwd, &paths_named_by(name, &arguments)) {
            return tool_error(error);
        }
        let manager = self.shared.tooling.lsp(&cwd);
        let output = nori_lsp::tools::call(&manager, name, arguments).await;
        let content = vec![Content::text(output.text)];
        if output.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        }
    }
}
