use crate::ConversationStore;
use crate::Event;
use crate::EventId;
use crate::FileStore;
use crate::Projection;
use crate::Record;
use crate::SliceRef;
use crate::codec;
use std::io;
use std::path::Path;

const INTERRUPTED: &str = " // nori-interrupted-record";

pub struct ConversationLog {
    store: Box<dyn ConversationStore>,
    records: Vec<Record>,
    projection: Projection,
    version: i64,
    interrupted: bool,
    poisoned: bool,
}

impl ConversationLog {
    /// Open an existing file or prepare a new one; no bytes are written until append.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::with_store(FileStore::new(path))
    }

    /// Load an append-only transport. A store must atomically compare and append.
    pub fn with_store(store: impl ConversationStore + 'static) -> io::Result<Self> {
        let mut log = Self {
            store: Box::new(store),
            records: Vec::new(),
            projection: Projection::default(),
            version: 0,
            interrupted: false,
            poisoned: false,
        };
        log.refresh()?;
        Ok(log)
    }

    /// Reload the journal without evaluating recorded operations.
    pub fn refresh(&mut self) -> io::Result<()> {
        let data = self.store.read()?;
        let version = i64::try_from(data.len()).map_err(io::Error::other)?;
        let mut records: Vec<Record> = Vec::new();
        let mut projection = Projection::default();
        let mut interrupted = false;
        let mut command_ids = std::collections::HashSet::new();
        for bytes in data.split_inclusive(|b| *b == b'\n') {
            if !bytes.ends_with(b"\n") {
                interrupted = true;
                break;
            }
            let line = std::str::from_utf8(&bytes[..bytes.len() - 1])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if line.ends_with(INTERRUPTED) {
                continue;
            }
            let record = codec::decode(line)?;
            let expected_id = records
                .last()
                .map_or(Some(1), |previous| previous.id.checked_add(1));
            if expected_id != Some(record.id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "nonconsecutive event id",
                ));
            }
            if let Some(id) = &record.command_id
                && !command_ids.insert(id.clone())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate command id",
                ));
            }
            projection.apply(&record)?;
            records.push(record);
        }
        self.records = records;
        self.projection = projection;
        self.version = version;
        self.interrupted = interrupted;
        self.poisoned = false;
        Ok(())
    }

    /// Read the records of the log at `path` in order, without loading the whole log, until `f`
    /// breaks. Records are not checked against each other and nothing is projected, so this is
    /// for reading a few facts (metadata, a first message, a count) out of a long log quickly.
    /// A record cut short by a crash ends the scan.
    pub fn scan(
        path: impl AsRef<Path>,
        mut f: impl FnMut(Record) -> std::ops::ControlFlow<()>,
    ) -> io::Result<()> {
        use std::io::BufRead;
        let file = std::fs::File::open(path)?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 || !line.ends_with('\n') {
                return Ok(());
            }
            let text = line.trim_end_matches('\n');
            if text.ends_with(INTERRUPTED) {
                continue;
            }
            if f(codec::decode(text)?).is_break() {
                return Ok(());
            }
        }
    }

    /// Resolve a slice at an earlier event, including a branch hidden by rewind.
    pub fn slice_at(&self, at: EventId, reference: SliceRef) -> io::Result<Option<Vec<Record>>> {
        let mut projection = Projection::default();
        for record in &self.records {
            projection.apply(record)?;
            if record.id == at {
                return Ok(projection.slice(reference));
            }
        }
        Ok(None)
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }
    pub fn projection(&self) -> &Projection {
        &self.projection
    }
    pub fn next_id(&self) -> io::Result<EventId> {
        self.records
            .last()
            .map_or(Some(1), |r| r.id.checked_add(1))
            .ok_or_else(|| io::Error::other("event id exhausted"))
    }

    /// Append at this instance's current head. On conflict, refresh and retry.
    pub fn append(&mut self, event: Event) -> io::Result<Record> {
        self.append_inner(self.projection.head(), None, event)
    }

    /// Append against an expected visible head, deduplicating a stable command ID.
    /// Retrying the same ID and event returns its original record even after rewind.
    pub fn append_with_id(
        &mut self,
        expected_head: Option<EventId>,
        command_id: &str,
        event: Event,
    ) -> io::Result<Record> {
        if command_id.is_empty()
            || !command_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command id must contain only ASCII letters, digits, hyphens or underscores",
            ));
        }
        self.append_inner(expected_head, Some(command_id.to_owned()), event)
    }

    fn append_inner(
        &mut self,
        expected_head: Option<EventId>,
        command_id: Option<String>,
        event: Event,
    ) -> io::Result<Record> {
        if self.poisoned {
            return Err(io::Error::other(
                "log must be refreshed after an incomplete write",
            ));
        }
        if matches!(event, Event::Unknown { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown events cannot be appended",
            ));
        }
        let mut refreshed = false;
        loop {
            if let Some(id) = &command_id
                && let Some(record) = self
                    .records
                    .iter()
                    .find(|r| r.command_id.as_ref() == Some(id))
            {
                if record.event == event {
                    return Ok(record.clone());
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "command id already used for another event",
                ));
            }
            if self.projection.head() != expected_head {
                if !refreshed {
                    self.refresh()?;
                    refreshed = true;
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "conversation head changed",
                ));
            }
            let record = Record {
                id: self.next_id()?,
                command_id: command_id.clone(),
                event: event.clone(),
            };
            let change = self.projection.prepare(&record)?;
            let mut bytes = Vec::new();
            if self.interrupted {
                bytes.extend_from_slice(format!("{INTERRUPTED}\n").as_bytes());
            }
            bytes.extend_from_slice(codec::encode(&record)?.as_bytes());
            let version = self
                .version
                .checked_add(i64::try_from(bytes.len()).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("journal version exhausted"))?;
            self.poisoned = true;
            if let Err(error) = self.store.append(self.version, &bytes) {
                // Failed comparisons cannot write bytes. Reload only on contention;
                // normal streamed events never reparse or clone the transcript.
                if error.kind() == io::ErrorKind::WouldBlock {
                    self.poisoned = false;
                    if !refreshed {
                        self.refresh()?;
                        refreshed = true;
                        continue;
                    }
                }
                return Err(error);
            }
            self.poisoned = false;
            self.version = version;
            self.interrupted = false;
            self.projection.commit(&record, change);
            self.records.push(record.clone());
            return Ok(record);
        }
    }
}
