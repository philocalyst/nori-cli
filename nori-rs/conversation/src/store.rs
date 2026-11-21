//! Transport seam for append-only journals. Versions are signed byte counts.
use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

/// Compare and append must be atomic with respect to all writers using this store.
/// A failed comparison returns `WouldBlock`; callers can reload and retry.
pub trait ConversationStore: Send {
    fn read(&self) -> io::Result<Vec<u8>>;
    /// The bytes from `offset` to the end. A log reads what was appended since it last read
    /// with this, not the whole journal again.
    fn read_from(&self, offset: i64) -> io::Result<Vec<u8>> {
        let bytes = self.read()?;
        let offset = usize::try_from(offset).map_err(io::Error::other)?;
        Ok(bytes.get(offset..).map(<[u8]>::to_vec).unwrap_or_default())
    }
    /// `len` bytes from `offset`: one record's line, or a run of them.
    fn read_range(&self, offset: i64, len: i64) -> io::Result<Vec<u8>> {
        let bytes = self.read()?;
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start
            .checked_add(usize::try_from(len).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("range past the end"))?;
        bytes
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "range past the end"))
    }
    fn append(&mut self, expected_version: i64, bytes: &[u8]) -> io::Result<()>;
    /// The journal's index as last saved beside it, if the store keeps one.
    fn load_index(&self) -> Option<Vec<u8>> {
        None
    }
    /// Keep the journal's index beside it for the next open. Losing it costs only time.
    fn save_index(&self, _index: &[u8]) {}
    /// Make everything appended so far durable now. A store that defers durability (the file
    /// store does: see [`FileStore`]) has to be told where a caller needs the barrier.
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How long appended records may wait for a disk flush. On macOS a flush is a full-drive cache
/// flush (`F_FULLFSYNC`) that takes 5 to 150 ms, so flushing after every record capped a
/// session at a few dozen events a second and held every event, title and config answer
/// behind it. A record is in the operating system's cache, visible to every reader and safe
/// from a crash of this process, the moment `append` returns; only a power loss or kernel
/// panic can lose the last interval.
const SYNC_INTERVAL: Duration = Duration::from_millis(400);

/// Flushes journals in the background, each at most once per [`SYNC_INTERVAL`] however many
/// records were appended to it meanwhile.
fn defer_sync(path: &Path) {
    static SYNCER: OnceLock<Option<Syncer>> = OnceLock::new();
    struct Syncer {
        pending: Arc<Mutex<HashSet<PathBuf>>>,
        wake: std::sync::mpsc::Sender<()>,
    }
    let syncer = SYNCER.get_or_init(|| {
        let pending: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
        let (wake, woken) = std::sync::mpsc::channel::<()>();
        let worker = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("journal-sync".into())
            .spawn(move || {
                while woken.recv().is_ok() {
                    std::thread::sleep(SYNC_INTERVAL);
                    while woken.try_recv().is_ok() {}
                    let paths = match worker.lock() {
                        Ok(mut pending) => std::mem::take(&mut *pending),
                        Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
                    };
                    for path in paths {
                        if let Ok(file) = OpenOptions::new().append(true).open(&path) {
                            let _ = file.sync_data();
                        }
                    }
                }
            })
            .ok()?;
        Some(Syncer { pending, wake })
    });
    let Some(syncer) = syncer else {
        // No thread to flush with: keep the old guarantee.
        if let Ok(file) = OpenOptions::new().append(true).open(path) {
            let _ = file.sync_data();
        }
        return;
    };
    let newly_pending = match syncer.pending.lock() {
        Ok(mut pending) => pending.insert(path.to_path_buf()),
        Err(poisoned) => poisoned.into_inner().insert(path.to_path_buf()),
    };
    if newly_pending {
        let _ = syncer.wake.send(());
    }
}

/// File journal with OS-level locking across compare and append. Records are written at once
/// and flushed to disk in the background (see [`SYNC_INTERVAL`]); [`ConversationStore::sync`]
/// flushes now.
pub struct FileStore {
    path: PathBuf,
    /// Kept open for reading records one at a time, once the file exists.
    reader: Mutex<Option<File>>,
}
impl FileStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            reader: Mutex::new(None),
        }
    }

    fn read_at(&self, offset: i64, buffer: &mut [u8]) -> io::Result<()> {
        let mut reader = self
            .reader
            .lock()
            .map_err(|_| io::Error::other("journal reader lock poisoned"))?;
        if reader.is_none() {
            *reader = Some(File::open(&self.path)?);
        }
        let Some(file) = reader.as_ref() else {
            return Err(io::Error::other("journal reader missing"));
        };
        let offset = u64::try_from(offset).map_err(io::Error::other)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            file.read_exact_at(buffer, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buffer.len() {
                let read = file.seek_read(&mut buffer[done..], offset + done as u64)?;
                if read == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                done += read;
            }
            Ok(())
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
    fn read_from(&self, offset: i64) -> io::Result<Vec<u8>> {
        use std::io::Seek;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        file.lock_shared()?;
        let len = i64::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
        if len <= offset {
            return Ok(Vec::new());
        }
        file.seek(io::SeekFrom::Start(
            u64::try_from(offset).map_err(io::Error::other)?,
        ))?;
        let mut bytes = Vec::with_capacity(usize::try_from(len - offset).unwrap_or(0));
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    fn read_range(&self, offset: i64, len: i64) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; usize::try_from(len).map_err(io::Error::other)?];
        self.read_at(offset, &mut bytes)?;
        Ok(bytes)
    }
    fn load_index(&self) -> Option<Vec<u8>> {
        std::fs::read(self.path.with_extension("pkx")).ok()
    }
    fn save_index(&self, index: &[u8]) {
        // Written whole beside the journal and moved into place, so a reader never sees half.
        let target = self.path.with_extension("pkx");
        let partial = self
            .path
            .with_extension(format!("pkx.{}.partial", std::process::id()));
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options
            .open(&partial)
            .and_then(|mut file| file.write_all(index))
            .and_then(|()| std::fs::rename(&partial, &target));
        if written.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
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
        drop(file);
        defer_sync(&self.path);
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        match OpenOptions::new().append(true).open(&self.path) {
            Ok(file) => file.sync_data(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
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
