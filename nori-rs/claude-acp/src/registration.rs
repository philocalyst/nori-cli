//! How the host finds this agent, and what it is started with.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use nori_acp_kit::InProcessSpec;
use nori_acp_kit::Registration;
use nori_acp_kit::SpecOptions;
use nori_acp_kit::schema::McpServer;

use crate::agent::ClaudeAcpAgent;
use crate::mcp::HostToolsBinding;
use crate::mcp::SessionTools;
use crate::types::AgentConfig;

/// The options a `native = { agent = "claude" }` distribution accepts.
const OPTIONS: &[&str] = &["cli_path", "model"];

/// The driver's configuration for `spec`: the user's settings and environment, with the
/// spec's options on top. The `claude` to run is how an account is chosen.
fn config_for(spec: &InProcessSpec) -> std::io::Result<AgentConfig> {
    let options = SpecOptions::parse("Claude", &spec.config, OPTIONS)?;
    let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // The agent's own environment (an account's home, its launcher's exports) reaches the CLI,
    // never the process environment that every in-process agent shares, and it decides whose
    // settings are read.
    let environment: std::collections::BTreeMap<String, String> = spec
        .environment
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut config = AgentConfig::from_settings_or_env(
        &project_dir,
        crate::utils::claude_config_dir(&environment).as_deref(),
    );
    config.cli_path = options.get("cli_path").map(PathBuf::from);
    if let Some(model) = options.get("model") {
        config.model = Some(model.to_owned());
    }
    config.environment = environment;
    Ok(config)
}

/// The environment a custom adapter would have read, as the options the driver takes: which
/// `claude` to run (how an account is chosen) and the model.
fn resolve_environment(
    mut spec: InProcessSpec,
    environment: &HashMap<String, String>,
) -> InProcessSpec {
    if let Some(path) = environment
        .get("CLAUDE_CODE_EXECUTABLE")
        .filter(|value| !value.is_empty())
    {
        spec.config.push(format!("cli_path={path}"));
    }
    if let Some(model) = environment
        .get("ANTHROPIC_MODEL")
        .filter(|value| !value.is_empty())
    {
        spec.config.push(format!("model={model}"));
    }
    spec
}

/// Claude Code, started inside Nori's process. Claude Code takes about a second to start, so a
/// session is started before it is needed (see [`crate::session::warm`]).
pub fn registration() -> Registration {
    Registration::new("claude", |spec| async move {
        Ok(ClaudeAcpAgent::new(config_for(&spec)?))
    })
    .with_prewarm(
        |spec: InProcessSpec, cwd: PathBuf, mcp_servers: serde_json::Value| {
            let Ok(config) = config_for(&spec) else {
                return;
            };
            let tools = SessionTools {
                mcp_servers: serde_json::from_value::<Vec<McpServer>>(mcp_servers)
                    .unwrap_or_default(),
                host_tools: Some(Arc::new(HostToolsBinding::default())),
            };
            let key = crate::session::warm::WarmKey::new(&cwd, &config, &tools);
            crate::session::warm::refill(key, cwd, config, tools.mcp_servers);
        },
    )
    .with_resolver(resolve_environment)
}
