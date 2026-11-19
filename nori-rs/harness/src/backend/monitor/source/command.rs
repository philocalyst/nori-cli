//! A shell command, run in the background in the in-process brush shell.

use std::collections::VecDeque;
use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::monitor::event::Ending;

/// The lines of stderr kept for a command that fails without stderr being watched.
const STDERR_TAIL: usize = 20;

pub(super) async fn run(
    command: &str,
    cwd: PathBuf,
    watch_stderr: bool,
    tx: &Sender<Item>,
) -> Option<Ending> {
    let mut session = match nori_bash::BashSession::new(cwd).await {
        Ok(session) => session,
        Err(error) => {
            return Some(Ending::Failed {
                reason: format!("the shell could not start: {error}"),
            });
        }
    };
    // The monitor's own timeout decides how long this runs.
    session.set_default_timeout_ms(None);
    let (lines_tx, mut lines) = mpsc::channel::<nori_bash::OutputLine>(256);
    let execution = session.exec_streaming(command, nori_bash::ExecOptions::default(), lines_tx);
    tokio::pin!(execution);
    let mut stderr_tail: VecDeque<String> = VecDeque::new();
    let result = loop {
        tokio::select! {
            Some(line) = lines.recv() => {
                if !forward(line, watch_stderr, &mut stderr_tail, tx).await {
                    return None;
                }
            }
            result = &mut execution => break result,
        }
    };
    // The readers finished before the command was reported done: what is left is in the channel.
    while let Ok(line) = lines.try_recv() {
        if !forward(line, watch_stderr, &mut stderr_tail, tx).await {
            return None;
        }
    }
    match result {
        Ok(output) => {
            if output.exit_code != 0 && !stderr_tail.is_empty() {
                for text in stderr_tail {
                    let line = Item::Line {
                        text: format!("stderr: {text}"),
                        failure: true,
                    };
                    if tx.send(line).await.is_err() {
                        return None;
                    }
                }
            }
            Some(Ending::Exited {
                code: output.exit_code,
            })
        }
        Err(error) => Some(Ending::Failed {
            reason: error.to_string(),
        }),
    }
}

/// Pass one line on. Returns `false` when nobody listens any more.
async fn forward(
    line: nori_bash::OutputLine,
    watch_stderr: bool,
    stderr_tail: &mut VecDeque<String>,
    tx: &Sender<Item>,
) -> bool {
    if line.stderr && !watch_stderr {
        if stderr_tail.len() == STDERR_TAIL {
            stderr_tail.pop_front();
        }
        stderr_tail.push_back(line.text);
        return true;
    }
    tx.send(Item::Line {
        text: line.text,
        failure: false,
    })
    .await
    .is_ok()
}
