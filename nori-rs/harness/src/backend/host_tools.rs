//! Nori's own tools handed to an agent built into this process, with no MCP server between them.
//!
//! The tools are the `nori-client` server's `#[tool]` methods. Here they are listed from the same
//! router and called directly, so a built-in agent and an MCP client see the same tools, schemas
//! and replies. How a built-in agent finds them is described in [`nori_host_tools`].

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nori_host_tools::HostToolFuture;
use nori_host_tools::HostToolOutput;
use nori_host_tools::HostToolSpec;
use nori_host_tools::HostTools;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::nori_client_mcp::NoriClientService;
use super::nori_client_mcp::NoriClientShared;
use crate::connection::acp_connection::AcpConnection;

impl super::AcpBackend {
    pub(crate) fn bind_program_runtime(self: &Arc<Self>) {
        self.orchestrator
            .install_program_backend(Arc::downgrade(self));
    }

    pub(crate) async fn execute_unsafe_program(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
    ) -> Result<Value, std::io::Error> {
        use base64::Engine as _;
        let output = tokio::process::Command::new(program)
            .args(args)
            .current_dir(cwd.map(std::path::Path::new).unwrap_or(&self.cwd))
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(serde_json::json!({
            "success": output.status.success(),
            "code": output.status.code(),
            "status": output.status.to_string(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "stdout_base64": base64::engine::general_purpose::STANDARD.encode(&output.stdout),
            "stderr_base64": base64::engine::general_purpose::STANDARD.encode(&output.stderr),
        }))
    }

    pub(crate) async fn execute_host_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<HostToolOutput, String> {
        let tools = {
            let server = self.goal_mcp_http_server.lock().await;
            match server.as_ref() {
                Some(super::nori_client_mcp::NoriClient::Host { _registration }) => {
                    nori_host_tools::from_session_meta(Some(&_registration.session_meta()))
                }
                Some(super::nori_client_mcp::NoriClient::Mcp(server)) => {
                    Some(Arc::clone(&server.local_tools))
                }
                None => None,
            }
        }
        .ok_or_else(|| "this session has no direct host tool registration".to_owned())?;
        Ok(tools.call(name, arguments).await)
    }
}

/// Whether `connection` is to a built-in agent that takes host tools. Only an agent in this
/// process can resolve a registration token, so an agent elsewhere never gets one.
pub(super) fn accepted_by(connection: &AcpConnection) -> bool {
    connection.is_in_process() && nori_host_tools::advertised(connection.initialize_meta())
}

/// Publish what every session's tools look like, once, so a built-in agent can prepare sessions
/// before any exists (see [`nori_host_tools::Catalog`]).
pub(crate) fn publish_catalog() {
    static PUBLISHED: std::sync::Once = std::sync::Once::new();
    PUBLISHED.call_once(|| {
        nori_host_tools::publish_catalog(nori_host_tools::Catalog {
            specs: specs(),
            instructions: super::nori_client_mcp::tool_instructions(),
        });
    });
}

/// Register `shared`'s tools and have every later session request on `connection` carry them.
/// The tools are usable as soon as the agent has the session, so they count as connected.
pub(super) fn hand_to(
    connection: &AcpConnection,
    shared: NoriClientShared,
    connected: &AtomicBool,
) -> nori_host_tools::Registration {
    publish_catalog();
    let registration = nori_host_tools::register(from_shared(shared));
    connection.set_session_meta(Some(registration.session_meta()));
    connected.store(true, Ordering::Relaxed);
    registration
}

pub(super) fn from_shared(shared: NoriClientShared) -> Arc<dyn HostTools> {
    let selection = shared.tool_selection.clone();
    let tools: Arc<dyn HostTools> = Arc::new(NoriHostTools {
        service: NoriClientService::new(shared),
        owner_session: std::sync::Mutex::new(None),
    });
    match selection {
        Some(selection) => Arc::new(nori_host_tools::SelectedTools::new(tools, &selection)),
        None => tools,
    }
}

struct NoriHostTools {
    service: NoriClientService,
    owner_session: std::sync::Mutex<Option<String>>,
}

impl HostTools for NoriHostTools {
    fn bind_session_id(&self, session_id: &str) {
        let mut owner = self
            .owner_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.is_none() {
            *owner = Some(session_id.to_owned());
        }
    }

