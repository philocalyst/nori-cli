//! How an agent crate offers itself to the host.
//!
//! Every agent crate exposes one `registration()`. The application collects them and hands each to
//! the ACP host, which starts the agent on one end of an in-memory pipe whenever a `native`
//! distribution names it (see `nori_acp_host::in_process`).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::io::DuplexStream;

use crate::agent::NativeAgent;
use crate::serve::serve_over_pipe_as;

/// What an in-process agent is started with.
#[derive(Clone, PartialEq, Eq)]
pub struct InProcessSpec {
    /// The built-in agent's name, such as `codex`.
    pub agent: String,
    /// Where this agent's state and login live. Per agent, because a process-wide environment
    /// variable cannot tell two accounts apart.
    pub home: Option<PathBuf>,
    /// `key=value` options. For Codex they are `codex -c` overrides; for the others see
    /// [`SpecOptions`].
    pub config: Vec<String>,
    /// Per-agent environment overrides. They never reach the process environment.
    pub environment: HashMap<String, String>,
}

impl InProcessSpec {
    /// An environment variable: this agent's own override first, the process's second.
    pub fn env(&self, key: &str) -> Option<String> {
        self.environment
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
    }

    /// The per-agent environment as `OsString`s, for starting a subprocess.
    pub fn environment_os(&self) -> HashMap<std::ffi::OsString, std::ffi::OsString> {
        self.environment
            .iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect()
    }
}

impl std::fmt::Debug for InProcessSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InProcessSpec")
            .field("agent", &self.agent)
            .field("home", &self.home)
            .field(
                "config",
                &self
                    .config
                    .iter()
                    .map(|value| {
                        if value.starts_with("server_password=") {
                            "server_password=<redacted>".to_owned()
                        } else {
                            value.clone()
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .field("environment", &"<redacted>")
            .finish()
    }
}

/// The `key=value` options of a spec, checked against the keys an agent accepts. An unknown key
/// is an error naming it, never silently ignored.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpecOptions {
    pairs: Vec<(String, String)>,
}

impl SpecOptions {
    /// Split `raw` into pairs. `agent` names the agent in the error for a malformed or
    /// unsupported option; `accepted` lists the keys it understands.
    pub fn parse(agent: &str, raw: &[String], accepted: &[&str]) -> std::io::Result<Self> {
        let mut pairs = Vec::with_capacity(raw.len());
        for option in raw {
            match option.split_once('=') {
                Some((key, value)) if accepted.contains(&key.trim()) => {
                    pairs.push((key.trim().to_owned(), value.to_owned()));
                }
                Some(_) | None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "unsupported {agent} native option: {option} (accepted: {})",
                            accepted.join(", ")
                        ),
                    ));
                }
            }
        }
        Ok(Self { pairs })
    }

    /// The value of `key`, the last one given when it repeats.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The first of `keys` that is set. For an option with aliases.
    pub fn get_any(&self, keys: &[&str]) -> Option<&str> {
        keys.iter().find_map(|key| self.get(key))
    }
}

/// Who is on the other end of the pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    /// Nori's own session harness, in this process: it registers Nori's host tools and passes their
    /// tokens in `_meta`, so an agent says it takes them.
    Nori,
    /// Any ACP client over stdio (`nori agent serve`). It has no host tools to give, so the agent
    /// does not advertise any.
    External,
}

/// Runs an agent on `pipe` until the connection ends.
pub type Launcher = Arc<
    dyn Fn(InProcessSpec, DuplexStream, Host) -> BoxFuture<'static, std::io::Result<()>>
        + Send
        + Sync,
>;

/// Starts what an agent needs for a session ahead of time, for the working directory and the
/// client's MCP servers (as the JSON array ACP sends in `session/new`). Returns at once.
pub type Prewarmer = Arc<dyn Fn(InProcessSpec, PathBuf, serde_json::Value) + Send + Sync>;

/// Folds the environment a custom adapter would have read into explicit options. The host calls
/// it with the agent's configured environment, since a process shared with other agents has no
/// per-agent environment.
pub type Resolver = fn(InProcessSpec, &HashMap<String, String>) -> InProcessSpec;

/// One built-in agent, ready for the host.
#[derive(Clone)]
pub struct Registration {
    /// What a `native = { agent = "..." }` distribution calls it.
    pub name: &'static str,
    pub launch: Launcher,
    pub prewarm: Option<Prewarmer>,
    pub resolve: Option<Resolver>,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("name", &self.name)
            .field("prewarm", &self.prewarm.is_some())
            .field("resolve", &self.resolve.is_some())
            .finish_non_exhaustive()
    }
}

impl Registration {
    /// Register the agent `build` makes from a spec. The kit serves it over the pipe and
    /// shuts it down when the connection ends.
    pub fn new<A, F, Fut>(name: &'static str, build: F) -> Self
    where
        A: NativeAgent,
        F: Fn(InProcessSpec) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::io::Result<A>> + Send + 'static,
    {
        let build = Arc::new(build);
        Self {
            name,
            launch: Arc::new(move |spec, pipe, host| {
                let build = Arc::clone(&build);
                Box::pin(async move {
                    let agent = build(spec).await?;
                    serve_over_pipe_as(pipe, agent, host).await
                })
            }),
            prewarm: None,
            resolve: None,
        }
    }

    /// Let the agent start sessions ahead of time.
    pub fn with_prewarm(
        mut self,
        prewarm: impl Fn(InProcessSpec, PathBuf, serde_json::Value) + Send + Sync + 'static,
    ) -> Self {
        self.prewarm = Some(Arc::new(prewarm));
        self
    }

    /// Fold a configured environment into the spec before the agent starts.
    pub fn with_resolver(mut self, resolve: Resolver) -> Self {
        self.resolve = Some(resolve);
        self
    }
}
