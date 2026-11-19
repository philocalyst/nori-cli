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
use nori_harness::runtime::SessionLaunchSpec;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::launch_session;
use nori_harness::runtime::prepare_agent;
use nori_protocol::acp;
use pretty_assertions::assert_eq;

struct FifoRelease {
    release: Option<mpsc::Sender<()>>,
    opened: Option<tokio::sync::oneshot::Receiver<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl FifoRelease {
    async fn wait_until_policy_reads(&mut self) {
        let opened = self.opened.take().expect("one FIFO open notification");
        // Real Pkl starts a JVM; under a full workspace build its startup can be slow.
        // The cancellation and shutdown assertions below keep their two-second deadlines.
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
            "tools = new Dynamic {{ enabled = List(\"get_goal\"); instructions = List(read(\"file://{}\").text) }}",
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

#[tokio::test]
async fn cancel_and_shutdown_are_responsive_during_tool_policy_preparation() {
    nori_harness::set_stdio_bridge(
        std::path::PathBuf::from("nori"),
        vec!["stdio-to-uds".into()],
    );
    let home = tempfile::tempdir().unwrap();
    let mut fifo = install_blocked_policy(home.path());
    let config = NoriConfig {
        active_agent: "mock-model".to_owned(),
        cwd: home.path().to_path_buf(),
        nori_home: home.path().to_path_buf(),
        ..Default::default()
    };
    let agent = prepare_agent(AgentPrepareSpec {
        handoff: None,
        config: Arc::new(config),
        cli_version: "tool-policy-test".into(),
        session_context: None,
        initial_context: None,
    })
    .await
    .unwrap();
    let session = launch_session(SessionLaunchSpec {
        agent,
        start: SessionStart::New,
    });
    fifo.wait_until_policy_reads().await;
    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
    let prompt = {
        let handle = session.handle.clone();
        tokio::spawn(async move {
            let prompt = handle.prompt(vec![acp::v1::ContentBlock::Text(
                acp::v1::TextContent::new("pending"),
            )]);
            tokio::pin!(prompt);
            let result = futures::poll!(&mut prompt);
            let _ = queued_tx.send(());
            match result {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => prompt.await,
            }
        })
    };
    queued_rx.await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), session.handle.get_session_config())
            .await
            .unwrap(),
        Some(Vec::new())
    );
    tokio::time::timeout(Duration::from_secs(2), session.handle.cancel())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), prompt)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), session.handle.shutdown())
        .await
        .unwrap()
        .unwrap();
    fifo.release_and_join();
}
