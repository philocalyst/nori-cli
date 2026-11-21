//! Verify the real terminal stream: what Nori reports to its terminal through OSC 7501, with the
//! terminator, ids, kinds and the clearing, as a decoded sequence of records.

#[cfg(unix)]
mod support {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use std::collections::HashMap;
    use std::time::Duration;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TuiSession;

    /// One decoded report; `title` and `msg` are decoded text.
    pub type Report = HashMap<String, String>;

    /// Every OSC 7501 report in the captured output, in order. `?` queries are not reports.
    pub fn reports(session: &TuiSession) -> Vec<Report> {
        let output = String::from_utf8_lossy(session.output_bytes().unwrap()).into_owned();
        let mut found = Vec::new();
        let mut rest = output.as_str();
        while let Some(start) = rest.find("\x1b]7501;") {
            rest = &rest[start + "\x1b]7501;".len()..];
            let Some(end) = rest.find("\x1b\\") else {
                break;
            };
            let body = &rest[..end];
            rest = &rest[end..];
            if body.starts_with('?') {
                continue;
            }
            found.push(
                body.split(':')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, value)| {
                        let value = if matches!(key, "title" | "msg") {
                            String::from_utf8(STANDARD.decode(value).unwrap()).unwrap()
                        } else {
                            value.to_owned()
                        };
                        (key.to_owned(), value)
                    })
                    .collect(),
            );
        }
        found
    }

    pub fn field<'a>(report: &'a Report, key: &str) -> &'a str {
        report.get(key).map_or("", String::as_str)
    }

    pub fn matches(report: &Report, pairs: &[(&str, &str)]) -> bool {
        pairs
            .iter()
            .all(|(key, value)| field(report, key) == *value)
    }

    /// Wait until `done` holds for the reports so far.
    pub fn wait_for_reports(
        session: &mut TuiSession,
        what: &str,
        done: impl Fn(&[Report]) -> bool,
    ) {
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            session.poll().unwrap();
            let seen = reports(session);
            if done(&seen) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing {what}; saw {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
#[test]
fn reports_permission_work_completion_and_exit() {
    use std::time::Duration;
    use support::field;
    use support::matches;
    use support::reports;
    use support::wait_for_reports;
    use tui_pty_e2e::Key;
    use tui_pty_e2e::SessionConfig;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TIMEOUT_INPUT;
    use tui_pty_e2e::TuiSession;
    let config = SessionConfig::new()
        .with_agent("mock-model".to_owned())
        .with_agent_env("NORI_PROGRAM_STATUS", "on")
        .with_agent_env("NORI_TEST_REQUIRE_APPROVAL", "1")
        .with_agent_env("MOCK_AGENT_REQUEST_PERMISSION", "1");
    let mut session = TuiSession::spawn_with_config(24, 80, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();
    session.submit_input("Run a command").unwrap();
    // The approval prompt, whatever its wording: its first choice allows.
    session.wait_for_text("Allow (y)", TIMEOUT).unwrap();
    wait_for_reports(&mut session, "blocked permission", |seen| {
        seen.iter()
            .any(|r| matches(r, &[("state", "blocked"), ("kind", "permission")]))
    });
    session.send_key(Key::Char('y')).unwrap();
    wait_for_reports(&mut session, "completed status", |seen| {
        seen.iter().any(|r| matches(r, &[("state", "done")]))
    });
    session.submit_input("/quit").unwrap();
    assert!(session.wait_for_process_exit(Duration::from_secs(10)));
    let _ = session.poll();
    let seen = reports(&session);
    let position = |pairs: &[(&str, &str)]| seen.iter().position(|r| matches(r, pairs));
    let working = position(&[("state", "working")]).expect("missing working");
    let blocked = position(&[("state", "blocked"), ("kind", "permission")]).unwrap();
    let done = position(&[("state", "done")]).unwrap();
    assert!(working < blocked && blocked < done, "order: {seen:?}");
    assert!(
        seen.iter()
            .all(|r| field(r, "app") == "nori" || field(r, "state") == "clear")
    );
    assert!(
        field(&seen[blocked], "msg").starts_with("Approve "),
        "blocked says what is asked: {:?}",
        seen[blocked]
    );
    assert!(
        field(&seen[done], "msg").starts_with("Finished"),
        "done summarizes the turn: {:?}",
        seen[done]
    );
    let last = seen.last().unwrap();
    assert!(
        matches(last, &[("state", "clear")]) && !last.contains_key("id"),
        "the final report clears everything: {last:?}"
    );
}

#[cfg(unix)]
#[test]
fn reports_nested_children_independently_from_the_root() {
    use support::field;
    use support::matches;
    use support::reports;
    use support::wait_for_reports;
    use tui_pty_e2e::Key;
    use tui_pty_e2e::SessionConfig;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TIMEOUT_INPUT;
    use tui_pty_e2e::TuiSession;
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/program_status_nested.jsonl"
    );
    let config = SessionConfig::new()
        .with_agent_env("NORI_PROGRAM_STATUS", "on")
        .with_agent_env("MOCK_AGENT_WIRE_REPLAY", fixture);
    let mut session = TuiSession::spawn_with_config(24, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();
    session.submit_input("Start nested work").unwrap();
    session
        .wait_for_text("Root work complete", TIMEOUT)
        .unwrap();
    wait_for_reports(&mut session, "completed root", |seen| {
        seen.iter()
            .any(|r| matches(r, &[("state", "done")]) && !r.contains_key("id"))
    });
    session.send_key(Key::Char(' ')).unwrap();
    wait_for_reports(&mut session, "acknowledged root and closed child", |seen| {
        let done = seen
            .iter()
            .position(|r| matches(r, &[("state", "done")]) && !r.contains_key("id"));
        done.is_some_and(|done| {
            seen[done..]
                .iter()
                .any(|r| matches(r, &[("state", "idle")]) && !r.contains_key("id"))
        }) && seen
            .iter()
            .any(|r| matches(r, &[("state", "clear"), ("id", "s1")]))
    });
    let seen = reports(&session);
    // The children are named as Nori names them: the task, with the type as the detail.
    let child = seen
        .iter()
        .find(|r| matches(r, &[("state", "working"), ("id", "s1")]))
        .expect("child working");
    assert_eq!(field(child, "title"), "Parent task");
    assert_eq!(field(child, "msg"), "First child");
    let grandchild = seen
        .iter()
        .find(|r| matches(r, &[("state", "working"), ("id", "s1/s2")]))
        .expect("grandchild working");
    assert_eq!(field(grandchild, "title"), "Nested task");
}

#[cfg(unix)]
#[test]
fn reports_and_clears_authentication_block_for_login() {
    use std::os::unix::fs::PermissionsExt;
    use support::matches;
    use support::wait_for_reports;
    use tui_pty_e2e::Key;
    use tui_pty_e2e::SessionConfig;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TIMEOUT_INPUT;
    use tui_pty_e2e::TuiSession;
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("claude");
    std::fs::write(
        &cli,
        "#!/bin/sh\nprintf 'AUTH_FIXTURE_READY\\n'\nread answer\n",
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = SessionConfig::new()
        .with_agent_env("NORI_PROGRAM_STATUS", "on")
        .with_extra_path(dir.path().to_owned());
    let mut session = TuiSession::spawn_with_config(24, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();
    session.submit_input("/login claude-code").unwrap();
    session
        .wait_for_text("AUTH_FIXTURE_READY", TIMEOUT)
        .unwrap();
    session.send_key(Key::Escape).unwrap();
    wait_for_reports(&mut session, "authentication block cleared", |seen| {
        let blocked = seen
            .iter()
            .position(|r| matches(r, &[("state", "blocked"), ("kind", "auth")]));
        blocked.is_some_and(|blocked| {
            seen[blocked..]
                .iter()
                .any(|r| matches(r, &[("state", "idle")]) && !r.contains_key("id"))
        })
    });
}

/// A terminal that does not answer the capability query, or a user who opted out, gets nothing.
#[cfg(unix)]
#[test]
fn stays_silent_without_a_terminal_that_answers_or_when_off() {
    use std::time::Duration;
    use support::reports;
    use tui_pty_e2e::SessionConfig;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TIMEOUT_INPUT;
    use tui_pty_e2e::TuiSession;
    for setting in [None, Some("off")] {
        let mut config = SessionConfig::new().with_agent("mock-model".to_owned());
        if let Some(setting) = setting {
            config = config.with_agent_env("NORI_PROGRAM_STATUS", setting);
        }
        let mut session = TuiSession::spawn_with_config(24, 80, config).unwrap();
        session.wait_for_text("›", TIMEOUT).unwrap();
        std::thread::sleep(TIMEOUT_INPUT);
        session.start_output_capture();
        session.submit_input("hello").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let _ = session.poll();
        assert_eq!(
            reports(&session),
            Vec::<support::Report>::new(),
            "setting {setting:?}"
        );
    }
}
