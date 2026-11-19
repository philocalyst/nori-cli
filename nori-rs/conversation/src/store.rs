//! Transport seam for append-only journals. Versions are signed byte counts.
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

/// Compare and append must be atomic with respect to all writers using this store.
/// A failed comparison returns `WouldBlock`; callers can reload and retry.
pub trait ConversationStore: Send {
    fn read(&self) -> io::Result<Vec<u8>>;
    fn append(&mut self, expected_version: i64, bytes: &[u8]) -> io::Result<()>;
}

/// File journal with OS-level locking across compare, append, and sync.
pub struct FileStore {
    path: PathBuf,
}
impl FileStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}
impl ConversationStore for FileStore {
    fn read(&self) -> io::Result<Vec<u8>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        file.lock_shared()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    fn append(&mut self, expected_version: i64, bytes: &[u8]) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        file.lock()?;
        let actual = i64::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
        if actual != expected_version {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "journal changed"));
        }
        file.write_all(bytes)?;
        file.sync_data()
    }
}

/// Cloneable in-memory transport with atomic compare-and-append.
#[derive(Clone, Default)]
pub struct MemoryStore {
    bytes: Arc<Mutex<Vec<u8>>>,
}
impl ConversationStore for MemoryStore {
    fn read(&self) -> io::Result<Vec<u8>> {
        Ok(self
            .bytes
            .lock()
            .map_err(|_| io::Error::other("store lock poisoned"))?
            .clone())
    }
    fn append(&mut self, expected_version: i64, bytes: &[u8]) -> io::Result<()> {
        let mut current = self
            .bytes
            .lock()
            .map_err(|_| io::Error::other("store lock poisoned"))?;
        if i64::try_from(current.len()).map_err(io::Error::other)? != expected_version {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "journal changed"));
        }
        current.extend_from_slice(bytes);
        Ok(())
    }
}
