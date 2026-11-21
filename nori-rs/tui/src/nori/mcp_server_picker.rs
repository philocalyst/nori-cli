//! The `/mcp` picker: the configured MCP servers as a choice list, and adding a server as a chain
//! of questions. It is a [`ChoiceSpec`]: the list toggles a server on Enter, logs in on `l` and
//! deletes on `d` (after a confirmation); "Add new…" asks what the server is one question at a
//! time; auth statuses and OAuth results arrive as notices and refresh the list.

use std::any::Any;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_rmcp_client::McpAuthStatus;
use nori_config::McpServerConfig;
use nori_config::McpServerTransportConfig;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::choice_picker::Ask;
use crate::choice_picker::ChoiceItem;
use crate::choice_picker::ChoiceSpec;
use crate::choice_picker::Next;

/// The servers, what is known of their auth, and the server whose OAuth login starts once its
/// auth status is known.
struct Servers {
    list: Vec<(String, McpServerConfig)>,
    auth: HashMap<String, McpAuthStatus>,
    login_when_known: Option<String>,
}

type Shared = Arc<Mutex<Servers>>;

/// What the user has said about the server being added.
#[derive(Default)]
struct Draft {
    name: String,
    http: bool,
    command: String,
    args: String,
    url: String,
    env: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    bearer_env_var: String,
    client_id: String,
}

type Wizard = Arc<Mutex<Draft>>;

const KEYS: &[(char, &str)] = &[('d', "delete"), ('l', "login")];

fn lock<T>(shared: &Arc<Mutex<T>>) -> std::sync::MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The `/mcp` picker over `servers`.
pub(crate) fn mcp_choices(servers: &BTreeMap<String, McpServerConfig>) -> ChoiceSpec {
    let shared: Shared = Arc::new(Mutex::new(Servers {
        list: servers
            .iter()
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect(),
        auth: HashMap::new(),
        login_when_known: None,
    }));
    list(&shared, 0)
}

fn save(servers: &Servers, tx: &AppEventSender) {
    tx.send(AppEvent::SaveMcpServers(
        servers.list.iter().cloned().collect(),
    ));
}

fn auth_note(config: &McpServerConfig, status: Option<McpAuthStatus>) -> &'static str {
    match (&config.transport, status) {
        (McpServerTransportConfig::StreamableHttp { .. }, Some(McpAuthStatus::NotLoggedIn)) => {
            " · not logged in"
        }
        (McpServerTransportConfig::StreamableHttp { .. }, Some(McpAuthStatus::OAuth)) => {
            " · logged in"
        }
        (McpServerTransportConfig::StreamableHttp { .. }, Some(McpAuthStatus::BearerToken)) => {
            " · bearer token"
        }
        _ => "",
    }
}

fn list(shared: &Shared, selected: usize) -> ChoiceSpec {
    let servers = lock(shared);
    let mut items = vec![ChoiceItem::step("+ Add new…", {
        let shared = shared.clone();
        Arc::new(move |_| Next::Show(transport(&shared)))
    })];
    for (index, (name, config)) in servers.list.iter().enumerate() {
        let kind = match config.transport {
            McpServerTransportConfig::Stdio { .. } => "stdio",
            McpServerTransportConfig::StreamableHttp { .. } => "http",
        };
        let state = if config.enabled { "on" } else { "off" };
        let note = auth_note(config, servers.auth.get(name).copied());
        let at = index + 1;
        let toggle = {
            let shared = shared.clone();
            Arc::new(move |tx: &AppEventSender| {
                let mut servers = lock(&shared);
                if let Some((_, config)) = servers.list.get_mut(index) {
                    config.enabled = !config.enabled;
                }
                save(&servers, tx);
                drop(servers);
                Next::Show(list(&shared, at))
            })
        };
        let delete = {
            let shared = shared.clone();
            Arc::new(move |_: &AppEventSender| Next::Show(confirm_delete(&shared, index)))
        };
        let login = {
            let shared = shared.clone();
            Arc::new(move |tx: &AppEventSender| {
                let target = {
                    let servers = lock(&shared);
                    servers.list.get(index).and_then(|(name, config)| {
                        let logged_out =
                            servers.auth.get(name) == Some(&McpAuthStatus::NotLoggedIn);
                        logged_out.then(|| (name.clone(), config.clone()))
                    })
                };
                match target.and_then(|(name, config)| start_login(&name, &config, tx)) {
                    Some(name) => Next::Show(authenticating(&shared, &name)),
                    None => Next::Show(list(&shared, at)),
                }
            })
        };
        items.push(
            ChoiceItem::step(name.clone(), toggle)
                .detail(format!("{kind} · {state}{note}"))
                .key("delete", delete)
                .key("login", login),
        );
    }
    let mut spec = ChoiceSpec::new("MCP servers", items);
    spec.subtitle = Some("Manage MCP server connections".to_owned());
    spec.searchable = false;
    spec.hint = "enter toggle · l login · d delete".to_owned();
    spec.keys = KEYS;
    spec.initial = Some(selected.min(servers.list.len()));
    spec.on_notice = Some(notices(shared));
    spec
}

