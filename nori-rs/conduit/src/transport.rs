//! Carry envelopes between a run and another world.
//!
//! A [`Transport`] is a duplex of [`Envelope`]s: the run sends everything it does, and
//! receives what the other world asks of it (signals, interrupt answers, cancellation).
//! [`FileTransport`] is the run journal and an inbox file; [`ChannelTransport`] is an
//! in-process pair for a supervisor in the same process (and for tests); a daemon or BEAM
//! node implements the same trait over a socket or port. [`FanoutTransport`] mirrors one
//! run to several of them.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::ConduitError;
use crate::wire::Envelope;
use crate::wire::WireOp;

#[async_trait]
pub trait Transport: Send + Sync {
    /// Deliver one envelope. An error means the envelope may not have been recorded.
    async fn send(&self, envelope: &Envelope) -> Result<(), ConduitError>;

    /// The next envelope from the other world, waiting for one. `Ok(None)` means the other
    /// side is gone for good.
    async fn recv(&self) -> Result<Option<Envelope>, ConduitError>;
}

fn journal_error(action: &str, path: &Path, error: impl std::fmt::Display) -> ConduitError {
    ConduitError::Journal {
        message: format!("cannot {action} {}: {error}", path.display()),
    }
}

/// `journal.jsonl` (outbound, append-only) and `inbox.jsonl` (inbound, tailed) in one directory.
pub struct FileTransport {
    journal_path: PathBuf,
    inbox_path: PathBuf,
    journal: Mutex<File>,
    inbox_offset: Mutex<u64>,
}

impl FileTransport {
    pub const JOURNAL: &'static str = "journal.jsonl";
    pub const INBOX: &'static str = "inbox.jsonl";

    /// Create `dir` if needed and open (or continue) its journal.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, ConduitError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|error| journal_error("create", dir, error))?;
        let journal_path = dir.join(Self::JOURNAL);
        let journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal_path)
            .map_err(|error| journal_error("open", &journal_path, error))?;
        Ok(Self {
            journal_path,
            inbox_path: dir.join(Self::INBOX),
            journal: Mutex::new(journal),
            inbox_offset: Mutex::new(0),
        })
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    /// Read every envelope of a journal, rejecting unknown versions and damaged lines.
    pub fn read_journal(path: impl AsRef<Path>) -> Result<Vec<Envelope>, ConduitError> {
        let path = path.as_ref();
        let text =
            std::fs::read_to_string(path).map_err(|error| journal_error("read", path, error))?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(index, line)| {
                Envelope::from_line(line).map_err(|error| ConduitError::Journal {
                    message: format!("{}:{}: {error}", path.display(), index + 1),
                })
            })
            .collect()
    }

    fn next_inbound(&self) -> Result<Option<Envelope>, ConduitError> {
        let mut file = match File::open(&self.inbox_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(journal_error("open", &self.inbox_path, error)),
        };
        let mut offset = self
            .inbox_offset
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.seek(SeekFrom::Start(*offset))
            .map_err(|error| journal_error("seek", &self.inbox_path, error))?;
        let mut pending = String::new();
        file.read_to_string(&mut pending)
            .map_err(|error| journal_error("read", &self.inbox_path, error))?;
        // Only a complete line is an envelope; a writer may be mid-append.
        let Some(end) = pending.find('\n') else {
            return Ok(None);
        };
        let line = &pending[..end];
        *offset += u64::try_from(end + 1).unwrap_or(0);
        if line.trim().is_empty() {
            return Ok(None);
        }
        Envelope::from_line(line).map(Some)
    }
}

#[async_trait]
impl Transport for FileTransport {
    async fn send(&self, envelope: &Envelope) -> Result<(), ConduitError> {
        let mut line = envelope.to_line()?;
        line.push('\n');
        let mut file = self
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.write_all(line.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|error| journal_error("append to", &self.journal_path, error))?;
        if matches!(
            envelope.op,
            WireOp::RunFinished | WireOp::RunFailed | WireOp::InterruptRequested
        ) {
            file.sync_data()
                .map_err(|error| journal_error("sync", &self.journal_path, error))?;
        }
        Ok(())
    }