    fn instructions(&self) -> String {
        self.service.instructions()
    }

    fn specs(&self) -> Vec<HostToolSpec> {
        specs_from(self.service.session_tools())
    }

    fn call(&self, name: &str, arguments: Value) -> HostToolFuture {
        let service = self.service.clone();
        let available = service.has_tool(name);
        let name = name.to_owned();
        Box::pin(async move {
            if !available {
                return HostToolOutput::error(format!("there is no Nori tool named {name}"));
            }
            match dispatch(&service, &name, arguments).await {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(result) => HostToolOutput::from_mcp_result(result),
                    Err(error) => {
                        HostToolOutput::error(format!("could not serialize tool result: {error}"))
                    }
                },
                Err(message) => HostToolOutput::error(message),
            }
        })
    }

    fn call_scoped(&self, name: &str, arguments: Value, session_id: &str) -> HostToolFuture {
        let owner = self
            .owner_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let eligible = owner.as_deref() == Some(session_id) && self.service.has_tool(name);
        drop(owner);
        let service = self.service.clone();
        let session_id = session_id.to_owned();
        let name = name.to_owned();
        Box::pin(async move {
            let service = match service.for_caller(Some(&session_id), &name).await {
                Ok(service) => service,
                Err(error) => return HostToolOutput::error(error.to_string()),
            };
            if !service.has_tool(&name) {
                return HostToolOutput::error(format!("there is no Nori tool named {name}"));
            }
            let mut output = match dispatch(&service, &name, arguments).await {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(result) => HostToolOutput::from_mcp_result(result),
                    Err(error) => HostToolOutput::error(error.to_string()),
                },
                Err(error) => HostToolOutput::error(error),
            };
            if eligible && let Some(context) = service.take_goal_context().await {
                output.model_context = Some(context);
            }
            output
        })
    }
}

/// Every tool as a host tool spec, in a stable order: that keeps the agent's tool list, and so
/// its prompt cache, the same across sessions.
fn specs() -> Vec<HostToolSpec> {
    specs_from(NoriClientService::all_tools())
}

fn specs_from(mut tools: Vec<rmcp::model::Tool>) -> Vec<HostToolSpec> {
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
        .into_iter()
        .map(|tool| HostToolSpec {
            name: tool.name.to_string(),
            description: tool.description.map(String::from).unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
            annotations: tool
                .annotations
                .and_then(|annotations| serde_json::to_value(annotations).ok()),
            meta: tool.meta.map(|meta| meta.0),
        })
        .collect()
}

