//! Codex's `config.toml`, layered as Codex layers it, for one working directory.
//!
//! Later layers win: `$CODEX_HOME/config.toml`; then, for a trusted project only, every
//! `.codex/config.toml` from the project root down to the working directory; then the selected
//! `profile`'s table (`[profiles.<name>]`); then the account's `key=value` overrides (`codex -c`).
//! A project is trusted when `[projects."<path>"] trust_level = "trusted"` names the working
//! directory or its project root. Only the keys the engine uses are read; Codex's sandbox and
//! approval keys are not (Nori's modes decide permissions).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use codex_config::types::AuthCredentialsStoreMode;
use codex_model_provider_info::ModelProviderInfo;
use nori_acp_kit::schema::EnvVariable;
use nori_acp_kit::schema::HttpHeader;
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::McpServerHttp;
use nori_acp_kit::schema::McpServerStdio;
use toml::Value;

const CONFIG_FILE: &str = "config.toml";
/// Codex's default budget for project instructions (`project_doc_max_bytes`).
const DEFAULT_PROJECT_DOC_MAX_BYTES: i64 = 32 * 1024;

/// Whether the project's own configuration and instructions may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trust {
    Trusted,
    Untrusted,
    /// No decision recorded: project configuration is not loaded, instructions are.
    Unknown,
}

/// The effective configuration for one working directory.
#[derive(Debug, Clone)]
pub(crate) struct CodexConfig {
    effective: Value,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) trust: Trust,
    pub(crate) warnings: Vec<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            effective: Value::Table(toml::Table::new()),
            project_root: None,
            trust: Trust::Unknown,
            warnings: Vec::new(),
        }
    }
}

/// `key=value` overrides as `codex -c` takes them: the value is TOML, else a plain string.
pub(crate) fn parse_overrides(raw: &[String]) -> Result<Vec<(String, Value)>, String> {
    raw.iter()
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| format!("invalid Codex override {pair:?}: expected key=value"))?;
            let value = value.trim();
            let parsed = toml::from_str::<toml::Table>(&format!("value = {value}"))
                .ok()
                .and_then(|mut table| table.remove("value"))
                .unwrap_or_else(|| Value::String(value.trim_matches('"').to_owned()));
            Ok((key.trim().to_owned(), parsed))
        })
        .collect()
}

fn set_path(root: &mut Value, key: &str, value: Value) {
    let mut cursor = root;
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    for part in parents {
        let Value::Table(table) = cursor else {
            return;
        };
        let entry = table
            .entry((*part).to_owned())
            .or_insert_with(|| Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = Value::Table(toml::Table::new());
        }
        cursor = entry;
    }
    if let Value::Table(table) = cursor {
        table.insert((*last).to_owned(), value);
    }
}

fn read(path: &Path, warnings: &mut Vec<String>) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<toml::Table>(&text) {
        Ok(table) => Some(Value::Table(table)),
        Err(error) => {
            warnings.push(format!("{} was not loaded: {error}", path.display()));
            None
        }
    }
}

