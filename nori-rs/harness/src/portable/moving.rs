//! Moving a session to another machine: plan, ship, hand over (see `docs/machines.md`).
//!
//! The caller fences the session first (no turn running, journal flushed) and records
//! [`super::MOVED_OUT`] after [`ship`] succeeds; this module only talks to the target.

use std::path::Path;
use std::time::Duration;

use nori_machines::Machine;
use nori_machines::RemoteDir;
use nori_machines::Roster;
use nori_machines::StartDir;

use super::Imported;
use super::bundle::Manifest;
use super::bundle::SessionBundle;

#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    #[error(transparent)]
    Machine(#[from] nori_machines::ExecError),
    #[error(
        "Nori is not installed on {machine} (set `nori` in [machines.{machine}] if it is elsewhere)"
    )]
    NoNori { machine: String },
    #[error("there is no directory for this project on {machine}: {tried}")]
    NoDirectory { machine: String, tried: String },
    #[error("the import on {machine} failed: {reason}")]
    Import { machine: String, reason: String },
}

/// What a move will do, before anything changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovePlan {
    pub machine: Machine,
    /// Where the session continues there.
    pub cwd: String,
    pub nori_version: String,
    /// The checkout there, when the directory is a git repository.
    pub checkout: Option<Checkout>,
    /// One line per consequence the person should know.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub branch: String,
    pub head: String,
    pub remote: Option<String>,
}

