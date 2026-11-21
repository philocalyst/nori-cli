//! Reading a session for its digest from where the last digest stopped.
//!
//! A session's records only ever grow at the end: the journal (`.pkl`) and the JSONL transcript
//! are append-only. Beside each digest the index keeps a cursor (`<session>.nst`): which file the
//! digest was read from, how many bytes of it, and the [`Extractor`] as it stood there. Bringing
//! a digest up to date reads the bytes appended since, never the session from its start, so the
//! session a person is in costs what it grew by, however long it is. Only a rewind or a fork,
//! which change which earlier records are visible, make the journal be read from the start again.
//!
//! The cursor also says how many bytes of the digest's turns file are valid: turns are appended
//! to it before the cursor is replaced, so bytes past that length belong to an update that did
//! not finish and are overwritten by the next one. Together with the extractor's unfinished tail
//! it is small, however long the session: nothing already final is kept in it.
//!
//! A cursor that cannot be read (absent, from another layout, or past the file's end because the
//! file was replaced) is not an error: the session is read from its start.

use std::io;
use std::io::BufRead;
use std::io::Seek;
use std::ops::ControlFlow;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::extract::Extractor;
use crate::transcript::TranscriptLine;

/// Bump when the cursor's layout or the extractor's state changes; older cursors are dropped and
/// their sessions read again from the start.
const CURSOR_SCHEMA: i32 = 2;

/// Which of a session's files its digest is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Source {
    /// The canonical journal, `<session>.pkl`.
    Journal,
    /// A session recorded before journals: its JSONL transcript.
    Lines,
}

impl Source {
    /// The file a session's digest is read from, and which kind it is.
    pub(super) fn of(transcript: &Path) -> (Self, PathBuf) {
        let journal = transcript.with_extension("pkl");
        if journal.exists() {
            (Self::Journal, journal)
        } else {
            (Self::Lines, transcript.to_path_buf())
        }
    }
}

/// How far a session has been read for its digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Cursor {
    schema: i32,
    pub(super) source: Source,
    /// Bytes of `source` read: always the end of a complete record.
    pub(super) read: i64,
    /// Bytes of the digest's turns file that hold the final turns read so far.
    pub(super) turns_bytes: i64,
    pub(super) extractor: Extractor,
    /// The digest has to be written again from its first turn: the session is read from its
    /// start (no usable cursor) or a rewind changed which of the records read count.
    #[serde(skip)]
    pub(super) restarted: bool,
}

impl Cursor {
    /// Nothing read yet.
    pub(super) fn start(source: Source) -> Self {
        Self {
            schema: CURSOR_SCHEMA,
            source,
            read: 0,
            turns_bytes: 0,
            extractor: Extractor::default(),
            restarted: true,
        }
    }

    /// The cursor saved at `path`, whatever file it stood at: for reading a digest, not for
    /// continuing one.
    pub(super) fn read_any(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice::<Self>(&bytes)
            .ok()
            .filter(|cursor| cursor.schema == CURSOR_SCHEMA)
    }

    /// The cursor saved at `path`, if it is one for `source` that `len` bytes can continue.
    pub(super) fn load(path: &Path, source: Source, len: i64) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice::<Self>(&bytes)
            .ok()
            .filter(|cursor| {
                cursor.schema == CURSOR_SCHEMA && cursor.source == source && cursor.read <= len
            })
    }

    pub(super) fn save(&self, path: &Path) -> io::Result<()> {
        super::digest::write_atomically(path, &serde_json::to_vec(self).map_err(io::Error::other)?)
    }

    /// Read what was appended to `file` since this cursor. Blocking.
    pub(super) fn advance(&mut self, file: &Path) -> io::Result<()> {
        match self.source {
            Source::Journal => {
                let mut rewritten = false;
                let extractor = &mut self.extractor;
                let read =
                    nori_conversation::ConversationLog::scan_from(file, self.read, |record| {
                        if extractor.feed_record(&record).is_err() {
                            rewritten = true;
                            return ControlFlow::Break(());
                        }
                        ControlFlow::Continue(())
                    })?;
                if rewritten {
                    self.rebuild_journal(file)
                } else {
                    self.read = read;
                    Ok(())
                }
            }
            Source::Lines => self.advance_lines(file),
        }
    }

    /// Read the whole journal as it is visible now: a rewind or a fork changed which of the
    /// records already read count.
    fn rebuild_journal(&mut self, file: &Path) -> io::Result<()> {
        let log = nori_conversation::ConversationLog::open(file)?;
        let mut extractor = Extractor::default();
        for record in log.visible() {
            // A fork record in the visible branch is only provenance here.
            let _ = extractor.feed_record(record?.as_ref());
        }
        self.extractor = extractor;
        self.read = log.complete_len();
        self.turns_bytes = 0;
        self.restarted = true;
        Ok(())
    }

    /// Read the JSON lines appended to a transcript without a journal. A line that does not parse
    /// (an entry kind from another version) is skipped, as the loader skips it.
    fn advance_lines(&mut self, file: &Path) -> io::Result<()> {
        let mut reader = std::fs::File::open(file)?;
        reader.seek(io::SeekFrom::Start(
            u64::try_from(self.read).map_err(io::Error::other)?,
        ))?;
        let mut reader = io::BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 || line.last() != Some(&b'\n') {
                return Ok(());
            }
            self.read = self
                .read
                .checked_add(i64::try_from(read).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("transcript offset exhausted"))?;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if let Ok(entry) = serde_json::from_slice::<TranscriptLine>(&line) {
                self.extractor.feed_line(&entry);
            }
        }
    }
}
