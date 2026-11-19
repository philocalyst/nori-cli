//! Workflow loading must not block Cancel or Shutdown command handling.

#![cfg(unix)]
#![expect(clippy::expect_used, reason = "fixture failures should fail loudly")]

use std::ffi::CString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionLaunchSpec;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::launch_session;
use nori_harness::runtime::prepare_agent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp;
use pretty_assertions::assert_eq;
use serial_test::serial;

struct FifoRelease {
    release: Option<mpsc::Sender<()>>,
    opened: Option<tokio::sync::oneshot::Receiver<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl FifoRelease {
    async fn wait_until_policy_reads(&mut self) {
        let opened = self.opened.take().expect("one FIFO open notification");
        // Wait for the native evaluator's resource read, independently of admission.
        // Cancellation and shutdown retain their two-second responsiveness deadlines.
        tokio::time::timeout(Duration::from_secs(120), opened)
            .await
            .expect("Pkl policy should start reading its FIFO")
            .expect("FIFO writer should observe the policy reader");
    }

    fn release_and_join(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(writer) = self.writer.take() {
            writer.join().expect("release FIFO writer");
        }
    }
}

impl Drop for FifoRelease {
    fn drop(&mut self) {
        self.release_and_join();
    }
}

fn install_blocked_policy(home: &Path) -> FifoRelease {
    let fifo_path = home.join("policy-input");
    let path = CString::new(fifo_path.as_os_str().as_bytes()).expect("FIFO path");
    // SAFETY: the path is NUL-free and remains alive for the FIFO's lifetime.
    let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(
        result,
        0,
        "create Pkl input FIFO: {}",
        std::io::Error::last_os_error()
    );
    std::fs::write(
        home.join("init.pkl"),
        format!(
            "instructions = List(read(\"file://{}\").text)",
            fifo_path.display()
        ),
    )
    .expect("write Pkl policy");

    let (release_tx, release_rx) = mpsc::channel();
    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
    let writer = std::thread::spawn(move || {
        loop {
            if release_rx.try_recv().is_ok() {
                return;
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo_path)
            {
                Ok(mut fifo) => {
                    let _ = opened_tx.send(());
                    if release_rx.recv().is_ok() {
                        fifo.write_all(b"released policy")
                            .expect("write valid Pkl string");
                    }
                    return;
                }
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("open policy FIFO: {error}"),
            }
        }
    });
    FifoRelease {
        release: Some(release_tx),
        opened: Some(opened_rx),
        writer: Some(writer),
    }
}

async fn launch_with_blocked_policy(home: &Path) -> LaunchedSession {
    let config = NoriConfig {
        active_agent: "mock-model".to_owned(),
        cwd: home.to_path_buf(),
        nori_home: home.to_path_buf(),
        ..Default::default()
    };
    let agent = prepare_agent(AgentPrepareSpec {
        handoff: None,
        config: Arc::new(config),
        cli_version: "workflow-prompt-liveness-test".to_owned(),
        session_context: None,
        initial_context: None,
    })
    .await
    .expect("prepare mock agent");
    let mut session = launch_session(SessionLaunchSpec {
        agent,
        start: SessionStart::New,
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match session.events.recv().await.expect("session event stream") {
                SessionEvent::Nori(NoriEvent::SessionStarted(_)) => return,
                SessionEvent::Nori(NoriEvent::SessionEnded(ended)) => {
                    panic!("session ended before startup: {ended:?}")
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("session should start");
    session
}

fn prompt(text: &str) -> Vec<acp::v1::ContentBlock> {
    vec![acp::v1::ContentBlock::Text(acp::v1::TextContent::new(text))]
}

#[tokio::test]
#[serial]
async fn cancel_and_shutdown_run_while_policy_read_is_blocked() {
    let home = tempfile::tempdir().expect("session home");
    let mut fifo = install_blocked_policy(home.path());
    let session = launch_with_blocked_policy(home.path()).await;

    let (cancel_queued_tx, cancel_queued_rx) = tokio::sync::oneshot::channel();
    let cancel_prompt = {
        let handle = session.handle.clone();
        tokio::spawn(async move {
            let prompt = handle.prompt(prompt("cancel while loading"));
            tokio::pin!(prompt);
            let _ = futures::poll!(&mut prompt);
            let _ = cancel_queued_tx.send(());
            prompt.await
        })
    };
    cancel_queued_rx.await.expect("prompt command was queued");
    let _ = session.handle.get_session_config().await;
    fifo.wait_until_policy_reads().await;
    tokio::time::timeout(Duration::from_secs(2), session.handle.cancel())
        .await
        .expect("Cancel must remain serviceable during policy loading")
        .expect("cancel active or pending work");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), cancel_prompt)
            .await
            .expect("canceled prompt admission should finish")
            .expect("prompt task")
            .is_err()
    );

    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
    let shutdown_prompt = {
        let handle = session.handle.clone();
        tokio::spawn(async move {
            let prompt = handle.prompt(prompt("shutdown while loading"));
            tokio::pin!(prompt);
            let _ = futures::poll!(&mut prompt);
            let _ = queued_tx.send(());
            prompt.await
        })
    };
    queued_rx.await.expect("prompt command was queued");
    let _ = session.handle.get_session_config().await;
    tokio::time::timeout(Duration::from_secs(2), session.handle.shutdown())
        .await
        .expect("Shutdown must remain serviceable during policy loading")
        .expect("shutdown session");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), shutdown_prompt)
            .await
            .expect("shutdown prompt admission should finish")
            .expect("prompt task")
            .is_err()
    );
    fifo.release_and_join();
}

#[tokio::test]
#[serial]
async fn rewind_rejects_a_prompt_blocked_in_policy_admission() {
    let home = tempfile::tempdir().expect("session home");
    let mut fifo = install_blocked_policy(home.path());
    let session = launch_with_blocked_policy(home.path()).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .expect("recorder request")
        .expect("journal");
    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
    let prompt_task = {
        let handle = session.handle.clone();
        tokio::spawn(async move {
            let prompt = handle.prompt(prompt("pending before rewind"));
            tokio::pin!(prompt);
            let _ = futures::poll!(&mut prompt);
            let _ = queued_tx.send(());
            prompt.await
        })
    };
    queued_rx.await.expect("prompt command was queued");
    let _ = session.handle.get_session_config().await;
    fifo.wait_until_policy_reads().await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        session
            .handle
            .rewind_and_rebind(recorder.session_id().to_owned(), None),
    )
    .await
    .expect("rewind remains responsive")
    .expect_err("pending prompt must block rewind");
    assert!(
        error.to_string().contains("prompt is being admitted"),
        "{error}"
    );
    session
        .handle
        .cancel()
        .await
        .expect("cancel blocked prompt");
    fifo.release_and_join();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), prompt_task)
            .await
            .expect("prompt finishes")
            .expect("prompt task")
            .is_err()
    );
    session.handle.shutdown().await.expect("shutdown");
}
