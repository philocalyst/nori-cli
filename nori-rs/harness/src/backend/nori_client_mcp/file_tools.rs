//! The file tools of the `nori-client` server (`read`, `write`, `edit`, `multi_edit`,
//! `apply_patch`, from [`nori_tools`]), and the per-session state they share with the language
//! server tools (see [`super::lsp_tools`]).
//!
//! Both groups are data-defined (name, description and schema come from the crates), so they are
//! registered as dynamic routes rather than `#[tool]` methods. Every call, over MCP or as a host
//! tool, ends in [`NoriClientService::call_file_tool`] or [`NoriClientService::call_lsp_tool`].
//!
//! The file tools are opt-in, like the git tools: every agent Nori runs already has its own
//! file tools, and a second set under another name only confuses the model. A workflow lists
//! them in `tools.enabled` for an agent that has none.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;

use base64::Engine as _;
use futures::FutureExt as _;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use serde_json::Value;

use super::NoriClientService;
use super::tool_error;

/// The names of the file tools, as `nori_tools::catalog(EditStyle::Both)` lists them.
pub(super) const FILE_TOOLS: [&str; 5] = ["read", "write", "edit", "multi_edit", "apply_patch"];

/// The session's file workspaces and language servers. Created lazily by the first call that
/// needs them and shared by every clone of the session's [`super::NoriClientShared`]. The
/// language servers are the process's for the session's root (`LspManager::shared`), so the
/// prompt editor and every session on the same root use the same ones.
#[derive(Default)]
pub(crate) struct SessionTooling {
    /// One workspace per caller: each agent has its own read ledger.
    workspaces: Mutex<HashMap<String, nori_tools::Workspace>>,
    /// One workspace per caller and other machine, rooted where the caller's last command there
    /// ended (or its home), reaching the machine through its own connection.
    remote: Mutex<HashMap<(String, nori_machines::MachineName), nori_tools::Workspace>>,
    lsp: OnceLock<nori_lsp::LspManager>,
}

impl SessionTooling {
    pub(super) fn workspace(&self, caller: &str, cwd: &Path) -> nori_tools::Workspace {
        self.workspaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(caller.to_owned())
            .or_insert_with(|| nori_tools::Workspace::local(cwd))
            .clone()
    }

    fn remote_workspace(
        &self,
        key: &(String, nori_machines::MachineName),
    ) -> Option<nori_tools::Workspace> {
        self.remote
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    /// Must be called inside the tokio runtime: the manager starts its actor.
    pub(super) fn lsp(&self, cwd: &Path) -> nori_lsp::LspManager {
        self.lsp
            .get_or_init(|| nori_lsp::LspManager::shared(cwd))
            .clone()
    }
}

/// Which dispatcher a dynamic route calls.
#[derive(Clone, Copy)]
pub(super) enum Group {
    File,
    Lsp,
}

/// A route whose name, description and schema are data.
pub(super) fn dynamic_route(
    group: Group,
    name: &'static str,
    description: String,
    input_schema: Value,
    read_only: bool,
) -> ToolRoute<NoriClientService> {
    let mut attr = Tool::new(name, description, rmcp::model::object(input_schema));
    attr.annotations = Some(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(!read_only)
            .open_world(false),
    );
    ToolRoute::new_dyn(
        attr,
        move |context: ToolCallContext<'_, NoriClientService>| {
            let service = context.service;
            let arguments = Value::Object(context.arguments.unwrap_or_default());
            async move {
                Ok(match group {
                    Group::File => service.call_file_tool(name, arguments).await,
                    Group::Lsp => service.call_lsp_tool(name, arguments).await,
                })
            }
            .boxed()
        },
    )
}

/// Which of the session's tools a workflow may not use because of what it says about
/// `selection`: file tools are opt-in, so they stay unless named in `tools.enabled`.
pub(super) fn remove_unless_opted_in(
    router: &mut ToolRouter<NoriClientService>,
    selection: Option<&nori_host_tools::ToolSelection>,
) {
    let enabled = selection.and_then(|selection| selection.enabled.as_ref());
    for name in FILE_TOOLS {
        if !enabled.is_some_and(|names| names.contains(name)) {
            router.remove_route(name);
        }
    }
}

