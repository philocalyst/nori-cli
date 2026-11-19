use std::path::Path;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use super::BackendEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::UserShellEvent;
use nori_protocol::UserShellStream;

pub(crate) async fn run_user_shell_command(
    event_tx: &mpsc::Sender<BackendEvent>,
    session_cell: &Mutex<Option<nori_bash::BashSession>>,
    id: &str,
    initial_cwd: &Path,
    command: String,
) {
    let mut guard = session_cell.lock().await;
    if guard.is_none() {
        match nori_bash::BashSession::new(initial_cwd).await {
            Ok(session) => *guard = Some(session),
            Err(err) => {
                let _ = event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        NoriEvent::UserShell(UserShellEvent::Started {
                            operation_id: id.to_string(),
                            command,
                            cwd: initial_cwd.to_path_buf(),
                        }),
                    )))
                    .await;
                let _ = event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        NoriEvent::UserShell(UserShellEvent::Output {
                            operation_id: id.to_string(),
                            stream: UserShellStream::Stderr,
                            chunk: err.to_string().into_bytes(),
                        }),
                    )))
                    .await;
                let _ = event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        NoriEvent::UserShell(UserShellEvent::Finished {
                            operation_id: id.to_string(),
                            exit_code: 1,
                        }),
                    )))
                    .await;
                return;
            }
        }
    }

    let Some(bash) = guard.as_mut() else {
        return;
    };
    let live_cwd = bash.cwd().to_path_buf();

    let _ = event_tx
        .send(BackendEvent::Public(SessionEvent::Nori(
            NoriEvent::UserShell(UserShellEvent::Started {
                operation_id: id.to_string(),
                command: command.clone(),
                cwd: live_cwd,
            }),
        )))
        .await;

    let (stdout_bytes, stderr_bytes, exit_code) = match bash.exec(&command).await {
        Ok(output) => (
            output.stdout.into_bytes(),
            output.stderr.into_bytes(),
            i32::try_from(output.exit_code).unwrap_or(1),
        ),
        Err(err) => (Vec::new(), err.to_string().into_bytes(), 1),
    };
    drop(guard);

    if !stdout_bytes.is_empty() {
        let _ = event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                NoriEvent::UserShell(UserShellEvent::Output {
                    operation_id: id.to_string(),
                    stream: UserShellStream::Stdout,
                    chunk: stdout_bytes,
                }),
            )))
            .await;
    }
    if !stderr_bytes.is_empty() {
        let _ = event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                NoriEvent::UserShell(UserShellEvent::Output {
                    operation_id: id.to_string(),
                    stream: UserShellStream::Stderr,
                    chunk: stderr_bytes,
                }),
            )))
            .await;
    }

    let _ = event_tx
        .send(BackendEvent::Public(SessionEvent::Nori(
            NoriEvent::UserShell(UserShellEvent::Finished {
                operation_id: id.to_string(),
                exit_code,
            }),
        )))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn user_shell_command_emits_typed_lifecycle_and_persists_brush_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let sub_dir = temp_dir.path().join("nested");
        std::fs::create_dir_all(&sub_dir).expect("mkdir nested");
        let session_cell = Mutex::new(None);
        let (event_tx, mut event_rx) = mpsc::channel(16);

        run_user_shell_command(
            &event_tx,
            &session_cell,
            "shell-1",
            temp_dir.path(),
            "cd nested && export NORI_TAG=brush-live && greet() { printf \"%s:%s\" \"$NORI_TAG\" \"$1\"; }".to_string(),
        )
        .await;

        run_user_shell_command(
            &event_tx,
            &session_cell,
            "shell-2",
            temp_dir.path(),
            "greet ok".to_string(),
        )
        .await;
        drop(event_tx);

        let mut second_stdout = String::new();
        let mut second_cwd = None;
        let mut second_exit = None;
        while let Some(event) = event_rx.recv().await {
            if let BackendEvent::Public(SessionEvent::Nori(NoriEvent::UserShell(ev))) = event {
                match ev {
                    UserShellEvent::Started {
                        operation_id, cwd, ..
                    } if operation_id == "shell-2" => {
                        second_cwd = Some(cwd);
                    }
                    UserShellEvent::Output {
                        operation_id,
                        stream: UserShellStream::Stdout,
                        chunk,
                    } if operation_id == "shell-2" => {
                        second_stdout.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    UserShellEvent::Finished {
                        operation_id,
                        exit_code,
                    } if operation_id == "shell-2" => {
                        second_exit = Some(exit_code);
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(second_stdout, "brush-live:ok");
        assert_eq!(second_exit, Some(0));
        assert!(second_cwd.is_some_and(|cwd| cwd.ends_with("nested")));
    }
}
