use crate::ConversationStore;
use crate::Event;
use crate::EventId;
use crate::FileStore;
use crate::Kind;
use crate::Projection;
use crate::Record;
use crate::SliceRef;
use crate::codec;
use crate::projection::Change;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::Arc;

const INTERRUPTED: &str = " // nori-interrupted-record";

/// How much of the journal one read brings in while records are read in order.
const READ_AHEAD: i64 = 4 << 20;

/// How many bytes an open must have indexed itself before it saves the index for the next one.
const SAVE_INDEX_AFTER: i64 = 1 << 20;

/// Where a record's line is in the journal and what it is, read without decoding its payload.
struct Slot {
    offset: i64,
    /// The line's length without its line end.
    len: i64,
    kind: Kind,
    /// The operation's or command's name.
    name: Option<Arc<str>>,
    command_id: Option<Arc<str>>,
    /// The record itself, for those the log holds: what it appended, and what projecting the
    /// branch needed (a rewind, a fork, a line in another shape).
    held: Option<Arc<Record>>,
}

/// An append-only conversation journal, opened in one pass over its bytes: every record's place,
/// kind and name are indexed and the visible branch projected, but a record is decoded only
/// when it is read. A long conversation opens in the time it takes to find its line ends, and a
/// reader that wants its last turns, or every command of one name, decodes only those.
pub struct ConversationLog {
    store: Box<dyn ConversationStore>,
    /// Every record, hidden branches included, at `id - 1`.
    slots: Vec<Slot>,
    /// The record each command id was recorded with.
    command_ids: HashMap<Arc<str>, EventId>,
    /// The names of operations and commands, each held once.
    names: HashSet<Arc<str>>,
    projection: Projection,
    version: i64,
    /// Bytes of complete records: the version without a record a crash cut short.
    complete: i64,
    interrupted: bool,
    poisoned: bool,
}

