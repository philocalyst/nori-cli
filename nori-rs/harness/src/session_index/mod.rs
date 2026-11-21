//! The on-disk full-text index of every recorded session, at `$NORI_HOME/index/v1/`.
//!
//! A raw transcript is mostly protocol traffic; the index keeps what a person remembers: what
//! was asked, what was answered, which tools ran with which commands and files. Per session it
//! stores a digest (merged turns and a header; see [`digest`]) and a trigram bloom filter. A search loads
//! the small filters of all sessions, prunes with them, and reads only the digests that might
//! match, so the cost follows the candidates, not the size of the history. The same digest
//! feeds the resume picker's preview, so opening a session's preview never re-parses a raw
//! transcript of tens of megabytes.
//!
//! Nothing here blocks the UI: every entry point is either `async` or meant for
//! `spawn_blocking`. Updates are incremental: a session's records only grow at the end, and a
//! cursor beside each digest says how far they were read (see [`cursor`]), so bringing a digest
//! up to date reads only what was appended since, and writes only what that added: turns that
//! became final are appended to the digest, the bloom filter is patched where new trigrams set
//! bits, and the small cursor and header are replaced. The session a person is in is current
//! after reading what its last turns added, however long it is, and an update on session end
//! ([`SessionIndex::sync_after_session`]) is a few appended records. Digests whose transcript was
//! deleted are removed by [`SessionIndex::sync_all`].

mod bloom;
mod cursor;
mod digest;
mod extract;
mod lineage;
mod listing;
mod search;

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use futures::StreamExt;

pub use bloom::Bloom;
pub use digest::DIGEST_SCHEMA;
pub use digest::DigestHeader;
pub use digest::DigestTurn;
pub use digest::SessionDigest;
pub use digest::SourceStamp;
pub use digest::TurnRole;
pub use lineage::Lineage;
pub use listing::ListedSession;
pub use listing::SessionListing;
pub use search::MIN_TERM_CHARS;
pub use search::SessionHit;
pub use search::SessionSearch;

use crate::transcript::SessionMetadata;
use crate::transcript::TranscriptLoader;

const INDEX_DIR: &str = "index";
const INDEX_VERSION_DIR: &str = "v1";
/// Transcripts digested at once while syncing.
const SYNC_CONCURRENCY: usize = 4;

pub(crate) struct CatalogEntry {
    pub(crate) header: DigestHeader,
    pub(crate) bloom: Bloom,
}

/// The headers and filters of every indexed session, loaded once and kept current.
#[derive(Default)]
pub(crate) struct Catalog {
    entries: HashMap<String, Arc<CatalogEntry>>,
}

impl Catalog {
    pub(crate) fn entries(&self) -> impl Iterator<Item = &Arc<CatalogEntry>> {
        self.entries.values()
    }
}

/// Progress of [`SessionIndex::sync_all`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncProgress {
    pub done: i64,
    pub total: i64,
    /// Sessions whose digest had to be (re)built so far.
    pub rebuilt: i64,
}

/// Handle on the index of one `NORI_HOME`. Cheap to clone; clones share the catalog.
#[derive(Clone)]
pub struct SessionIndex {
    inner: Arc<Inner>,
}