impl NoriClientService {
    pub(super) fn file_tool_router() -> ToolRouter<Self> {
        let mut router = ToolRouter::new();
        for spec in nori_tools::catalog(nori_tools::EditStyle::Both) {
            let mut schema = spec.input_schema;
            // `describe_machines` fills in which machines; the call strips it before the tool.
            if let Some(Value::Object(properties)) = schema.get_mut("properties") {
                properties.insert("machine".to_owned(), serde_json::json!({"type": "string"}));
            }
            router.add_route(dynamic_route(
                Group::File,
                spec.name,
                spec.description,
                schema,
                spec.read_only,
            ));
        }
        router
    }

    /// Run a file tool as the model asked (on this computer, or on the machine it names), then
    /// tell the language servers what changed and append what they think of it.
    pub(in crate::backend) async fn call_file_tool(
        &self,
        name: &str,
        mut arguments: Value,
    ) -> CallToolResult {
        let Some(cwd) = self.shared_cwd() else {
            return tool_error("this session has no working directory");
        };
        let machine = arguments
            .as_object_mut()
            .and_then(|object| object.remove("machine"))
            .and_then(|machine| machine.as_str().map(str::to_owned));
        let roster = nori_machines::global_ready().await;
        let target = match roster.resolve(machine.as_deref()) {
            Ok(nori_machines::Target::Local) => None,
            Ok(nori_machines::Target::Remote(machine)) => Some(machine.clone()),
            Err(error) => return tool_error(error.to_string()),
        };
        let remote = target.is_some();
        let workspace = match target {
            None => {
                if let Err(error) =
                    self.enforce_path_sandbox(&cwd, &paths_named_by(name, &arguments))
                {
                    return tool_error(error);
                }
                self.shared.tooling.workspace(&self.shared.caller_key, &cwd)
            }
            Some(machine) => match self.machine_workspace(&roster, &machine, &cwd).await {
                Ok(workspace) => workspace,
                Err(error) => return tool_error(error),
            },
        };
        let result = workspace.call(name, arguments).await;
        let mut text = result.text.clone();
        if !remote
            && !result.is_error
            && !result.changes.is_empty()
            && self.diagnostics_after_edits()
        {
            let diagnostics = self.diagnostics_for(&cwd, &result.changes).await;
            if !diagnostics.is_empty() {
                text.push_str("\n\n");
                text.push_str(&diagnostics);
            }
        }
        let mut content = vec![Content::text(text)];
        content.extend(result.images.iter().map(|image| {
            Content::image(
                base64::engine::general_purpose::STANDARD.encode(&image.data),
                image.mime_type.clone(),
            )
        }));
        CallToolResult {
            content,
            structured_content: (!result.changes.is_empty()).then(|| result.structured()),
            is_error: Some(result.is_error),
            meta: None,
        }
    }

