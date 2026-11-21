//! Persistent in-process `brush` (`!`) user shell history cell and event handling.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::UserShellEvent;
use nori_protocol::UserShellStream;
use ratatui::text::Line;
use tokio::sync::Mutex;

use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::history_cell::HistoryCell;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Disclosure;
use crate::history_cell::chrome::Progress;
use crate::history_cell::shell_block::ShellBlock;

static LOCAL_SHELL_SEQ: AtomicI64 = AtomicI64::new(0);

#[derive(Debug, Clone)]
pub(super) struct PendingUserShell {
    command: String,
    cwd: PathBuf,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Default, Clone)]
pub(super) struct UserShellState {
    pending: HashMap<String, PendingUserShell>,
    local_session: Arc<Mutex<Option<nori_bash::BashSession>>>,
}

impl UserShellState {
    /// The in-process shell the `!` commands run in when no session is attached.
    pub(super) fn local_session(&self) -> Arc<Mutex<Option<nori_bash::BashSession>>> {
        Arc::clone(&self.local_session)
    }
}

impl std::fmt::Debug for UserShellState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserShellState")
            .field("pending_count", &self.pending.len())
            .finish()
    }
}

/// A command the user ran with `!`, and what it printed: the same block an agent's shell call
/// is drawn as, with the directory it ran in at the right when there is room for it.
#[derive(Debug, Clone)]
pub(crate) struct UserShellCell {
    command: String,
    cwd: PathBuf,
    stdout: String,
    stderr: String,
    exit_code: i32,
}

impl UserShellCell {
    pub(crate) fn new(
        command: String,
        cwd: PathBuf,
        stdout: String,
        stderr: String,
        exit_code: i32,
    ) -> Self {
        Self {
            command,
            cwd,
            stdout,
            stderr,
            exit_code,
        }
    }
}

impl HistoryCell for UserShellCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let cwd = compact_cwd(&self.cwd);
        ShellBlock {
            command: &self.command,
            progress: if self.exit_code == 0 {
                Progress::Done
            } else {
                Progress::Failed
            },
            since: None,
            animate: false,
            elapsed: None,
            exit_code: Some(self.exit_code),
            cwd: (!cwd.is_empty()).then_some(cwd.as_str()),
            machine: None,
            stdout: &self.stdout,
            stderr: &self.stderr,
        }
        // The user ran it to see what it printed.
        .render(&Chrome::live(), usize::from(width), Disclosure::Expanded)
    }
}

fn compact_cwd(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = path.strip_prefix(&home)
    {
        let rel_str = rel.display().to_string();
        if rel_str.is_empty() {
            return "~".to_owned();
        }
        return format!("~/{rel_str}");
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.display().to_string(), str::to_owned)
}

impl ChatWidget {
    pub(super) fn dispatch_user_shell_command(&mut self, cmd: String) {
        self.persist_prompt_history(&format!("!{cmd}"));
        if self.harness_handle.is_some() {
            self.submit_harness_action(crate::app_event::HarnessAction::RunUserShell(cmd));
            return;
        }
        let tx = self.app_event_tx.clone();
        let generation = self.session_generation;
        let initial_cwd = self.config.cwd.clone();
        let session_cell = Arc::clone(&self.user_shell_state.local_session);
        let seq = LOCAL_SHELL_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let op_id = format!("local-shell-{seq}");

        tokio::spawn(async move {
            let mut guard = session_cell.lock().await;
            if guard.is_none() {
                match nori_bash::BashSession::new(&initial_cwd).await {
                    Ok(session) => *guard = Some(session),
                    Err(err) => {
                        send_local_shell_event(
                            &tx,
                            generation,
                            UserShellEvent::Started {
                                operation_id: op_id.clone(),
                                command: cmd,
                                cwd: initial_cwd,
                            },
                        );
                        send_local_shell_event(
                            &tx,
                            generation,
                            UserShellEvent::Output {
                                operation_id: op_id.clone(),
                                stream: UserShellStream::Stderr,
                                chunk: err.to_string().into_bytes(),
                            },
                        );
                        send_local_shell_event(
                            &tx,
                            generation,
                            UserShellEvent::Finished {
                                operation_id: op_id,
                                exit_code: 1,
                            },
                        );
                        return;
                    }
                }
            }
            let Some(bash) = guard.as_mut() else {
                return;
            };
            let live_cwd = bash.cwd().to_path_buf();
            send_local_shell_event(
                &tx,
                generation,
                UserShellEvent::Started {
                    operation_id: op_id.clone(),
                    command: cmd.clone(),
                    cwd: live_cwd,
                },
            );
            let (stdout, stderr, exit_code) = match bash.exec(&cmd).await {
                Ok(out) => (
                    out.stdout.into_bytes(),
                    out.stderr.into_bytes(),
                    i32::try_from(out.exit_code).unwrap_or(1),
                ),
                Err(err) => (Vec::new(), err.to_string().into_bytes(), 1),
            };
            drop(guard);

            if !stdout.is_empty() {
                send_local_shell_event(
                    &tx,
                    generation,
                    UserShellEvent::Output {
                        operation_id: op_id.clone(),
                        stream: UserShellStream::Stdout,
                        chunk: stdout,
                    },
                );
            }
            if !stderr.is_empty() {
                send_local_shell_event(
                    &tx,
                    generation,
                    UserShellEvent::Output {
                        operation_id: op_id.clone(),
                        stream: UserShellStream::Stderr,
                        chunk: stderr,
                    },
                );
            }
            send_local_shell_event(
                &tx,
                generation,
                UserShellEvent::Finished {
                    operation_id: op_id,
                    exit_code,
                },
            );
        });
    }

    pub(super) fn on_user_shell_event(&mut self, event: UserShellEvent) {
        match event {
            UserShellEvent::Started {
                operation_id,
                command,
                cwd,
            } => {
                self.user_shell_state.pending.insert(
                    operation_id,
                    PendingUserShell {
                        command,
                        cwd,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    },
                );
            }
            UserShellEvent::Output {
                operation_id,
                stream,
                chunk,
            } => {
                if let Some(entry) = self.user_shell_state.pending.get_mut(&operation_id) {
                    match stream {
                        UserShellStream::Stdout => entry.stdout.extend_from_slice(&chunk),
                        UserShellStream::Stderr => entry.stderr.extend_from_slice(&chunk),
                    }
                }
            }
            UserShellEvent::Finished {
                operation_id,
                exit_code,
            } => {
                let Some(entry) = self.user_shell_state.pending.remove(&operation_id) else {
                    return;
                };
                let refresh_dir = entry.cwd.clone();
                let cell = UserShellCell::new(
                    entry.command,
                    entry.cwd,
                    String::from_utf8_lossy(&entry.stdout).into_owned(),
                    String::from_utf8_lossy(&entry.stderr).into_owned(),
                    exit_code,
                );
                self.add_to_history(cell);
                self.app_event_tx
                    .send(AppEvent::RefreshSystemInfoForDirectory {
                        dir: refresh_dir,
                        agent: Some(self.config.active_agent.clone()),
                    });
                self.request_redraw();
            }
        }
    }
}

fn send_local_shell_event(
    tx: &crate::app_event_sender::AppEventSender,
    generation: crate::app_event::SessionGeneration,
    event: UserShellEvent,
) {
    tx.send(AppEvent::SessionEvent {
        generation,
        event: SessionEvent::Nori(NoriEvent::UserShell(event)),
    });
}
