//! `/move`: this session, whole, to another machine (see `docs/machines.md`).
//!
//! Choosing a machine starts the move at once; while a turn runs it waits for the turn to end,
//! so nothing is cut short. The move runs on a task: the bundle is made and shipped, the target
//! imports it, the journal here records where it went, and the terminal is handed over: in Rex a
//! pane on the target's Rex host continues the conversation and the user's app shows it; outside
//! Rex, Nori exits into `ssh -t TARGET nori resume ID` in this terminal.

use std::path::PathBuf;

use nori_harness::portable;
use nori_harness::portable::moving;

use super::App;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

/// How the terminal goes to the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Handover {
    /// A Rex pane on the target runs it; this Nori just exits.
    Rex { machine: String, shown: bool },
    /// This terminal runs this command (`ssh -t ...`) once Nori has exited.
    Exec(Vec<String>),
}

/// A finished move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Moved {
    pub(crate) machine: String,
    pub(crate) cwd: String,
    pub(crate) notes: Vec<String>,
    pub(crate) handover: Handover,
}

impl App {
    /// Start moving to `machine`: now when idle, else when the running turn ends.
    pub(super) fn begin_move(&mut self, machine: String) {
        if self.move_in_progress || self.pending_move.is_some() {
            self.chat_widget
                .add_info_message("A session move is already in progress.".to_owned(), None);
            return;
        }
        let Some(conversation) = self.chat_widget.conversation_id() else {
            self.chat_widget.add_error_message(
                "This session has no recorded conversation to move yet: send a message first."
                    .to_owned(),
            );
            return;
        };
        if self.chat_widget.is_task_running() {
            self.chat_widget.add_info_message(
                format!("Moving to {machine} as soon as this turn ends."),
                None,
            );
            self.pending_move = Some(machine);
            return;
        }
        self.chat_widget.add_info_message(
            format!("Moving this session to {machine}…"),
            Some("Planning, then shipping the conversation; nothing here stops.".to_owned()),
        );
        self.move_in_progress = true;
        let home = self.config.nori_home.clone();
        let tx = self.app_event_tx.clone();
        tokio::spawn(run(home, conversation.to_string(), machine, tx));
    }

    /// A turn ended: a move that waited for it starts.
    pub(crate) fn start_pending_move(&mut self) {
        if !self.chat_widget.is_task_running()
            && let Some(machine) = self.pending_move.take()
        {
            self.begin_move(machine);
        }
    }

    /// The move is done (or failed): say so, and hand the terminal over.
    pub(super) fn finish_move(&mut self, outcome: Result<Moved, String>) {
        self.move_in_progress = false;
        match outcome {
            Err(error) => self
                .chat_widget
                .add_error_message(format!("The session did not move: {error}")),
            Ok(moved) => {
                let hint = moved.notes.join(" ");
                let message = match &moved.handover {
                    Handover::Rex {
                        machine,
                        shown: true,
                    } => {
                        format!(
                            "Moved to {machine} ({}); it continues in Rex there.",
                            moved.cwd
                        )
                    }
                    Handover::Rex {
                        machine,
                        shown: false,
                    } => format!(
                        "Moved to {machine} ({}); open its new session in Rex's sidebar to continue.",
                        moved.cwd
                    ),
                    Handover::Exec(_) => {
                        format!(
                            "Moved to {} ({}); continuing there.",
                            moved.machine, moved.cwd
                        )
                    }
                };
                self.chat_widget
                    .add_info_message(message, (!hint.is_empty()).then_some(hint));
                if let Handover::Exec(argv) = moved.handover {
                    self.pending_handover = Some(argv);
                }
                self.app_event_tx.send(AppEvent::ExitRequest);
            }
        }
    }
}

impl App {
    /// A conversation chosen in `/resume` that moved to another machine: open it there.
    pub(super) fn follow_moved(
        &mut self,
        moved: nori_harness::portable::MovedTo,
        conversation: String,
    ) {
        let Some(launcher) = moved.launcher.clone() else {
            self.chat_widget.add_info_message(
                format!(
                    "This conversation moved to {} and continues there.",
                    moved.machine
                ),
                None,
            );
            return;
        };
        if nori_rex::RexPane::from_env().is_none() {
            self.pending_handover = Some(vec![
                "ssh".to_owned(),
                "-t".to_owned(),
                moved.ssh,
                format!("/bin/sh {launcher}"),
            ]);
            self.app_event_tx.send(AppEvent::ExitRequest);
            return;
        }
        let command = format!("ssh -t {} /bin/sh {launcher}", moved.ssh);
        self.chat_widget.add_info_message(
            format!(
                "This conversation moved to {} and continues there.",
                moved.machine
            ),
            Some(format!("Opening it there; outside Rex: {command}")),
        );
        tokio::spawn(async move {
            let _ = in_rex(&moved.machine, &moved.cwd, &launcher, &conversation).await;
        });
    }
}