struct Inner {
    nori_home: PathBuf,
    root: PathBuf,
    catalog: RwLock<Option<Catalog>>,
    /// One gate per session being digested: a second request for a session waits for the first
    /// and then finds it current, instead of reading the same transcript twice at once.
    gates: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionIndex {
    /// The index of `nori_home`. Handles opened while another is alive share its catalog and its
    /// per-session gates, so a picker opening while an earlier one is still indexing neither
    /// reads every header again nor digests the same transcript twice.
    pub fn open(nori_home: &Path) -> Self {
        static OPEN: std::sync::LazyLock<
            std::sync::Mutex<HashMap<PathBuf, std::sync::Weak<Inner>>>,
        > = std::sync::LazyLock::new(std::sync::Mutex::default);
        let mut open = OPEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(inner) = open.get(nori_home).and_then(std::sync::Weak::upgrade) {
            return Self { inner };
        }
        open.retain(|_, inner| inner.strong_count() > 0);
        let inner = Arc::new(Inner {
            nori_home: nori_home.to_path_buf(),
            root: nori_home.join(INDEX_DIR).join(INDEX_VERSION_DIR),
            catalog: RwLock::new(None),
            gates: std::sync::Mutex::default(),
        });
        open.insert(nori_home.to_path_buf(), Arc::downgrade(&inner));
        Self { inner }
    }

    fn transcripts(&self) -> TranscriptLoader {
        TranscriptLoader::new(self.inner.nori_home.clone())
    }

    /// Load the catalog from disk if it is not loaded yet. Blocking: use `spawn_blocking`.
    fn ensure_catalog(&self) {
        let mut guard = self
            .inner
            .catalog
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_some() {
            return;
        }
        let mut catalog = Catalog::default();
        let Ok(projects) = std::fs::read_dir(&self.inner.root) else {
            *guard = Some(catalog);
            return;
        };
        for project in projects.flatten() {
            let Ok(files) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().is_none_or(|extension| extension != "nhd") {
                    continue;
                }
                let loaded = digest::read_header(&path)
                    .ok()
                    .filter(|header| header.schema == DIGEST_SCHEMA)
                    .and_then(|header| {
                        let bytes = std::fs::read(path.with_extension("tri")).ok()?;
                        let bloom = Bloom::decode(&bytes)?;
                        Some(CatalogEntry { header, bloom })
                    });
                if let Some(entry) = loaded {
                    catalog
                        .entries
                        .insert(entry.header.session_id.clone(), Arc::new(entry));
                }
            }
        }
        *guard = Some(catalog);
    }

    fn catalog_entry(&self, session_id: &str) -> Option<Arc<CatalogEntry>> {
        self.ensure_catalog();
        self.inner
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()?
            .entries
            .get(session_id)
            .cloned()
    }