/// Start the OAuth login of an HTTP server; the server's name when one started.
fn start_login(name: &str, config: &McpServerConfig, tx: &AppEventSender) -> Option<String> {
    let McpServerTransportConfig::StreamableHttp {
        url,
        http_headers,
        env_http_headers,
        client_id,
        client_secret_env_var,
        ..
    } = &config.transport
    else {
        return None;
    };
    tx.send(AppEvent::McpOAuthLogin {
        server_name: name.to_owned(),
        server_url: url.clone(),
        http_headers: http_headers.clone(),
        env_http_headers: env_http_headers.clone(),
        client_id: client_id.clone(),
        client_secret_env_var: client_secret_env_var.clone(),
    });
    Some(name.to_owned())
}

/// What the app tells the picker: auth statuses (which may start a just-added server's login)
/// and OAuth results.
fn notices(shared: &Shared) -> crate::choice_picker::NoticeAction {
    let shared = shared.clone();
    Arc::new(move |notice: &dyn Any, tx: &AppEventSender| {
        if let Some(statuses) = notice.downcast_ref::<HashMap<String, McpAuthStatus>>() {
            let login = {
                let mut servers = lock(&shared);
                servers.auth.clone_from(statuses);
                servers.login_when_known.take().and_then(|name| {
                    let logged_out = statuses.get(&name) == Some(&McpAuthStatus::NotLoggedIn);
                    let config = servers
                        .list
                        .iter()
                        .find(|(candidate, _)| *candidate == name)
                        .map(|(_, config)| config.clone());
                    config
                        .filter(|_| logged_out)
                        .and_then(|config| start_login(&name, &config, tx))
                })
            };
            return Some(match login {
                Some(name) => Next::Show(authenticating(&shared, &name)),
                None => Next::Show(list(&shared, 0)),
            });
        }
        notice
            .downcast_ref::<(String, bool)>()
            .map(|_| Next::Show(list(&shared, 0)))
    })
}

fn authenticating(shared: &Shared, name: &str) -> ChoiceSpec {
    let mut item = ChoiceItem::step(
        format!("Authenticating `{name}`…"),
        Arc::new(|_| Next::Done),
    );
    item.disabled = true;
    let mut spec = ChoiceSpec::new("MCP servers", vec![item]);
    spec.subtitle = Some("Finish signing in in your browser, or press esc to cancel".to_owned());
    spec.searchable = false;
    spec.on_notice = Some(notices(shared));
    spec.on_back = Some({
        let (shared, name) = (shared.clone(), name.to_owned());
        Arc::new(move |tx| {
            tx.send(AppEvent::McpOAuthLoginCancel {
                server_name: name.clone(),
            });
            Next::Show(list(&shared, 0))
        })
    });
    spec
}

