//! The per-session read ledger.
//!
//! Every successful `read` records what the model saw of a file: a hash of its content, its
//! modification time and the line ranges shown. `edit`, `multi_edit` and `write` consult the
//! ledger before touching an existing file: a file never read, or whose content differs from what
//! was read, is refused with an instruction to read it again. The tools' own writes update the
//! ledger, so a chain of edits needs only one read.
//!
//! The ledger is serializable ([`LedgerSnapshot`]), so a session that moves to another machine
//! keeps the knowledge of what the model has seen.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

/// An inclusive, 1-based range of lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: i64,
    pub end: i64,
}

/// What the model has seen of one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRecord {
    /// SHA-256 of the file's bytes when it was read or last written by a tool.
    pub content_hash: String,
    /// Modification time then, in milliseconds since the Unix epoch.
    pub modified_ms: Option<i64>,
    pub total_lines: i64,
    /// Disjoint, sorted ranges the model has been shown.
    pub ranges: Vec<LineRange>,
    /// True when the last change to the file was made by a tool call rather than seen by a read.
    pub written_by_tool: bool,
}

impl ReadRecord {
    /// Whether the model has seen every line.
    pub fn fully_read(&self) -> bool {
        matches!(
            self.ranges.as_slice(),
            [only] if only.start <= 1 && only.end >= self.total_lines
        ) || self.total_lines == 0
    }
}

/// The ledger's verdict on a file about to be changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Read (or written by a tool) and unchanged since.
    Fresh,
    NeverRead,
    ChangedOnDisk,
}

/// A serializable copy of a ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    pub entries: Vec<(PathBuf, ReadRecord)>,
}

#[derive(Debug, Default)]
pub struct ReadLedger {
    records: Mutex<HashMap<PathBuf, ReadRecord>>,
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn millis(time: Option<SystemTime>) -> Option<i64> {
    let since = time?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

impl ReadLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, ReadRecord>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record that the model was shown `range` of the file whose bytes hash to `content_hash`.
    /// A read of changed content starts a fresh record.
    pub fn record_read(
        &self,
        path: &Path,
        content_hash: &str,
        modified: Option<SystemTime>,
        total_lines: i64,
        range: Option<LineRange>,
    ) {
        let mut records = self.lock();
        let record = records
            .entry(path.to_path_buf())
            .or_insert_with(|| ReadRecord {
                content_hash: content_hash.to_owned(),
                modified_ms: millis(modified),
                total_lines,
                ranges: Vec::new(),
                written_by_tool: false,
            });
        if record.content_hash != content_hash {
            *record = ReadRecord {
                content_hash: content_hash.to_owned(),
                modified_ms: millis(modified),
                total_lines,
                ranges: Vec::new(),
                written_by_tool: false,
            };
        }
        record.modified_ms = millis(modified);
        record.total_lines = total_lines;
        if let Some(range) = range {
            add_range(&mut record.ranges, range);
        }
    }

    /// Record a write made by a tool: the model knows what it wrote, so the file counts as fully
    /// read at its new content.
    pub fn record_write(
        &self,
        path: &Path,
        content_hash: &str,
        modified: Option<SystemTime>,
        total_lines: i64,
    ) {
        let ranges = if total_lines > 0 {
            vec![LineRange {
                start: 1,
                end: total_lines,
            }]
        } else {
            Vec::new()
        };
        self.lock().insert(
            path.to_path_buf(),
            ReadRecord {
                content_hash: content_hash.to_owned(),
                modified_ms: millis(modified),
                total_lines,
                ranges,
                written_by_tool: true,
            },
        );
    }

    /// Compare the file's current content hash with what the model saw.
    pub fn check(&self, path: &Path, current_hash: &str) -> Freshness {
        match self.lock().get(path) {
            None => Freshness::NeverRead,
            Some(record) if record.content_hash == current_hash => Freshness::Fresh,
            Some(_) => Freshness::ChangedOnDisk,
        }
    }

    pub fn record(&self, path: &Path) -> Option<ReadRecord> {
        self.lock().get(path).cloned()
    }

    pub fn forget(&self, path: &Path) {
        self.lock().remove(path);
    }

    pub fn snapshot(&self) -> LedgerSnapshot {
        let mut entries: Vec<_> = self
            .lock()
            .iter()
            .map(|(path, record)| (path.clone(), record.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        LedgerSnapshot { entries }
    }

    pub fn restore(&self, snapshot: LedgerSnapshot) {
        let mut records = self.lock();
        records.clear();
        records.extend(snapshot.entries);
    }
}

fn add_range(ranges: &mut Vec<LineRange>, new: LineRange) {
    ranges.push(new);
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<LineRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        match merged.last_mut() {
            Some(last) if range.start <= last.end + 1 => last.end = last.end.max(range.end),
            Some(_) | None => merged.push(range),
        }
    }
    *ranges = merged;
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn ranges_merge_and_full_read_is_detected() {
        let ledger = ReadLedger::new();
        let path = Path::new("/a");
        let hash = hash_bytes(b"x");
        ledger.record_read(path, &hash, None, 10, Some(LineRange { start: 1, end: 4 }));
        assert_eq!(ledger.record(path).map(|r| r.fully_read()), Some(false));
        ledger.record_read(path, &hash, None, 10, Some(LineRange { start: 5, end: 10 }));
        let record = ledger.record(path).expect("record");
        assert_eq!(record.ranges, vec![LineRange { start: 1, end: 10 }]);
        assert!(record.fully_read());
    }

    #[test]
    fn check_distinguishes_never_read_fresh_and_changed() {
        let ledger = ReadLedger::new();
        let path = Path::new("/a");
        assert_eq!(ledger.check(path, "h1"), Freshness::NeverRead);
        ledger.record_read(path, "h1", None, 1, None);
        assert_eq!(ledger.check(path, "h1"), Freshness::Fresh);
        assert_eq!(ledger.check(path, "h2"), Freshness::ChangedOnDisk);
        ledger.record_write(path, "h2", None, 3);
        assert_eq!(ledger.check(path, "h2"), Freshness::Fresh);
    }

    #[test]
    fn snapshot_round_trips() {
        let ledger = ReadLedger::new();
        ledger.record_write(Path::new("/b"), "h", None, 2);
        let restored = ReadLedger::new();
        restored.restore(ledger.snapshot());
        assert_eq!(restored.snapshot(), ledger.snapshot());
    }
}