/// Call the tool `name` as the MCP server would. Every tool the router lists has an arm here;
/// a test holds the two together.
async fn dispatch(
    service: &NoriClientService,
    name: &str,
    arguments: Value,
) -> Result<CallToolResult, String> {
    let result = match name {
        "bash" => service.bash(parameters(arguments)?).await,
        "conversation" => service.conversation(parameters(arguments)?).await,
        "conversation_context" => service.conversation_context(parameters(arguments)?).await,
        "get_goal" => {
            no_arguments(name, &arguments)?;
            service.get_goal().await
        }
        "create_goal" => service.create_goal(parameters(arguments)?).await,
        "update_goal" => service.update_goal(parameters(arguments)?).await,
        "git_status" => {
            no_arguments(name, &arguments)?;
            service.git_status().await
        }
        "git_diff" => service.git_diff(parameters(arguments)?).await,
        "git_log" => service.git_log(parameters(arguments)?).await,
        "git_commit" => service.git_commit(parameters(arguments)?).await,
        "ci_status" => service.ci_status(parameters(arguments)?).await,
        "pull_request" => service.pull_request(parameters(arguments)?).await,
        "monitor_start" => service.monitor_start(parameters(arguments)?).await,
        "monitor_list" => {
            no_arguments(name, &arguments)?;
            service.monitor_list().await
        }
        "monitor_stop" => service.monitor_stop(parameters(arguments)?).await,
        "monitor_rearm" => service.monitor_rearm(parameters(arguments)?).await,
        "agent_capabilities" => {
            no_arguments(name, &arguments)?;
            service.agent_capabilities().await
        }
        "delegate_task" => service.delegate_task(parameters(arguments)?).await,
        "task_status" => service.task_status(parameters(arguments)?).await,
        "task_wait" => service.task_wait(parameters(arguments)?).await,
        "task_send" => service.task_send(parameters(arguments)?).await,
        "task_cancel" => service.task_cancel(parameters(arguments)?).await,
        "task_list" => {
            no_arguments(name, &arguments)?;
            service.task_list().await
        }
        _ => return Err(format!("there is no Nori tool named {name}")),
    };
    result.map_err(|error| error.message.into_owned())
}

/// A tool's arguments, as rmcp would parse them. A model that sends no arguments at all means an
/// empty object.
fn parameters<T: DeserializeOwned>(arguments: Value) -> Result<Parameters<T>, String> {
    let arguments = if arguments.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        arguments
    };
    serde_json::from_value(arguments)
        .map(Parameters)
        .map_err(|error| format!("invalid arguments: {error}"))
}

