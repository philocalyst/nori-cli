//! `nori session`: carry conversations between machines.
//!
//! `export` writes a conversation's bundle, `import` reads one into this Nori home, and `move`
//! does the whole move of a conversation that is not running: plan, ship, record, and say how to
//! continue there. A running session moves with `/move` (see `docs/machines.md`).

use std::io::Read as _;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use nori_harness::portable;
use nori_harness::portable::SessionBundle;
use nori_harness::portable::moving;

#[derive(Debug, Parser)]
pub struct SessionCommand {
    #[clap(subcommand)]
    action: SessionAction,
}

#[derive(Debug, clap::Subcommand)]
enum SessionAction {
    /// Write a conversation's bundle (transcripts and journals, never credentials).
    Export {
        /// The conversation ID.
        conversation: String,
        /// Where to write it; standard output when omitted.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Read a bundle into this Nori home; `nori resume ID` then continues it.
    Import {
        /// The bundle file, or `-` for standard input.
        bundle: String,
        /// The working directory the conversation continues in here.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Print where it went as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Move a conversation that is not running to another machine.
    Move {
        /// The conversation ID.
        conversation: String,
        /// The machine (a Rex host or `[machines.NAME]`).
        machine: String,
        /// The directory it continues in there.
        #[arg(long)]
        cwd: Option<String>,
    },
}

pub async fn run(command: SessionCommand) -> anyhow::Result<()> {
    let home = nori_config::find_nori_home()?;
    match command.action {
        SessionAction::Export {
            conversation,
            output,
        } => {
            let bundle = portable::export(&home, &conversation).await?;
            let bytes = bundle.encode();
            match output {
                Some(path) => std::fs::write(&path, bytes)
                    .with_context(|| format!("writing {}", path.display()))?,
                None => std::io::stdout().write_all(&bytes)?,
            }
        }
        SessionAction::Import { bundle, cwd, json } => {
            let bytes = if bundle == "-" {
                let mut bytes = Vec::new();
                std::io::stdin().read_to_end(&mut bytes)?;
                bytes
            } else {
                std::fs::read(&bundle).with_context(|| format!("reading {bundle}"))?
            };
            let bundle = SessionBundle::decode(&bytes)?;
            let cwd = match cwd {
                Some(cwd) => cwd,
                None => std::env::current_dir()?,
            };
            let imported = portable::import(&home, &bundle, &cwd).await?;
            if json {
                println!("{}", serde_json::to_string(&imported)?);
            } else {
                println!(
                    "Imported {} ({} children). Continue it with: nori resume {}",
                    imported.conversation_id, imported.children, imported.conversation_id
                );
            }
        }
        SessionAction::Move {
            conversation,
            machine,
            cwd,
        } => {
            nori_machines::install(std::sync::Arc::new(nori_machines::Roster::discover(&home)));
            let roster = nori_machines::global();
            let machine = match roster.resolve(Some(&machine))? {
                nori_machines::Target::Remote(machine) => machine.clone(),
                nori_machines::Target::Local => {
                    anyhow::bail!("{machine} is this computer; the conversation is already here")
                }
            };
            let bundle = portable::export(&home, &conversation).await?;
            let plan = moving::plan(
                &roster,
                &machine,
                &bundle.manifest,
                &bundle.manifest.origin.cwd,
                cwd.as_deref(),
            )
            .await?;
            println!(
                "Moving {conversation} to {} ({}), into {}",
                machine.name, plan.nori_version, plan.cwd
            );
            for note in &plan.notes {
                println!("  · {note}");
            }
            let imported = moving::ship(&roster, &plan, &bundle).await?;
            let launcher = moving::stage_resume(&roster, &plan, &imported.conversation_id).await?;
            portable::record_moved_out(
                &home,
                &conversation,
                &portable::MovedTo {
                    machine: machine.name.to_string(),
                    ssh: machine.ssh.to_string(),
                    cwd: plan.cwd.clone(),
                    launcher: Some(launcher.clone()),
                    at: chrono_now(),
                },
            )
            .await?;
            println!(
                "Moved ({} children). Continue it there with: ssh -t {} /bin/sh {launcher}",
                imported.children, machine.ssh,
            );
        }
    }
    Ok(())
}

fn chrono_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs().to_string())
        .unwrap_or_default()
}