impl CodexConfig {
    /// Read the configuration for `cwd` (or none, for what does not depend on a directory).
    /// Blocking: call it off the async threads.
    pub(crate) fn load(home: &Path, cwd: Option<&Path>, overrides: &[(String, Value)]) -> Self {
        let mut warnings = Vec::new();
        let mut effective = Value::Table(toml::Table::new());
        if let Some(user) = read(&home.join(CONFIG_FILE), &mut warnings) {
            codex_config::merge_toml_values(&mut effective, &user);
        }
        let apply_overrides = |effective: &mut Value| {
            for (key, value) in overrides {
                set_path(effective, key, value.clone());
            }
        };
        apply_overrides(&mut effective);
        let mut project_root = None;
        let mut trust = Trust::Unknown;
        if let Some(cwd) = cwd {
            let markers: Vec<String> = effective
                .get("project_root_markers")
                .and_then(Value::as_array)
                .map(|markers| {
                    markers
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_else(|| vec![".git".to_owned()]);
            project_root = cwd
                .ancestors()
                .find(|directory| markers.iter().any(|marker| directory.join(marker).exists()))
                .map(Path::to_path_buf);
            trust = trust_for(&effective, cwd, project_root.as_deref());
            let top = project_root.clone().unwrap_or_else(|| cwd.to_path_buf());
            let mut layers: Vec<PathBuf> = cwd
                .ancestors()
                .take_while(|directory| directory.starts_with(&top))
                .map(|directory| directory.join(".codex").join(CONFIG_FILE))
                .filter(|path| path.is_file() && path.parent() != Some(home))
                .collect();
            layers.reverse();
            if trust == Trust::Trusted {
                for layer in &layers {
                    if let Some(table) = read(layer, &mut warnings) {
                        codex_config::merge_toml_values(&mut effective, &table);
                    }
                }
                apply_overrides(&mut effective);
            } else if let Some(layer) = layers.first() {
                warnings.push(format!(
                    "{} was not loaded: add {} as a trusted project in {}",
                    layer.display(),
                    top.display(),
                    home.join(CONFIG_FILE).display()
                ));
            }
        }
        let profile = effective
            .get("profile")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(name) = profile {
            match effective
                .get("profiles")
                .and_then(|profiles| profiles.get(&name))
                .cloned()
            {
                Some(table) => {
                    codex_config::merge_toml_values(&mut effective, &table);
                    apply_overrides(&mut effective);
                }
                None => warnings.push(format!(
                    "the profile {name:?} is not defined in config.toml"
                )),
            }
        }
        Self {
            effective,
            project_root,
            trust,
            warnings,
        }
    }

    fn string(&self, key: &str) -> Option<String> {
        self.effective
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    pub(crate) fn model(&self) -> Option<String> {
        self.string("model")
    }

    pub(crate) fn effort(&self) -> Option<String> {
        self.string("model_reasoning_effort")
    }

    pub(crate) fn summary(&self) -> Option<String> {
        self.string("model_reasoning_summary")
    }

    pub(crate) fn verbosity(&self) -> Option<String> {
        self.string("model_verbosity")
    }

    /// The service tier asked for (`fast` is Codex's old name for `priority`).
    pub(crate) fn service_tier(&self) -> Option<String> {
        self.string("service_tier").map(|tier| {
            if tier == "fast" {
                "priority".to_owned()
            } else {
                tier
            }
        })
    }

    pub(crate) fn provider_id(&self) -> String {
        self.string("model_provider")
            .unwrap_or_else(|| codex_model_provider_info::OPENAI_PROVIDER_ID.to_owned())
    }

    /// The built-in providers with the configured ones merged in, as Codex merges them.
    pub(crate) fn providers(&self) -> Result<HashMap<String, ModelProviderInfo>, String> {
        let built_in =
            codex_model_provider_info::built_in_model_providers(self.string("openai_base_url"));
        let configured: HashMap<String, ModelProviderInfo> =
            match self.effective.get("model_providers") {
                Some(table) => table
                    .clone()
                    .try_into()
                    .map_err(|error| format!("model_providers in config.toml: {error}"))?,
                None => HashMap::new(),
            };
        codex_model_provider_info::merge_configured_model_providers(built_in, configured)
    }

    pub(crate) fn project_doc_max_bytes(&self) -> i64 {
        self.effective
            .get("project_doc_max_bytes")
            .and_then(Value::as_integer)
            .unwrap_or(DEFAULT_PROJECT_DOC_MAX_BYTES)
    }

    pub(crate) fn project_doc_fallback_filenames(&self) -> Vec<String> {
        self.effective
            .get("project_doc_fallback_filenames")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A file that replaces the model's own base instructions (`model_instructions_file`).
    pub(crate) fn instructions_file(&self, home: &Path) -> Option<PathBuf> {
        self.string("model_instructions_file")
            .map(|file| home.join(file))
    }

    pub(crate) fn store_mode(&self) -> AuthCredentialsStoreMode {
        self.effective
            .get("cli_auth_credentials_store")
            .cloned()
            .and_then(|value| value.try_into().ok())
            .unwrap_or_default()
    }

    pub(crate) fn forced_workspaces(&self) -> Option<Vec<String>> {
        match self.effective.get("forced_chatgpt_workspace_id")? {
            Value::String(id) => Some(vec![id.clone()]),
            Value::Array(ids) => Some(
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect(),
            ),
            _ => None,
        }
    }

    pub(crate) fn chatgpt_base_url(&self) -> Option<String> {
        self.string("chatgpt_base_url")
    }

    /// The MCP servers `[mcp_servers.<name>]` configures and leaves enabled: a `command` (with
    /// `args` and `env`) runs one, a `url` (with `http_headers`, `env_http_headers` and
    /// `bearer_token_env_var`) reaches one.
    pub(crate) fn mcp_servers(&self, env: &super::env::Env) -> Vec<McpServer> {
        let strings = |value: Option<&Value>| -> Vec<(String, String)> {
            value
                .and_then(Value::as_table)
                .into_iter()
                .flatten()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        };
        let Some(servers) = self.effective.get("mcp_servers").and_then(Value::as_table) else {
            return Vec::new();
        };
        servers
            .iter()
            .filter(|(_, server)| server.get("enabled").and_then(Value::as_bool) != Some(false))
            .filter_map(|(name, server)| {
                if let Some(command) = server.get("command").and_then(Value::as_str) {
                    let args = server
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|args| {
                            args.iter()
                                .filter_map(Value::as_str)
                                .map(ToOwned::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    let variables = strings(server.get("env"))
                        .into_iter()
                        .map(|(key, value)| EnvVariable::new(key, value))
                        .collect();
                    return Some(McpServer::Stdio(
                        McpServerStdio::new(name.clone(), command.to_owned())
                            .args(args)
                            .env(variables),
                    ));
                }
                let url = server.get("url").and_then(Value::as_str)?;
                let mut headers: Vec<HttpHeader> = strings(server.get("http_headers"))
                    .into_iter()
                    .map(|(key, value)| HttpHeader::new(key, value))
                    .collect();
                headers.extend(
                    strings(server.get("env_http_headers"))
                        .into_iter()
                        .filter_map(|(key, variable)| {
                            Some(HttpHeader::new(key, env.get(&variable)?))
                        }),
                );
                if let Some(token) = server
                    .get("bearer_token_env_var")
                    .and_then(Value::as_str)
                    .and_then(|variable| env.get(variable))
                {
                    headers.push(HttpHeader::new("Authorization", format!("Bearer {token}")));
                }
                Some(McpServer::Http(
                    McpServerHttp::new(name.clone(), url.to_owned()).headers(headers),
                ))
            })
            .collect()
    }
}

/// The trust recorded for `cwd`: its own entry first, then its project root's.
fn trust_for(effective: &Value, cwd: &Path, root: Option<&Path>) -> Trust {
    let Some(projects) = effective.get("projects").and_then(Value::as_table) else {
        return Trust::Unknown;
    };
    let candidates = [Some(cwd), root].into_iter().flatten().flat_map(|path| {
        [
            path.to_path_buf(),
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        ]
    });
    for path in candidates {
        let level = projects
            .get(path.to_string_lossy().as_ref())
            .and_then(|project| project.get("trust_level"))
            .and_then(Value::as_str);
        match level {
            Some("trusted") => return Trust::Trusted,
            Some("untrusted") => return Trust::Untrusted,
            Some(_) | None => {}
        }
    }
    Trust::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn layers_profile_and_overrides_stack_as_codex_stacks_them() {
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let work = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let repo = work.path().join("repo");
        let sub = repo.join("sub");
        std::fs::create_dir_all(sub.join(".codex")).unwrap_or_else(|error| panic!("{error}"));
        std::fs::create_dir_all(repo.join(".git")).unwrap_or_else(|error| panic!("{error}"));
        let user = format!(
            "model = \"user\"\nmodel_reasoning_effort = \"low\"\nprofile = \"deep\"\n[profiles.deep]\nmodel_reasoning_effort = \"high\"\n[projects.{:?}]\ntrust_level = \"trusted\"\n",
            repo.display().to_string()
        );
        std::fs::write(home.path().join("config.toml"), user)
            .unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(
            sub.join(".codex/config.toml"),
            "model = \"project\"\nservice_tier = \"fast\"\n",
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let overrides =
            parse_overrides(&["model_verbosity=\"low\"".to_owned()]).unwrap_or_default();
        let config = CodexConfig::load(home.path(), Some(&sub), &overrides);
        assert_eq!(
            (
                config.model(),
                config.effort(),
                config.service_tier(),
                config.verbosity(),
                config.trust,
                config.project_root.clone()
            ),
            (
                Some("project".to_owned()),
                Some("high".to_owned()),
                Some("priority".to_owned()),
                Some("low".to_owned()),
                Trust::Trusted,
                Some(repo.clone())
            )
        );

        // Untrusted: the project's file is named in a warning and not read.
        std::fs::write(home.path().join("config.toml"), "model = \"user\"\n")
            .unwrap_or_else(|error| panic!("{error}"));
        let config = CodexConfig::load(home.path(), Some(&sub), &[]);
        assert_eq!(
            (config.model(), config.trust, config.warnings.len()),
            (Some("user".to_owned()), Trust::Unknown, 1)
        );
    }
}