fn no_arguments(name: &str, arguments: &Value) -> Result<(), String> {
    match arguments {
        Value::Null => Ok(()),
        Value::Object(map) if map.is_empty() => Ok(()),
        _ => Err(format!("invalid arguments: {name} takes none")),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;

    use super::*;
    use crate::backend::nori_client_mcp::Workspace;
    use crate::backend::orchestrator::Orchestrator;
    use crate::backend::orchestrator::Seed;
    use crate::backend::thread_goal::ThreadGoalState;

    fn tools(cwd: Option<std::path::PathBuf>) -> NoriHostTools {
        tools_with_selection(cwd, None)
    }

    #[tokio::test]
    async fn goal_delta_on_host_results_is_root_scoped_and_once_per_revision() {
        let tools = nori_host_tools::SelectedTools::new(Arc::new(tools(None)), &Default::default());
        tools.bind_session_id("root");
        tools
            .call("create_goal", json!({"objective":"ship it"}))
            .await;
        let child = tools.call_scoped("task_list", json!({}), "child").await;
        assert_eq!(child.model_context, None);
        let root = tools.call_scoped("task_list", json!({}), "root").await;
        assert!(
            root.model_context
                .as_deref()
                .is_some_and(|text| text.contains("ship it"))
        );
        assert!(
            child.is_error,
            "an unobserved caller must not execute root tools"
        );
        assert!(!root.is_error);
        let unchanged = tools.call_scoped("task_list", json!({}), "root").await;
        assert_eq!(unchanged.model_context, None);
        tools
            .call("update_goal", json!({"status":"complete"}))
            .await;
        let updated = tools.call_scoped("task_list", json!({}), "root").await;
        assert!(
            updated
                .model_context
                .as_deref()
                .is_some_and(|text| text.contains("Revision: 2"))
        );
    }

    fn tools_with_selection(
        cwd: Option<std::path::PathBuf>,
        selection: Option<nori_host_tools::ToolSelection>,
    ) -> NoriHostTools {
        let (events, _events_rx) = mpsc::channel(8);
        let (orchestrator, _signals) = Orchestrator::new(
            Seed::root(
                Arc::new(crate::config::NoriConfig::default()),
                "test".to_owned(),
            ),
            events.clone(),
        );
        orchestrator.bind(crate::backend::orchestrator::Binding {
            session_id: "root".into(),
            approval_policy: tokio::sync::watch::channel(nori_config::AskForApproval::Never).1,
        });
        let shared = NoriClientShared::new(
            Arc::new(Mutex::new(ThreadGoalState::default())),
            events,
            Arc::new(AtomicBool::new(false)),
            crate::normalized::AgentCapabilitiesView {
                http_mcp: false,
                load_session: false,
                session_list: false,
                session_resume: false,
                session_close: false,
                session_fork: false,
            },
            true,
            false,
            orchestrator,
        );
        let mut shared = match cwd {
            Some(cwd) => shared.with_workspace(Workspace {
                cwd,
                workflow: Arc::default(),
            }),
            None => shared,
        };
        shared.tool_selection = selection;
        NoriHostTools {
            service: NoriClientService::new(shared),
            owner_session: std::sync::Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn selected_catalog_and_dispatch_share_the_same_session_policy() {
        let selection = nori_host_tools::ToolSelection {
            enabled: Some(["get_goal".to_owned()].into_iter().collect()),
            descriptions: [("get_goal".to_owned(), "Read our chosen goal.".to_owned())]
                .into_iter()
                .collect(),
            instructions: vec!["Use this policy.".to_owned()],
            ..Default::default()
        };
        let native: Arc<dyn HostTools> =
            Arc::new(tools_with_selection(None, Some(selection.clone())));
        let selected = nori_host_tools::SelectedTools::new(native, &selection);
        assert_eq!(
            selected
                .specs()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["get_goal"]
        );
        assert_eq!(selected.specs()[0].description, "Read our chosen goal.");
        assert!(selected.instructions().contains("Use this policy."));
        assert!(
            selected
                .call("create_goal", json!({"objective": "must not run"}))
                .await
                .is_error
        );
        assert_eq!(
            selected.call("get_goal", json!({})).await.text,
            json!({"goal": null}).to_string()
        );
        let mcp = tools_with_selection(None, Some(selection));
        assert_eq!(
            mcp.service
                .session_tools()
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["get_goal"]
        );
        assert!(!mcp.service.has_tool("create_goal"));
    }

    #[test]
    fn selected_cross_provider_tools_include_discovery_guidance() {
        let selection = nori_host_tools::ToolSelection {
            enabled: Some(
                ["agent_capabilities".to_owned(), "delegate_task".to_owned()]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        };
        let native: Arc<dyn HostTools> =
            Arc::new(tools_with_selection(None, Some(selection.clone())));
        let selected = nori_host_tools::SelectedTools::new(native, &selection);
        assert!(
            selected
                .instructions()
                .contains(nori_host_tools::DELEGATION_INSTRUCTIONS)
        );
        let limited = nori_host_tools::ToolSelection {
            disabled: ["delegate_task".to_owned()].into_iter().collect(),
            ..selection
        };
        let native: Arc<dyn HostTools> =
            Arc::new(tools_with_selection(None, Some(limited.clone())));
        let selected = nori_host_tools::SelectedTools::new(native, &limited);
        assert!(
            !selected
                .instructions()
                .contains(nori_host_tools::DELEGATION_INSTRUCTIONS)
        );
    }

    #[test]
    fn goal_continuations_require_both_selected_goal_tools() {
        let all: Arc<dyn HostTools> = Arc::new(tools(None));
        let full = super::super::nori_client_mcp::NoriClient::Host {
            _registration: nori_host_tools::register(Arc::clone(&all)),
        };
        assert!(full.has_goal_tools());
        let selection = nori_host_tools::ToolSelection {
            disabled: ["update_goal".to_owned()].into_iter().collect(),
            ..Default::default()
        };
        let selected = Arc::new(nori_host_tools::SelectedTools::new(all, &selection));
        let limited = super::super::nori_client_mcp::NoriClient::Host {
            _registration: nori_host_tools::register(selected),
        };
        assert!(!limited.has_goal_tools());
    }

    #[test]
    fn the_tools_are_the_mcp_servers_in_a_stable_order() {
        let names: Vec<String> = tools(None)
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "agent_capabilities",
                "bash",
                "conversation",
                "conversation_context",
                "create_goal",
                "delegate_task",
                "get_goal",
                "monitor_list",
                "monitor_rearm",
                "monitor_start",
                "monitor_stop",
                "task_cancel",
                "task_list",
                "task_send",
                "task_status",
                "task_wait",
                "update_goal",
            ]
        );
    }

    #[test]
    fn every_spec_has_an_object_schema_and_delegation_stays_loaded() {
        for spec in tools(None).specs() {
            assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
            assert!(!spec.description.is_empty(), "{}", spec.name);
        }
        let delegate = tools(None)
            .specs()
            .into_iter()
            .find(|spec| spec.name == "delegate_task")
            .expect("delegate_task");
        assert_eq!(
            delegate
                .meta
                .and_then(|meta| meta.get("anthropic/alwaysLoad").cloned()),
            Some(json!(true))
        );
    }

    /// Every listed tool reaches its own arm: given an argument no tool takes, each one rejects
    /// it as an argument error rather than as an unknown tool, and nothing runs.
    #[tokio::test]
    async fn every_listed_tool_is_dispatched() {
        let tools = tools(None);
        for spec in tools.specs() {
            let output = tools
                .call(&spec.name, json!({"no_such_argument": true}))
                .await;
            assert!(output.is_error, "{}", spec.name);
            assert!(
                output.text.starts_with("invalid arguments"),
                "{}: {}",
                spec.name,
                output.text
            );
        }
        let unknown = tools.call("rm_rf", json!({})).await;
        assert_eq!(
            unknown,
            HostToolOutput::error("there is no Nori tool named rm_rf")
        );
    }

    #[tokio::test]
    async fn a_goal_round_trips_through_direct_calls() {
        let tools = tools(None);
        let initial = tools.call("get_goal", Value::Null).await;
        assert_eq!(initial.text, json!({"goal": null}).to_string());
        assert!(!initial.is_error);
        let created = tools
            .call("create_goal", json!({"objective": "ship host tools"}))
            .await;
        assert!(!created.is_error, "{}", created.text);
        let goal: Value = serde_json::from_str(&created.text).expect("json");
        assert_eq!(goal["goal"]["objective"], "ship host tools");
        assert_eq!(goal["goal"]["status"], "active");

        let again = tools
            .call("create_goal", json!({"objective": "another"}))
            .await;
        assert!(again.is_error);
    }

    #[tokio::test]
    async fn git_status_is_excluded_by_default_and_callable_when_opted_in() {
        let default_output = tools(None).call("git_status", json!({})).await;
        assert!(default_output.is_error);
        assert_eq!(
            default_output.text,
            "there is no Nori tool named git_status"
        );

        let selection = nori_host_tools::ToolSelection {
            enabled: Some(["git_status".to_owned()].into_iter().collect()),
            ..Default::default()
        };
        let opted_in = tools_with_selection(None, Some(selection));
        let output = opted_in.call("git_status", json!({})).await;
        assert_eq!(output.text, "this session has no working directory");
        assert!(output.is_error);
        assert_eq!(output.as_mcp_result()["isError"], true);
    }

    #[tokio::test]
    async fn bash_host_tool_persists_state_across_calls() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");

        let tools = tools(Some(root));
        let first = tools
            .call(
                "bash",
                json!({"command": "export NORI_HOST_VAR=ok; hi() { echo $NORI_HOST_VAR; }; cd sub"}),
            )
            .await;
        assert!(!first.is_error, "{}", first.text);

        let second = tools.call("bash", json!({"command": "hi && pwd"})).await;
        assert!(!second.is_error, "{}", second.text);
        let parsed: Value = serde_json::from_str(&second.text).expect("json");
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["stdout"], format!("ok\n{}\n", sub.display()));
    }
}