    async fn recv(&self) -> Result<Option<Envelope>, ConduitError> {
        loop {
            if let Some(envelope) = self.next_inbound()? {
                return Ok(Some(envelope));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// One end of an in-process pair: what one end sends the other receives.
pub struct ChannelTransport {
    outbound: mpsc::UnboundedSender<Envelope>,
    inbound: tokio::sync::Mutex<mpsc::UnboundedReceiver<Envelope>>,
}

impl ChannelTransport {
    /// Two connected ends: give one to the run, keep the other as the second world.
    pub fn pair() -> (Self, Self) {
        let (a_tx, b_rx) = mpsc::unbounded_channel();
        let (b_tx, a_rx) = mpsc::unbounded_channel();
        (
            Self {
                outbound: a_tx,
                inbound: tokio::sync::Mutex::new(a_rx),
            },
            Self {
                outbound: b_tx,
                inbound: tokio::sync::Mutex::new(b_rx),
            },
        )
    }
}

#[async_trait]
impl Transport for ChannelTransport {
    async fn send(&self, envelope: &Envelope) -> Result<(), ConduitError> {
        self.outbound
            .send(envelope.clone())
            .map_err(|_| ConduitError::Wire {
                message: "the other end of the channel transport is closed".to_owned(),
            })
    }

    async fn recv(&self) -> Result<Option<Envelope>, ConduitError> {
        Ok(self.inbound.lock().await.recv().await)
    }
}

/// Send to a primary transport and mirror to others. A failure of the primary is a failure of
/// the send; a failing mirror is counted and the run continues, so a dead observer never stops
/// the program. Inbound envelopes come from whichever member has one first.
pub struct FanoutTransport {
    primary: Arc<dyn Transport>,
    mirrors: Vec<Arc<dyn Transport>>,
    mirror_failures: AtomicI64,
}

impl FanoutTransport {
    pub fn new(primary: Arc<dyn Transport>, mirrors: Vec<Arc<dyn Transport>>) -> Self {
        Self {
            primary,
            mirrors,
            mirror_failures: AtomicI64::new(0),
        }
    }

    /// Envelopes a mirror failed to take.
    pub fn mirror_failures(&self) -> i64 {
        self.mirror_failures.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Transport for FanoutTransport {
    async fn send(&self, envelope: &Envelope) -> Result<(), ConduitError> {
        self.primary.send(envelope).await?;
        for mirror in &self.mirrors {
            if mirror.send(envelope).await.is_err() {
                self.mirror_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    async fn recv(&self) -> Result<Option<Envelope>, ConduitError> {
        let members = std::iter::once(&self.primary).chain(self.mirrors.iter());
        let mut pending =
            futures::future::select_all(members.map(|member| Box::pin(member.recv())));
        loop {
            let (outcome, _, rest) = pending.await;
            match outcome? {
                Some(envelope) => return Ok(Some(envelope)),
                None if rest.is_empty() => return Ok(None),
                None => pending = futures::future::select_all(rest),
            }
        }
    }
}

/// Keeps every envelope in memory: the journal of a run with no directory, and the record a
/// caller reads back after the run.
#[derive(Default)]
pub struct RecordingTransport {
    envelopes: Mutex<Vec<Envelope>>,
}

impl RecordingTransport {
    pub fn envelopes(&self) -> Vec<Envelope> {
        self.envelopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn send(&self, envelope: &Envelope) -> Result<(), ConduitError> {
        self.envelopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(envelope.clone());
        Ok(())
    }

    /// Nothing ever arrives; waits until the run ends.
    async fn recv(&self) -> Result<Option<Envelope>, ConduitError> {
        std::future::pending().await
    }
}