/// The Nori command on `machine`, as shell words.
fn nori_words(machine: &Machine) -> String {
    let program = machine.nori.program.as_deref().unwrap_or("nori");
    let program = match program.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None => quote(program),
    };
    match machine.nori.home.as_deref() {
        Some(home) => {
            let home = match home.strip_prefix("~/") {
                Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
                None => quote(home),
            };
            format!("NORI_HOME={home} {program}")
        }
        None => program,
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Learn what the target has: Nori, and a directory for this project (the one named, else the
/// project's path under its home, else its configured directory).
pub async fn plan(
    roster: &Roster,
    machine: &Machine,
    manifest: &Manifest,
    local_cwd: &Path,
    requested_cwd: Option<&str>,
) -> Result<MovePlan, MoveError> {
    let candidates = match requested_cwd {
        Some(dir) => vec![RemoteDir::parse(dir)],
        None => roster.default_dirs(machine, local_cwd),
    };
    let tried: Vec<String> = candidates
        .iter()
        .map(|dir| match dir {
            RemoteDir::Path(path) => path.clone(),
            RemoteDir::Home => "~".to_owned(),
        })
        .collect();
    let script = format!(
        r#"nori_version=$({nori} --version 2>/dev/null | head -n 1)
printf 'NORI %s\n' "$nori_version"
printf 'DIR %s\n' "$PWD"
if git rev-parse --git-dir >/dev/null 2>&1; then
  printf 'BRANCH %s\n' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
  printf 'HEAD %s\n' "$(git rev-parse HEAD 2>/dev/null)"
  printf 'REMOTE %s\n' "$(git remote get-url origin 2>/dev/null)"
fi"#,
        nori = nori_words(machine),
    );
    let run = roster
        .exec(
            machine,
            &script,
            &StartDir::FirstOf(candidates),
            Some(Duration::from_secs(60)),
            None,
        )
        .await?;
    let field = |key: &str| {
        run.stdout
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let Some(nori_version) = field("NORI") else {
        return Err(MoveError::NoNori {
            machine: machine.name.to_string(),
        });
    };
    let cwd = field("DIR").unwrap_or_else(|| run.started_in.clone());
    let checkout = field("HEAD").map(|head| Checkout {
        branch: field("BRANCH").unwrap_or_default(),
        head,
        remote: field("REMOTE"),
    });
    let home_dir = roster
        .reach(&machine.name)
        .and_then(|reach| match reach {
            nori_machines::Reach::Reachable(facts) => Some(facts.home),
            nori_machines::Reach::Unreachable { .. } => None,
        })
        .unwrap_or_default();
    if requested_cwd.is_none() && !home_dir.is_empty() && cwd == home_dir {
        return Err(MoveError::NoDirectory {
            machine: machine.name.to_string(),
            tried: tried.join(", "),
        });
    }
    let mut notes = vec![format!(
        "The agent's own session starts again on {} with this conversation handed over; its prompt cache is lost.",
        machine.name
    )];
    match (&checkout, &manifest.git) {
        (Some(there), Some(here)) => {
            if here.head.as_deref() != Some(there.head.as_str()) {
                notes.push(format!(
                    "{} is at {} on {}, not at this checkout's {}.",
                    cwd,
                    short(&there.head),
                    there.branch,
                    here.head.as_deref().map_or("commit", short)
                ));
            }
        }
        (None, Some(_)) => notes.push(format!("{cwd} on {} is not a git checkout.", machine.name)),
        (Some(_), None) | (None, None) => {}
    }
    Ok(MovePlan {
        machine: machine.clone(),
        cwd,
        nori_version,
        checkout,
        notes,
    })
}

fn short(head: &str) -> &str {
    &head[..head.len().min(8)]
}

/// Send the bundle to the plan's machine and import it there.
pub async fn ship(
    roster: &Roster,
    plan: &MovePlan,
    bundle: &SessionBundle,
) -> Result<Imported, MoveError> {
    let machine = &plan.machine;
    let incoming = match machine.nori.home.as_deref() {
        Some(home) => RemoteDir::parse(&format!("{}/incoming", home.trim_end_matches('/'))),
        None => RemoteDir::parse("~/.nori/cli/incoming"),
    };
    let name = format!("{}.bundle", bundle.manifest.conversation_id);
    let path = roster
        .upload(machine, &incoming, &name, &bundle.encode())
        .await?;
    let command = format!(
        "{} session import {} --cwd {} --json && rm -f {}",
        nori_words(machine),
        quote(&path),
        quote(&plan.cwd),
        quote(&path),
    );
    let run = roster
        .exec(
            machine,
            &command,
            &StartDir::Required(RemoteDir::Path(plan.cwd.clone())),
            Some(Duration::from_secs(120)),
            None,
        )
        .await?;
    if run.exit_code != 0 {
        return Err(MoveError::Import {
            machine: machine.name.to_string(),
            reason: run
                .stderr
                .lines()
                .last()
                .unwrap_or("it exited with an error")
                .to_owned(),
        });
    }
    let imported = run
        .stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .ok_or_else(|| MoveError::Import {
            machine: machine.name.to_string(),
            reason: "it did not say where the conversation went".to_owned(),
        })?;
    Ok(Imported {
        conversation_id: imported["conversation_id"]
            .as_str()
            .unwrap_or(&bundle.manifest.conversation_id)
            .to_owned(),
        project_id: imported["project_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        transcript: imported["transcript"].as_str().unwrap_or_default().into(),
        children: imported["children"].as_i64().unwrap_or_default(),
    })
}

/// The command that continues the conversation on the plan's machine, as a POSIX `sh` line to
/// run in its directory.
pub fn resume_command(plan: &MovePlan, conversation_id: &str) -> String {
    format!(
        "cd {} && exec {} resume {}",
        quote(&plan.cwd),
        nori_words(&plan.machine),
        quote(conversation_id)
    )
}

/// Put a launcher for the conversation on the plan's machine and return its absolute path there:
/// `/bin/sh PATH` continues the conversation, from any login shell (`ssh -t TARGET`, a Rex pane),
/// with nothing for that shell to parse but two words.
pub async fn stage_resume(
    roster: &Roster,
    plan: &MovePlan,
    conversation_id: &str,
) -> Result<String, MoveError> {
    let incoming = match plan.machine.nori.home.as_deref() {
        Some(home) => RemoteDir::parse(&format!("{}/incoming", home.trim_end_matches('/'))),
        None => RemoteDir::parse("~/.nori/cli/incoming"),
    };
    let facts_path = match roster.reach(&plan.machine.name) {
        Some(nori_machines::Reach::Reachable(facts)) => facts.path,
        Some(nori_machines::Reach::Unreachable { .. }) | None => None,
    };
    let mut script = String::from("#!/bin/sh\n");
    // A pane or `ssh -t` starts with a bare environment: the login PATH, as commands there get.
    if let Some(path) = facts_path {
        script.push_str(&format!("PATH={}; export PATH\n", quote(&path)));
    }
    script.push_str(&resume_command(plan, conversation_id));
    script.push('\n');
    Ok(roster
        .upload(
            &plan.machine,
            &incoming,
            &format!("{conversation_id}.resume.sh"),
            script.as_bytes(),
        )
        .await?)
}