/// What reading a journal from some offset found, kept apart until all of it reads.
struct Indexed {
    slots: Vec<Slot>,
    command_ids: Vec<(Arc<str>, EventId)>,
    projection: Projection,
    complete: i64,
    interrupted: bool,
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
            slots: Vec::new(),
            command_ids: HashMap::new(),
            names: HashSet::new(),
            projection: Projection::default(),
            version: 0,
            complete: 0,
            interrupted: false,
            poisoned: false,
        };
        log.restore_index();
        let restored = log.complete;
        log.refresh()?;
        if log.complete - restored >= SAVE_INDEX_AFTER {
            log.save_index();
        }
        Ok(log)
    }

    /// Take the index saved beside the journal, when it still describes the journal's first
    /// bytes: the open then reads only what was appended after them. Records projecting needs
    /// (rewinds, forks, lines only the full decoder reads) are read again from the journal.
    fn restore_index(&mut self) {
        use crate::saved_index::CHECK_BYTES;
        let Some(saved) = self
            .store
            .load_index()
            .and_then(|bytes| crate::saved_index::SavedIndex::decode(&bytes))
        else {
            return;
        };
        let check_start = (saved.covered - CHECK_BYTES).max(0);
        let still_there = self
            .store
            .read_range(check_start, saved.covered - check_start)
            .is_ok_and(|bytes| bytes == saved.check);
        if !still_there {
            return;
        }
        let names: Vec<Arc<str>> = saved.names.iter().map(|name| self.intern(name)).collect();
        let mut slots = Vec::with_capacity(saved.slots.len());
        let mut command_ids = HashMap::new();
        let mut projection = Projection::default();
        for (index, saved_slot) in saved.slots.into_iter().enumerate() {
            let Ok(id) = EventId::try_from(index + 1) else {
                return;
            };
            let mut slot = Slot {
                offset: saved_slot.offset,
                len: saved_slot.len,
                kind: saved_slot.kind,
                name: saved_slot
                    .name
                    .and_then(|name| usize::try_from(name).ok())
                    .and_then(|name| names.get(name).cloned()),
                command_id: saved_slot.command_id.map(Arc::from),
                held: None,
            };
            let change = if matches!(slot.kind, Kind::Rewind | Kind::Fork) {
                let Ok(record) = self
                    .store
                    .read_range(slot.offset, slot.len)
                    .map_err(drop)
                    .and_then(|bytes| String::from_utf8(bytes).map_err(drop))
                    .and_then(|line| codec::decode(&line).map_err(drop))
                else {
                    return;
                };
                let Ok(change) = projection.prepare(&record.event) else {
                    return;
                };
                slot.held = Some(Arc::new(record));
                change
            } else {
                Change::Push
            };
            if let Some(command_id) = &slot.command_id {
                command_ids.insert(Arc::clone(command_id), id);
            }
            projection.commit(id, change);
            slots.push(slot);
        }
        self.slots = slots;
        self.command_ids = command_ids;
        self.projection = projection;
        self.complete = saved.covered;
        self.version = saved.covered;
    }

    /// Save the index of every complete record beside the journal.
    fn save_index(&self) {
        use crate::saved_index::CHECK_BYTES;
        let check_start = (self.complete - CHECK_BYTES).max(0);
        let Ok(check) = self
            .store
            .read_range(check_start, self.complete - check_start)
        else {
            return;
        };
        let names: Vec<Arc<str>> = self.names.iter().cloned().collect();
        let name_index: HashMap<&str, i32> = names
            .iter()
            .enumerate()
            .filter_map(|(index, name)| Some((name.as_ref(), i32::try_from(index).ok()?)))
            .collect();
        let index = crate::saved_index::SavedIndex {
            covered: self.complete,
            check,
            names: names.iter().map(ToString::to_string).collect(),
            slots: self
                .slots
                .iter()
                .map(|slot| crate::saved_index::SavedSlot {
                    offset: slot.offset,
                    len: slot.len,
                    kind: slot.kind,
                    name: slot
                        .name
                        .as_deref()
                        .and_then(|name| name_index.get(name).copied()),
                    command_id: slot.command_id.as_deref().map(str::to_owned),
                })
                .collect(),
        };
        self.store.save_index(&index.encode());
    }

    /// Read what other writers appended since this log last read, without evaluating recorded
    /// operations. The journal is append-only, so only the bytes after the last complete record
    /// are read.
    pub fn refresh(&mut self) -> io::Result<()> {
        let data = self.store.read_from(self.complete)?;
        let indexed = self.index(&data)?;
        self.version = self
            .complete
            .checked_add(i64::try_from(data.len()).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("journal version exhausted"))?;
        self.slots.extend(indexed.slots);
        self.command_ids.extend(indexed.command_ids);
        self.projection = indexed.projection;
        self.complete = indexed.complete;
        self.interrupted = indexed.interrupted;
        self.poisoned = false;
        Ok(())
    }

    /// Index the lines of `data`, the journal from the end of the last complete record on.
    fn index(&mut self, data: &[u8]) -> io::Result<Indexed> {
        let mut indexed = Indexed {
            slots: Vec::new(),
            command_ids: Vec::new(),
            projection: self.projection.clone(),
            complete: self.complete,
            interrupted: false,
        };
        let mut seen: HashSet<Arc<str>> = HashSet::new();
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', data) {
            let offset = indexed.complete;
            indexed.complete += i64::try_from(end + 1 - start).map_err(io::Error::other)?;
            let line = std::str::from_utf8(&data[start..end])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            start = end + 1;
            if line.ends_with(INTERRUPTED) {
                continue;
            }
            let expected = self
                .slots
                .len()
                .checked_add(indexed.slots.len())
                .and_then(|count| EventId::try_from(count).ok())
                .and_then(|count| count.checked_add(1));
            let slot = self.slot(line, offset)?;
            let (id, event) = match &slot.held {
                Some(record) => (record.id, Some(&record.event)),
                None => (codec::locate(line).map_or(0, |located| located.id), None),
            };
            if expected != Some(id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "nonconsecutive event id",
                ));
            }
            if let Some(command_id) = &slot.command_id {
                if self.command_ids.contains_key(command_id) || !seen.insert(command_id.clone()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "duplicate command id",
                    ));
                }
                indexed.command_ids.push((command_id.clone(), id));
            }
            let change = match event {
                Some(event) => indexed.projection.prepare(event)?,
                None => Change::Push,
            };
            indexed.projection.commit(id, change);
            indexed.slots.push(slot);
        }
        indexed.interrupted = start < data.len();
        Ok(indexed)
    }

    /// The slot of `line`, which starts at `offset`. A line read only for its place unless
    /// projecting needs the record (a rewind or fork) or it is in a shape only the full decoder
    /// reads; those are decoded now, so a malformed one fails the open.
    fn slot(&mut self, line: &str, offset: i64) -> io::Result<Slot> {
        let len = i64::try_from(line.len()).map_err(io::Error::other)?;
        if let Some(located) = codec::locate(line)
            && !matches!(located.kind, Kind::Rewind | Kind::Fork)
        {
            return Ok(Slot {
                offset,
                len,
                kind: located.kind,
                name: located.name.map(|name| self.intern(name)),
                command_id: located.command_id.map(Arc::from),
                held: None,
            });
        }
        let record = codec::decode(line)?;
        Ok(Slot {
            offset,
            len,
            kind: record.event.kind(),
            name: record.event.name().map(|name| self.intern(name)),
            command_id: record.command_id.as_deref().map(Arc::from),
            held: Some(Arc::new(record)),
        })
    }

    fn intern(&mut self, name: &str) -> Arc<str> {
        if let Some(name) = self.names.get(name) {
            return Arc::clone(name);
        }
        let name: Arc<str> = Arc::from(name);
        self.names.insert(Arc::clone(&name));
        name
    }

    /// The byte length of the complete records read, which is where [`Self::scan_from`] goes on
    /// reading what was appended since.
    pub fn complete_len(&self) -> i64 {
        self.complete
    }

    /// Read the records of the log at `path` in order, without loading the whole log, until `f`
    /// breaks. Records are not checked against each other and nothing is projected, so this is
    /// for reading a few facts (metadata, a first message, a count) out of a long log quickly.
    /// A record cut short by a crash ends the scan.
    pub fn scan(
        path: impl AsRef<Path>,
        f: impl FnMut(Record) -> std::ops::ControlFlow<()>,
    ) -> io::Result<()> {
        Self::scan_from(path, 0, f).map(drop)
    }

    /// As [`Self::scan`], from byte `offset` of the log, which is where an earlier scan stopped.
    /// A log is append-only, so a reader that keeps up with it reads each record once. Returns the
    /// offset just past the last complete record read, where the next scan starts; a record still
    /// being written (no line end yet) is left for that scan. An offset that is not the start of a
    /// record fails to decode.
    pub fn scan_from(
        path: impl AsRef<Path>,
        offset: i64,
        mut f: impl FnMut(Record) -> std::ops::ControlFlow<()>,
    ) -> io::Result<i64> {
        use std::io::BufRead;
        use std::io::Seek;
        let mut file = std::fs::File::open(path)?;
        file.seek(io::SeekFrom::Start(
            u64::try_from(offset).map_err(io::Error::other)?,
        ))?;
        let mut reader = std::io::BufReader::new(file);
        let mut consumed = offset;
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 || !line.ends_with('\n') {
                return Ok(consumed);
            }
            consumed = consumed
                .checked_add(i64::try_from(read).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("log offset exhausted"))?;
            let text = line.trim_end_matches('\n');
            if text.ends_with(INTERRUPTED) {
                continue;
            }
            if f(codec::decode(text)?).is_break() {
                return Ok(consumed);
            }
        }
    }

    /// How many records the log holds, hidden branches included.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    /// The newest visible record's id.
    pub fn head(&self) -> Option<EventId> {
        self.projection.head()
    }

    /// The ids of the visible branch, oldest first.
    pub fn visible_ids(&self) -> &[EventId] {
        self.projection.visible()
    }

    fn slot_of(&self, id: EventId) -> Option<&Slot> {
        usize::try_from(id.checked_sub(1)?)
            .ok()
            .and_then(|index| self.slots.get(index))
    }

    /// The length of the record `id`'s line in the journal, known without reading it.
    pub fn line_len(&self, id: EventId) -> Option<i64> {
        self.slot_of(id).map(|slot| slot.len)
    }

    /// The kind of the record `id`, known without reading it.
    pub fn kind(&self, id: EventId) -> Option<Kind> {
        self.slot_of(id).map(|slot| slot.kind)
    }

    /// The operation or command name of the record `id`, known without reading it.
    pub fn name(&self, id: EventId) -> Option<&str> {
        self.slot_of(id).and_then(|slot| slot.name.as_deref())
    }

    /// The ids of the visible records `pick` chooses by kind and name, oldest first, found
    /// without reading a record.
    pub fn visible_where(&self, pick: impl Fn(Kind, Option<&str>) -> bool) -> Vec<EventId> {
        self.visible_ids()
            .iter()
            .copied()
            .filter(|id| {
                self.slot_of(*id)
                    .is_some_and(|slot| pick(slot.kind, slot.name.as_deref()))
            })
            .collect()
    }

    /// The visible operations or commands named `name`, oldest first, found without reading a
    /// record.
    pub fn visible_named(&self, kind: Kind, name: &str) -> Vec<EventId> {
        self.visible_where(|found, named| found == kind && named == Some(name))
    }

    /// Read the record `id`.
    pub fn record(&self, id: EventId) -> io::Result<Arc<Record>> {
        self.records([id])
            .next()
            .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::NotFound, "no such record")))
    }

    /// Read the records `ids` in the order given. Records next to each other in the journal are
    /// read together, so reading a branch in order (or in reverse) costs a few large reads.
    pub fn records(&self, ids: impl IntoIterator<Item = EventId>) -> Records<'_> {
        Records {
            log: self,
            ids: ids.into_iter().collect::<Vec<_>>().into_iter(),
            window: Window::default(),
        }
    }

    /// The visible records whose line contains `needle`, oldest first: each line is searched as
    /// written, and none is decoded. An id or a name is written into the line literally, so this
    /// finds the few records about one thing in a long conversation for the cost of reading it.
    pub fn visible_containing(&self, needle: &str) -> io::Result<Vec<EventId>> {
        let mut records = self.records(self.visible_ids().to_vec());
        let mut found = Vec::new();
        while let Some(id) = records.ids.next() {
            if records.line(id)?.is_some_and(|line| line.contains(needle)) {
                found.push(id);
            }
        }
        Ok(found)
    }

    /// Read the visible branch, oldest first.
    pub fn visible(&self) -> Records<'_> {
        self.records(self.visible_ids().to_vec())
    }

    /// Read every record, hidden branches included, oldest first.
    pub fn all(&self) -> Records<'_> {
        let len = EventId::try_from(self.slots.len()).unwrap_or(EventId::MAX);
        self.records(1..=len)
    }

    /// Resolve inclusive stable identifiers against the current branch.
    pub fn slice(&self, reference: SliceRef) -> io::Result<Option<Vec<Record>>> {
        let Some(ids) = self.projection.slice(reference) else {
            return Ok(None);
        };
        self.records(ids.to_vec())
            .map(|record| record.map(Arc::unwrap_or_clone))
            .collect::<io::Result<Vec<_>>>()
            .map(Some)
    }

    /// Resolve a slice at an earlier event, including a branch hidden by rewind.
    pub fn slice_at(&self, at: EventId, reference: SliceRef) -> io::Result<Option<Vec<Record>>> {
        let mut projection = Projection::default();
        for (index, slot) in self.slots.iter().enumerate() {
            let id = EventId::try_from(index + 1).map_err(io::Error::other)?;
            let change = match &slot.held {
                Some(record) => projection.prepare(&record.event)?,
                None => Change::Push,
            };
            projection.commit(id, change);
            if id == at {
                let Some(ids) = projection.slice(reference) else {
                    return Ok(None);
                };
                return self
                    .records(ids.to_vec())
                    .map(|record| record.map(Arc::unwrap_or_clone))
                    .collect::<io::Result<Vec<_>>>()
                    .map(Some);
            }
        }
        Ok(None)
    }

    /// Flush everything appended so far to disk now; appends otherwise reach it within a
    /// moment on their own. Call it where a barrier matters: a clean shutdown, a flush request.
    pub fn sync(&mut self) -> io::Result<()> {
        self.store.sync()
    }

    pub fn next_id(&self) -> io::Result<EventId> {
        EventId::try_from(self.slots.len())
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| io::Error::other("event id exhausted"))
    }

    /// Append at this instance's current head. On conflict, refresh and retry.
    pub fn append(&mut self, event: Event) -> io::Result<Arc<Record>> {
        self.append_inner(self.projection.head(), None, event)
    }

    /// Append against an expected visible head, deduplicating a stable command ID.
    /// Retrying the same ID and event returns its original record even after rewind.
    pub fn append_with_id(
        &mut self,
        expected_head: Option<EventId>,
        command_id: &str,
        event: Event,
    ) -> io::Result<Arc<Record>> {
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
        mut event: Event,
    ) -> io::Result<Arc<Record>> {
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
                && let Some(recorded) = self.command_ids.get(id.as_str()).copied()
            {
                let record = self.record(recorded)?;
                if record.event == event {
                    return Ok(record);
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
            // The event moves into the record, and comes back out of it for a retry: a record
            // can be megabytes, and appending it never copies it.
            let record = Arc::new(Record {
                id: self.next_id()?,
                command_id: command_id.clone(),
                event,
            });
            let change = self.projection.prepare(&record.event)?;
            let mut bytes = Vec::new();
            if self.interrupted {
                bytes.extend_from_slice(format!("{INTERRUPTED}\n").as_bytes());
            }
            let line_start = self
                .version
                .checked_add(i64::try_from(bytes.len()).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("journal version exhausted"))?;
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
                        event = Arc::unwrap_or_clone(record).event;
                        continue;
                    }
                }
                return Err(error);
            }
            self.poisoned = false;
            self.version = version;
            self.complete = version;
            self.interrupted = false;
            self.projection.commit(record.id, change);
            if let Some(id) = &record.command_id {
                self.command_ids.insert(Arc::from(id.as_str()), record.id);
            }
            let name = record.event.name().map(|name| self.intern(name));
            self.slots.push(Slot {
                offset: line_start,
                len: version - line_start - 1,
                kind: record.event.kind(),
                name,
                command_id: record.command_id.as_deref().map(Arc::from),
                held: Some(Arc::clone(&record)),
            });
            return Ok(record);
        }
    }
}

