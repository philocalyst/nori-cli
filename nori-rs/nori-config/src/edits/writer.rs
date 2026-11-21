//! The config writer: one thread that applies every config edit of the process, in the order
//! they were submitted.
//!
//! An edit is a read, a change and a replacement of `config.toml`, and the replacement is flushed
//! to disk (5 to 150 ms on macOS). Run on whatever thread asked, it stalled the interface's event
//! loop for that long, and two edits made at once each read the file before the other wrote it,
//! so one of them was lost. Here an edit never runs on the asker's thread, and edits run one at a
//! time, each reading what the one before wrote.

use std::sync::OnceLock;
use std::sync::mpsc;

use anyhow::Result;

use super::NoriConfigEdits;

struct Job {
    edits: NoriConfigEdits,
    done: Option<tokio::sync::oneshot::Sender<Result<()>>>,
}

/// Queue `edits`; `done` hears how they went. Without a writer thread (the process cannot start
/// one), the edit is applied here, as it always was.
pub(super) fn submit(
    edits: NoriConfigEdits,
    done: Option<tokio::sync::oneshot::Sender<Result<()>>>,
) {
    static WRITER: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
    let writer = WRITER.get_or_init(|| {
        let (jobs, queue) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("nori-config-writer".into())
            .spawn(move || {
                while let Ok(job) = queue.recv() {
                    run(job);
                }
            })
            .ok()
            .map(|_| jobs)
    });
    let job = Job { edits, done };
    match writer {
        Some(jobs) => {
            if let Err(mpsc::SendError(job)) = jobs.send(job) {
                run(job);
            }
        }
        None => run(job),
    }
}

fn run(Job { edits, done }: Job) {
    let path = edits.nori_home.join(crate::CONFIG_FILE);
    let result = edits.apply_blocking();
    match done {
        Some(done) => {
            let _ = done.send(result);
        }
        None => {
            if let Err(error) = result {
                tracing::warn!(path = %path.display(), "could not save config.toml: {error:#}");
            }
        }
    }
}
