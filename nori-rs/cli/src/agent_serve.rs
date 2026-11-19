//! `nori agent serve <slug> [KEY=VALUE...]`: any built-in agent over stdio ACP.
//!
//! Every agent built into this binary can be run by any ACP client, not only by Nori's own
//! harness: the client starts `nori agent serve claude` as it would any ACP agent process.
//! Nothing but ACP goes to stdout.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use nori_acp_host::in_process;

#[derive(Debug, Parser)]
pub struct AgentCommand {
    #[clap(subcommand)]
    pub sub: AgentSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum AgentSubcommand {
    /// Serve a built-in agent over stdio ACP.
    Serve(ServeCommand),
}

#[derive(Debug, Parser)]
pub struct ServeCommand {
    /// The built-in agent: `claude`, `codex`, `opencode`, and `gemini` or `antigravity-sdk` in
    /// builds that include them.
    pub slug: String,

    /// Options the agent understands, as `KEY=VALUE` (for example `cli_path=/path/to/claude` or
    /// `model=opus` for claude; `-c` overrides such as `model="o3"` for codex).
    #[arg(value_name = "KEY=VALUE")]
    pub options: Vec<String>,

    /// Where the agent keeps its state and login (for codex, a `CODEX_HOME`).
    #[arg(long, value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// Environment for this agent only, as `KEY=VALUE`, on top of the process environment.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub environment: Vec<String>,

    /// For the `fake` agent: the world (Pkl or JSON) it plays, shorthand for `world=FILE`.
    #[arg(long, value_name = "FILE")]
    pub world: Option<PathBuf>,
}

pub async fn run(command: AgentCommand) -> anyhow::Result<()> {
    match command.sub {
        AgentSubcommand::Serve(serve) => run_serve(serve).await,
    }
}

async fn run_serve(command: ServeCommand) -> anyhow::Result<()> {
    let mut overrides = HashMap::new();
    for pair in &command.environment {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("--env takes KEY=VALUE, got {pair:?}"))?;
        overrides.insert(key.to_owned(), value.to_owned());
    }
    // What a custom adapter would have read: the process environment with the overrides on top.
    let mut environment: HashMap<String, String> = std::env::vars().collect();
    environment.extend(overrides.clone());
    let mut options = command.options;
    if let Some(world) = &command.world {
        options.push(format!("world={}", world.display()));
    }
    let spec = in_process::InProcessSpec {
        agent: command.slug,
        home: command.home,
        config: options,
        environment: overrides,
    };
    in_process::serve_stdio(spec, &environment).await?;
    Ok(())
}