/// Journal bytes read ahead: `bytes` start at `offset`.
#[derive(Default)]
struct Window {
    offset: i64,
    bytes: Vec<u8>,
}

/// Records read from a log in a given order; see [`ConversationLog::records`].
pub struct Records<'a> {
    log: &'a ConversationLog,
    ids: std::vec::IntoIter<EventId>,
    window: Window,
}

impl Records<'_> {
    fn read(&mut self, id: EventId) -> io::Result<Arc<Record>> {
        if let Some(record) = self.log.slot_of(id).and_then(|slot| slot.held.as_ref()) {
            return Ok(Arc::clone(record));
        }
        let line = self
            .line(id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such record"))?;
        let record = codec::decode(line)?;
        if record.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "record is not where the journal index put it",
            ));
        }
        Ok(Arc::new(record))
    }

    /// The line of the record `id` as the journal has it; `None` for no such record.
    fn line(&mut self, id: EventId) -> io::Result<Option<&str>> {
        let Some(slot) = self.log.slot_of(id) else {
            return Ok(None);
        };
        let end = slot.offset + slot.len;
        let window_end = self.window.offset
            + i64::try_from(self.window.bytes.len()).map_err(io::Error::other)?;
        if slot.offset < self.window.offset || end > window_end {
            // Bring in this line and the lines around it the next reads want, whichever way the
            // reading goes: a run of neighbours costs one read, a lone record only its own line.
            let next = self
                .ids
                .as_slice()
                .first()
                .and_then(|id| self.log.slot_of(*id));
            let (start, stop) = match next {
                Some(next) if next.offset > slot.offset && next.offset - end < READ_AHEAD => (
                    slot.offset,
                    (slot.offset + READ_AHEAD).min(self.log.complete).max(end),
                ),
                Some(next)
                    if next.offset < slot.offset && slot.offset - next.offset < READ_AHEAD =>
                {
                    ((end - READ_AHEAD).max(0).min(slot.offset), end)
                }
                Some(_) | None => (slot.offset, end),
            };
            self.window = Window {
                offset: start,
                bytes: self.log.store.read_range(start, stop - start)?,
            };
        }
        let from = usize::try_from(slot.offset - self.window.offset).map_err(io::Error::other)?;
        let to = usize::try_from(end - self.window.offset).map_err(io::Error::other)?;
        std::str::from_utf8(&self.window.bytes[from..to])
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

impl Iterator for Records<'_> {
    type Item = io::Result<Arc<Record>>;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.ids.next()?;
        Some(self.read(id))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ids.size_hint()
    }
}

impl ExactSizeIterator for Records<'_> {}