fn confirm_delete(shared: &Shared, index: usize) -> ChoiceSpec {
    let name = lock(shared)
        .list
        .get(index)
        .map(|(name, _)| name.clone())
        .unwrap_or_default();
    let delete = {
        let shared = shared.clone();
        Arc::new(move |tx: &AppEventSender| {
            let mut servers = lock(&shared);
            if index < servers.list.len() {
                servers.list.remove(index);
            }
            save(&servers, tx);
            drop(servers);
            Next::Show(list(&shared, index))
        })
    };
    let keep = {
        let shared = shared.clone();
        Arc::new(move |_: &AppEventSender| Next::Show(list(&shared, index + 1)))
    };
    let mut spec = ChoiceSpec::new(
        "MCP servers",
        vec![
            ChoiceItem::step(format!("Delete `{name}`"), delete),
            ChoiceItem::step("Keep it", keep.clone()),
        ],
    );
    spec.initial = Some(1);
    spec.searchable = false;
    spec.on_back = Some(keep);
    spec
}

fn transport(shared: &Shared) -> ChoiceSpec {
    let draft: Wizard = Arc::new(Mutex::new(Draft::default()));
    let choose = |http: bool| {
        let (shared, draft) = (shared.clone(), draft.clone());
        Arc::new(move |_: &AppEventSender| {
            lock(&draft).http = http;
            Next::Ask(ask_name(&shared, &draft))
        })
    };
    let mut spec = ChoiceSpec::new(
        "Add MCP server",
        vec![
            ChoiceItem::step("Stdio", choose(false)).detail("Run a local command"),
            ChoiceItem::step("HTTP", choose(true)).detail("Connect to a remote URL"),
        ],
    );
    spec.subtitle = Some("Select transport type".to_owned());
    spec.searchable = false;
    spec.on_back = Some({
        let shared = shared.clone();
        Arc::new(move |_| Next::Show(list(&shared, 0)))
    });
    spec
}

/// A question of the add wizard; `answer` receives the trimmed text.
fn question(
    verb: &'static str,
    label: &str,
    initial: String,
    answer: impl Fn(String, &AppEventSender) -> Result<Next, String> + Send + Sync + 'static,
) -> Ask {
    Ask {
        verb,
        label: label.to_owned(),
        initial,
        secret: false,
        then: Arc::new(move |text, tx| answer(text.trim().to_owned(), tx)),
    }
}

fn ask_name(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    question("server name", "", String::new(), move |name, _| {
        if name.is_empty() {
            return Err("a name is required".to_owned());
        }
        if lock(&shared)
            .list
            .iter()
            .any(|(existing, _)| *existing == name)
        {
            return Err(format!("`{name}` already exists; pick another name"));
        }
        let http = {
            let mut draft = lock(&draft);
            draft.name = name;
            draft.http
        };
        Ok(Next::Ask(if http {
            ask_url(&shared, &draft)
        } else {
            ask_command(&shared, &draft)
        }))
    })
}

fn ask_command(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question("command to run", &name, String::new(), move |command, _| {
        if command.is_empty() {
            return Err("a command is required".to_owned());
        }
        lock(&draft).command = command;
        Ok(Next::Ask(ask_args(&shared, &draft)))
    })
}

fn ask_args(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question(
        "args (space-separated, empty to skip)",
        &name,
        String::new(),
        move |args, _| {
            lock(&draft).args = args;
            Ok(Next::Ask(ask_env(&shared, &draft)))
        },
    )
}

fn ask_env(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question(
        "env KEY=VALUE (empty to finish)",
        &name,
        String::new(),
        move |entry, tx| match entry.split_once('=') {
            Some((key, value)) if !key.trim().is_empty() => {
                lock(&draft)
                    .env
                    .push((key.trim().to_owned(), value.trim().to_owned()));
                Ok(Next::Ask(ask_env(&shared, &draft)))
            }
            Some(_) => Err("write it as KEY=VALUE".to_owned()),
            None if entry.is_empty() => Ok(finish(&shared, &draft, tx)),
            None => Err("write it as KEY=VALUE".to_owned()),
        },
    )
}

fn ask_url(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question("server URL", &name, String::new(), move |url, _| {
        if url.is_empty() {
            return Err("a URL is required".to_owned());
        }
        lock(&draft).url = url;
        Ok(Next::Ask(ask_header(&shared, &draft)))
    })
}