    /// The file tools' workspace on `machine`: through its own connection, rooted where the
    /// caller's last command there ended, else this project's path or the configured directory
    /// there when they exist, else its home. A restricted actor stays on this computer.
    async fn machine_workspace(
        &self,
        roster: &std::sync::Arc<nori_machines::Roster>,
        machine: &nori_machines::Machine,
        local_cwd: &Path,
    ) -> Result<nori_tools::Workspace, String> {
        let restricted = self
            .shared
            .orchestrator
            .seed()
            .scope
            .as_ref()
            .is_some_and(|restriction| restriction.sandbox.is_some());
        if restricted {
            return Err(format!(
                "this actor's file tools are restricted to this computer; they cannot reach {}",
                machine.name
            ));
        }
        let key = (self.shared.caller_key.clone(), machine.name.clone());
        if let Some(workspace) = self.shared.tooling.remote_workspace(&key) {
            return Ok(workspace);
        }
        let home = match roster.reach(&machine.name) {
            Some(nori_machines::Reach::Reachable(facts)) => facts.home,
            Some(nori_machines::Reach::Unreachable { .. }) | None => {
                roster
                    .probe(machine)
                    .await
                    .map_err(|error| format!("could not reach {}: {error}", machine.name))?
                    .home
            }
        };
        let home = if home.is_empty() {
            "/".to_owned()
        } else {
            home
        };
        let fs = nori_machines::MachineFs::new(std::sync::Arc::clone(roster), machine.clone());
        let previous = self.shared.remote_dirs.lock().await.get(&key).cloned();
        let candidates =
            previous
                .into_iter()
                .chain(
                    roster
                        .default_dirs(machine, local_cwd)
                        .into_iter()
                        .map(|dir| match dir {
                            nori_machines::RemoteDir::Home => home.clone(),
                            nori_machines::RemoteDir::Path(path) => match path.strip_prefix("~/") {
                                Some(rest) => format!("{home}/{rest}"),
                                None if path == "~" => home.clone(),
                                None => path,
                            },
                        }),
                );
        let mut remote_cwd = home.clone();
        for candidate in candidates {
            if fs
                .stat(&candidate)
                .await
                .is_ok_and(|stat| stat.kind == nori_machines::PathKind::Dir)
            {
                remote_cwd = candidate;
                break;
            }
        }
        let workspace = nori_tools::Workspace::new(
            remote_cwd,
            std::sync::Arc::new(crate::backend::machine_fs::MachineToolFs(fs)),
            nori_tools::WorkspaceConfig::default(),
        );
        let mut remote = self
            .shared
            .tooling
            .remote
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(remote.entry(key).or_insert(workspace).clone())
    }

    /// Whether edits are followed by diagnostics: yes, unless the session's policy turned the
    /// language server tools off.
    fn diagnostics_after_edits(&self) -> bool {
        self.shared.tool_selection.as_ref().is_none_or(|selection| {
            !selection.disabled.contains("lsp_diagnostics")
                && selection
                    .enabled
                    .as_ref()
                    .is_none_or(|enabled| enabled.contains("lsp_diagnostics"))
        })
    }

    /// Tell the servers about every changed file, wait for them together, and render what they
    /// report (empty when no server serves the files).
    async fn diagnostics_for(&self, cwd: &Path, changes: &[nori_tools::FileChange]) -> String {
        let lsp = self.shared.tooling.lsp(cwd);
        let mut written = Vec::new();
        for change in changes {
            match &change.new_text {
                Some(new_text) => {
                    if lsp
                        .notify_file_changed(&change.path, new_text.clone())
                        .await
                        .is_ok()
                    {
                        written.push(change.path.clone());
                    }
                }
                None => lsp.close_file(&change.path).await,
            }
        }
        let options = nori_lsp::RenderOptions::new(cwd);
        futures::future::join_all(written.iter().map(|path| lsp.diagnostics_after_edit(path)))
            .await
            .iter()
            .map(|report| nori_lsp::render_summary(report, &options))
            .filter(|summary| !summary.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A program may restrict an actor's tools to some paths; the file and language server
    /// tools honour that like `bash` does.
    pub(super) fn enforce_path_sandbox(&self, cwd: &Path, paths: &[String]) -> Result<(), String> {
        let sandbox = self
            .shared
            .orchestrator
            .seed()
            .scope
            .as_ref()
            .and_then(|restriction| restriction.sandbox.as_ref());
        for path in paths {
            nori_conduit::enforce_sandbox_path(sandbox, cwd, path)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

/// The paths a call names, for the path sandbox: the path arguments of the tools, and the file
/// headers of a patch.
pub(super) fn paths_named_by(name: &str, arguments: &Value) -> Vec<String> {
    let mut paths: Vec<String> = ["file_path", "filePath", "path"]
        .iter()
        .filter_map(|key| arguments.get(*key)?.as_str())
        .map(str::to_owned)
        .collect();
    let patch = arguments
        .get("input")
        .or_else(|| arguments.get("patch"))
        .and_then(Value::as_str)
        .or_else(|| arguments.as_str());
    if let Some(patch) = patch.filter(|_| name.contains("patch")) {
        paths.extend(patch.lines().filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .map(|path| path.trim().to_owned())
        }));
    }
    paths
}