/// Plan, ship, record, and prepare the handover.
async fn run(home: PathBuf, conversation: String, machine: String, tx: AppEventSender) {
    let outcome = move_to(&home, &conversation, &machine).await;
    tx.send(AppEvent::MoveFinished(outcome));
}

async fn move_to(home: &std::path::Path, conversation: &str, name: &str) -> Result<Moved, String> {
    let roster = nori_machines::global_ready().await;
    let machine = match roster
        .resolve(Some(name))
        .map_err(|error| error.to_string())?
    {
        nori_machines::Target::Remote(machine) => machine.clone(),
        nori_machines::Target::Local => return Err(format!("{name} is this computer")),
    };
    let bundle = portable::export(home, conversation)
        .await
        .map_err(|error| error.to_string())?;
    let plan = moving::plan(
        &roster,
        &machine,
        &bundle.manifest,
        &bundle.manifest.origin.cwd,
        None,
    )
    .await
    .map_err(|error| error.to_string())?;
    moving::ship(&roster, &plan, &bundle)
        .await
        .map_err(|error| error.to_string())?;
    let launcher = moving::stage_resume(&roster, &plan, conversation)
        .await
        .map_err(|error| error.to_string())?;
    portable::record_moved_out(
        home,
        conversation,
        &portable::MovedTo {
            machine: machine.name.to_string(),
            ssh: machine.ssh.to_string(),
            cwd: plan.cwd.clone(),
            launcher: Some(launcher.clone()),
            at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let handover = match in_rex(
        &machine.name.to_string(),
        &plan.cwd,
        &launcher,
        conversation,
    )
    .await
    {
        Some(shown) => Handover::Rex {
            machine: machine.name.to_string(),
            shown,
        },
        None => Handover::Exec(vec![
            "ssh".to_owned(),
            "-t".to_owned(),
            machine.ssh.to_string(),
            format!("/bin/sh {launcher}"),
        ]),
    };
    Ok(Moved {
        machine: machine.name.to_string(),
        cwd: plan.cwd,
        notes: plan.notes,
        handover,
    })
}

/// In Rex, open the conversation in a new session on the target's Rex host and show it in the
/// user's app. `None` when Nori is not in Rex or the target is not a Rex host; `Some(false)`
/// when the session opened but the app could not be told to show it.
async fn in_rex(host: &str, cwd: &str, launcher: &str, conversation: &str) -> Option<bool> {
    let pane = nori_rex::RexPane::from_env()?;
    let title = format!("nori {}", &conversation[..conversation.len().min(8)]);
    let created = tokio::process::Command::new(&pane.rex)
        .args([
            "-S", host, "new", &title, "-c", cwd, "--json", "--", "/bin/sh", launcher,
        ])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())?;
    let session = serde_json::from_slice::<serde_json::Value>(&created.stdout)
        .ok()?
        .get("session_id")?
        .as_str()?
        .to_owned();
    // The user's app is connected to that host too; ask it to show the new session.
    let clients = tokio::process::Command::new(&pane.rex)
        .args(["-S", host, "client", "ls", "--json"])
        .output()
        .await
        .ok()?;
    let clients: serde_json::Value = serde_json::from_slice(&clients.stdout).ok()?;
    let app = clients
        .as_array()
        .or_else(|| clients.get("clients").and_then(serde_json::Value::as_array))?
        .iter()
        .find(|client| client.to_string().contains("\"app\""))
        .and_then(|client| {
            client
                .get("client_id")
                .or_else(|| client.get("id"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned);
    let Some(app) = app else {
        return Some(false);
    };
    let shown = tokio::process::Command::new(&pane.rex)
        .args(["-S", host, "-C", &app, "do", "session.select"])
        .arg(format!("session_id={session}"))
        .output()
        .await
        .is_ok_and(|output| output.status.success());
    Some(shown)
}