fn ask_header(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question(
        "header Key: Value (empty to finish)",
        &name,
        String::new(),
        move |entry, _| match entry.split_once(':') {
            Some((key, value)) if !key.trim().is_empty() => {
                lock(&draft)
                    .headers
                    .push((key.trim().to_owned(), value.trim().to_owned()));
                Ok(Next::Ask(ask_header(&shared, &draft)))
            }
            Some(_) => Err("write it as Key: Value".to_owned()),
            None if entry.is_empty() => Ok(Next::Ask(ask_bearer(&shared, &draft))),
            None => Err("write it as Key: Value".to_owned()),
        },
    )
}

fn ask_bearer(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question(
        "bearer token env var (empty to use OAuth)",
        &name,
        String::new(),
        move |var, tx| {
            lock(&draft).bearer_env_var = var.clone();
            // A bearer token and OAuth client credentials exclude each other.
            Ok(if var.is_empty() {
                Next::Ask(ask_client_id(&shared, &draft))
            } else {
                finish(&shared, &draft, tx)
            })
        },
    )
}

fn ask_client_id(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question(
        "OAuth client id (empty to skip)",
        &name,
        String::new(),
        move |id, tx| {
            lock(&draft).client_id = id.clone();
            Ok(if id.is_empty() {
                finish(&shared, &draft, tx)
            } else {
                Next::Ask(ask_client_secret(&shared, &draft))
            })
        },
    )
}

fn ask_client_secret(shared: &Shared, draft: &Wizard) -> Ask {
    let (shared, draft) = (shared.clone(), draft.clone());
    let name = lock(&draft).name.clone();
    question(
        "client secret env var (empty to skip)",
        &name,
        String::new(),
        move |var, tx| Ok(finish_with(&shared, &draft, var, tx)),
    )
}

fn finish(shared: &Shared, draft: &Wizard, tx: &AppEventSender) -> Next {
    finish_with(shared, draft, String::new(), tx)
}

/// Add the server the wizard described, and show the list with it selected. An HTTP server with
/// no bearer token has its auth status computed so a needed OAuth login starts on its own.
fn finish_with(
    shared: &Shared,
    draft: &Wizard,
    client_secret_env_var: String,
    tx: &AppEventSender,
) -> Next {
    let draft = lock(draft);
    let config = if draft.http {
        let option = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        McpServerTransportConfig::StreamableHttp {
            url: draft.url.clone(),
            bearer_token_env_var: option(&draft.bearer_env_var),
            http_headers: (!draft.headers.is_empty())
                .then(|| draft.headers.iter().cloned().collect()),
            env_http_headers: None,
            client_id: option(&draft.client_id),
            client_secret_env_var: option(&client_secret_env_var),
        }
    } else {
        McpServerTransportConfig::Stdio {
            command: draft.command.clone(),
            args: draft.args.split_whitespace().map(String::from).collect(),
            env: (!draft.env.is_empty()).then(|| draft.env.iter().cloned().collect()),
            env_vars: vec![],
        }
    };
    let needs_login_check = draft.http && draft.bearer_env_var.is_empty();
    let mut servers = lock(shared);
    servers.list.push((
        draft.name.clone(),
        McpServerConfig {
            transport: config,
            enabled: true,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
        },
    ));
    save(&servers, tx);
    if needs_login_check {
        servers.login_when_known = Some(draft.name.clone());
        tx.send(AppEvent::ComputeMcpAuthStatuses);
    }
    let last = servers.list.len();
    drop(servers);
    Next::Show(list(shared, last))
}

/// A stdio and an HTTP server, for the storybook.
#[cfg(feature = "storybook")]
pub(crate) fn mcp_specimen() -> ChoiceSpec {
    let server = |transport| McpServerConfig {
        transport,
        enabled: true,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        enabled_tools: None,
        disabled_tools: None,
    };
    mcp_choices(&BTreeMap::from([
        (
            "filesystem".to_owned(),
            server(McpServerTransportConfig::Stdio {
                command: "mcp-fs".to_owned(),
                args: vec![],
                env: None,
                env_vars: vec![],
            }),
        ),
        (
            "linear".to_owned(),
            server(McpServerTransportConfig::StreamableHttp {
                url: "https://mcp.linear.app/mcp".to_owned(),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
                client_id: None,
                client_secret_env_var: None,
            }),
        ),
    ]))
}