    /// The digest headers of every indexed session: titles, branches, models and turn counts,
    /// without touching a transcript. Blocking.
    pub fn headers(&self) -> Vec<DigestHeader> {
        self.ensure_catalog();
        let guard = self
            .inner
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .map(|catalog| {
                catalog
                    .entries()
                    .map(|entry| entry.header.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Full-text search. Blocking.
    pub fn search(&self, search: &SessionSearch) -> Vec<SessionHit> {
        self.ensure_catalog();
        let guard = self
            .inner
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .map(|catalog| search::run(catalog, &self.inner.root, search))
            .unwrap_or_default()
    }

    /// The digest of one session, if it is indexed. Blocking.
    pub fn digest(&self, project_id: &str, session_id: &str) -> Option<SessionDigest> {
        digest::read_digest(&self.inner.root, project_id, session_id).ok()
    }

    /// Bring one session's digest up to date with its records, reading only what was appended
    /// since the last digest. Returns whether the digest changed.
    pub async fn sync_session(&self, project_id: &str, session_id: &str) -> io::Result<bool> {
        let gate = Arc::clone(
            self.inner
                .gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(session_id.to_owned())
                .or_default(),
        );
        let _one_at_a_time = gate.lock().await;
        let index = self.clone();
        let (project, session) = (project_id.to_owned(), session_id.to_owned());
        tokio::task::spawn_blocking(move || index.sync_blocking(&project, &session))
            .await
            .map_err(io::Error::other)?
    }

    /// [`Self::sync_session`] on the calling thread, which may block.
    fn sync_blocking(&self, project_id: &str, session_id: &str) -> io::Result<bool> {
        let transcript = self.transcripts().session_path(project_id, session_id);
        let (source, file) = cursor::Source::of(&transcript);
        let metadata = std::fs::metadata(&file).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => {
                io::Error::new(io::ErrorKind::NotFound, "transcript is gone")
            }
            _ => error,
        })?;
        let len = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        let root = &self.inner.root;
        let cursor_path = digest::cursor_path(root, project_id, session_id);
        let turns_path = digest::digest_path(root, project_id, session_id);
        let entry = self
            .catalog_entry(session_id)
            .filter(|entry| entry.header.project_id == project_id);
        // The digest's stamp is where its cursor stopped: a digest read to the file's end is
        // current without reading the cursor.
        if entry
            .as_ref()
            .is_some_and(|entry| entry.header.source.len == len)
            && cursor_path.exists()
        {
            return Ok(false);
        }
        // A cursor continues a digest only when the digest it belongs to is whole: its header
        // and filter loaded, and its turns file holds what the cursor says.
        let turns_len = std::fs::metadata(&turns_path).map_or(0, |metadata| {
            i64::try_from(metadata.len()).unwrap_or(i64::MAX)
        });
        let saved = entry
            .as_ref()
            .and_then(|_| cursor::Cursor::load(&cursor_path, source, len))
            .filter(|cursor| cursor.turns_bytes <= turns_len);
        let mut cursor = saved.unwrap_or_else(|| cursor::Cursor::start(source));
        let before = cursor.read;
        cursor.advance(&file)?;
        if !cursor.restarted
            && cursor.read == before
            && entry
                .as_ref()
                .is_some_and(|entry| entry.header.source.len == before)
        {
            // Nothing complete was appended: a record is still being written.
            return Ok(false);
        }
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0));
        let stamp = SourceStamp {
            len: cursor.read,
            mtime_ms,
        };
        // A session whose metadata is not written yet has nothing to show.
        let Some(header) = cursor.extractor.header(project_id, stamp) else {
            return Ok(false);
        };
        let ready = cursor.extractor.take_ready();
        let tail = cursor.extractor.tail_turns();
        let previous = if cursor.restarted {
            None
        } else {
            entry.map(|entry| entry.bloom.clone())
        };

        // 1. The turns that became final are appended after the ones already there.
        let keep = if cursor.restarted {
            0
        } else {
            cursor.turns_bytes
        };
        cursor.turns_bytes = digest::append_turns(&turns_path, keep, &ready)?;
        // 2. The filter learns the new text.
        let new_texts = || {
            header
                .branch
                .iter()
                .map(String::as_str)
                .chain(ready.iter().chain(&tail).flat_map(DigestTurn::texts))
        };
        let bloom_path = digest::bloom_path(root, project_id, session_id);
        let bloom = match previous {
            Some(mut bloom) => {
                let mut patch = bloom::Patch::new();
                for text in new_texts() {
                    bloom.add(text, &mut patch);
                }
                if bloom.crowded() {
                    // Too full to answer well: build it again, larger, from all of the text.
                    let mut all = digest::read_turns(&turns_path, cursor.turns_bytes)?;
                    all.extend(tail.iter().cloned());
                    let bloom = Bloom::build(
                        header
                            .branch
                            .iter()
                            .map(String::as_str)
                            .chain(all.iter().flat_map(DigestTurn::texts)),
                    );
                    digest::write_atomically(&bloom_path, &bloom.encode())?;
                    bloom
                } else {
                    if bloom.patch_file(&bloom_path, &patch).is_err() {
                        digest::write_atomically(&bloom_path, &bloom.encode())?;
                    }
                    bloom
                }
            }
            None => {
                let bloom = Bloom::build(new_texts());
                digest::write_atomically(&bloom_path, &bloom.encode())?;
                bloom
            }
        };
        // 3. The cursor, then the header, which is what makes the update visible.
        cursor.save(&cursor_path)?;
        digest::write_atomically(
            &digest::header_path(root, project_id, session_id),
            &serde_json::to_vec(&header).map_err(io::Error::other)?,
        )?;
        self.ensure_catalog();
        if let Some(catalog) = self
            .inner
            .catalog
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            catalog.entries.insert(
                header.session_id.clone(),
                Arc::new(CatalogEntry { header, bloom }),
            );
        }
        Ok(true)
    }

    /// Bring every session up to date and drop digests of deleted transcripts: the sessions
    /// named in `first` (by id) in that order, then the rest, newest first. `progress` is
    /// called after each session. A session that cannot be read is skipped and reported through
    /// `tracing`; it never stops the rest.
    pub async fn sync_all(
        &self,
        first: &[String],
        mut progress: impl FnMut(SyncProgress) + Send,
    ) -> io::Result<()> {
        // Each session's first record and nothing more: enumerating them reads the same however
        // long the sessions grew.
        let mut sessions: Vec<SessionMetadata> =
            self.transcripts().list_session_heads(None).await?;
        // A stable sort on the position in `first` keeps newest-first within each group.
        sessions.sort_by_key(|session| {
            first
                .iter()
                .position(|id| *id == session.session_id)
                .unwrap_or(first.len())
        });
        let total = i64::try_from(sessions.len()).unwrap_or(i64::MAX);
        let mut state = SyncProgress {
            done: 0,
            total,
            rebuilt: 0,
        };
        progress(state);
        // Several transcripts at once: each digest is read, parsed and written off the async
        // threads, and `buffered` keeps the priority order of `sessions`.
        let mut results = std::pin::pin!(
            futures::stream::iter(sessions.iter().cloned())
                .map(|session| async move {
                    let result = self
                        .sync_session(&session.project_id, &session.session_id)
                        .await;
                    (session, result)
                })
                .buffered(SYNC_CONCURRENCY)
        );
        while let Some((session, result)) = results.next().await {
            match result {
                Ok(true) => state.rebuilt += 1,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    target: "nori_index",
                    session_id = %session.session_id,
                    error = %error,
                    "could not index session transcript",
                ),
            }
            state.done += 1;
            progress(state);
        }
        let live: std::collections::HashSet<&str> = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        let index = self.clone();
        let indexed = tokio::task::spawn_blocking(move || index.headers())
            .await
            .map_err(io::Error::other)?;
        let stale: Vec<DigestHeader> = indexed
            .into_iter()
            .filter(|header| !live.contains(header.session_id.as_str()))
            .collect();
        for header in stale {
            self.forget(&header.project_id, &header.session_id);
        }
        Ok(())
    }

    fn forget(&self, project_id: &str, session_id: &str) {
        let _ = std::fs::remove_file(digest::header_path(
            &self.inner.root,
            project_id,
            session_id,
        ));
        let _ = std::fs::remove_file(digest::digest_path(
            &self.inner.root,
            project_id,
            session_id,
        ));
        let _ = std::fs::remove_file(digest::bloom_path(&self.inner.root, project_id, session_id));
        let _ = std::fs::remove_file(digest::cursor_path(
            &self.inner.root,
            project_id,
            session_id,
        ));
        if let Some(catalog) = self
            .inner
            .catalog
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            catalog.entries.remove(session_id);
        }
    }

    /// Update the index for the session whose transcript is at `transcript_path`, after the
    /// session ended, and wait for it for at most `budget`: a process that is about to exit
    /// has no runtime left to finish it in the background, and the next picker should find the
    /// session titled. Does nothing when the path is not under a `$NORI_HOME` layout. Failures
    /// are logged, never raised.
    pub async fn sync_after_session(transcript_path: &Path, budget: std::time::Duration) {
        // <home>/transcripts/by-project/<project>/sessions/<session>.jsonl
        let mut ancestors = transcript_path.ancestors();
        let Some(session_id) = transcript_path.file_stem().and_then(|stem| stem.to_str()) else {
            return;
        };
        let project_id = ancestors
            .nth(2)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str());
        let nori_home = ancestors.nth(2);
        let (Some(project_id), Some(nori_home)) = (project_id, nori_home) else {
            return;
        };
        let index = Self::open(nori_home);
        match tokio::time::timeout(budget, index.sync_session(project_id, session_id)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(target: "nori_index", %session_id, %error, "could not index ended session");
            }
            Err(_) => {
                tracing::warn!(target: "nori_index", %session_id, "indexing the ended session took too long; the picker will finish it");
            }
        }
    }
}

#[cfg(test)]
mod tests;
