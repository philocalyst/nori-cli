#[cfg(all(feature = "login", unix))]
mod unix_tests {
    use super::super::*;
    use crate::app_event::AppEvent;
    use crate::login_handler::LoginHandler;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;

    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("write fake CLI");
        let mut permissions = std::fs::metadata(path)
            .expect("stat fake CLI")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).expect("make fake CLI executable");
    }

    async fn wait_for_output(
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        expected: &str,
    ) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = String::new();
            loop {
                match event_rx.recv().await.expect("app event channel stays open") {
                    AppEvent::ExternalCliLoginOutput { data } => {
                        output.push_str(&String::from_utf8_lossy(&data));
                        if output.contains(expected) {
                            return output;
                        }
                    }
                    AppEvent::ExternalCliLoginComplete { success, agent_name } => {
                        panic!("login completed before {expected:?}: success={success}, agent={agent_name}");
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for login output {expected:?}"))
    }

    async fn wait_for_completion(
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) -> (bool, String) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let AppEvent::ExternalCliLoginComplete {
                    success,
                    agent_name,
                } = event_rx.recv().await.expect("app event channel stays open")
                {
                    return (success, agent_name);
                }
            }
        })
        .await
        .expect("login completion event arrives")
    }

    #[tokio::test]
    async fn fake_claude_login_streams_output_and_accepts_enter_and_pasted_code() {
        let temp = tempfile::tempdir().expect("temp directory");
        let cli = temp.path().join("claude");
        write_executable(
            &cli,
            "#!/bin/sh\nprintf 'ARGS:%s\\n' \"$*\"\nprintf 'Paste code, then press Enter\\n'\nIFS= read -r answer\nprintf 'AUTH_OK:%s\\n' \"$answer\"\n",
        );

        let (mut chat, mut event_rx, _unused_rx) = make_chatwidget_manual();
        chat.start_external_cli_login_flow(
            LoginHandler::new(),
            cli.to_string_lossy().into_owned(),
            vec!["auth".into(), "login".into(), "--claudeai".into()],
            "Claude Code".into(),
        );

        let output = wait_for_output(&mut event_rx, "Paste code").await;
        assert!(output.contains("ARGS:auth login --claudeai"));
        assert!(chat.route_external_cli_login_paste("fixture-code".into()));
        assert!(
            chat.route_external_cli_login_key_event(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))
        );
        assert!(
            chat.composer_is_empty(),
            "pasted auth input must not enter the composer"
        );

        let output = wait_for_output(&mut event_rx, "AUTH_OK:fixture-code").await;
        assert!(output.contains("AUTH_OK:fixture-code"));
        assert_eq!(
            wait_for_completion(&mut event_rx).await,
            (true, "Claude Code".into())
        );
    }

    fn process_is_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    async fn assert_cancel_reaps_fake_cli(key: KeyEvent) {
        let temp = tempfile::tempdir().expect("temp directory");
        let cli = temp.path().join("claude");
        let pid_file = temp.path().join("child.pid");
        write_executable(
            &cli,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$1\"\nprintf 'WAITING_FOR_INPUT\\n'\nwhile IFS= read -r line; do :; done\n",
        );

        let (mut chat, mut event_rx, _unused_rx) = make_chatwidget_manual();
        chat.start_external_cli_login_flow(
            LoginHandler::new(),
            cli.to_string_lossy().into_owned(),
            vec![pid_file.to_string_lossy().into_owned()],
            "Claude Code".into(),
        );
        wait_for_output(&mut event_rx, "WAITING_FOR_INPUT").await;
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake CLI writes its PID");
        assert!(
            process_is_alive(pid),
            "fake CLI should be blocked on PTY input"
        );

        assert!(chat.route_external_cli_login_key_event(key));
        assert!(
            chat.login_handler.is_none(),
            "cancel should clear active login state"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while process_is_alive(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancelling login should kill and reap the PTY child");

        assert!(
            tokio::time::timeout(Duration::from_millis(150), async {
                loop {
                    if matches!(
                        event_rx.recv().await,
                        Some(AppEvent::ExternalCliLoginComplete { .. })
                    ) {
                        return true;
                    }
                }
            })
            .await
            .is_err(),
            "cancelled login must not later publish completion"
        );
    }

    #[tokio::test]
    async fn escape_cancels_and_reaps_external_cli_child() {
        assert_cancel_reaps_fake_cli(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).await;
    }

    #[tokio::test]
    async fn ctrl_c_cancels_and_reaps_external_cli_child() {
        assert_cancel_reaps_fake_cli(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
    }
}
