//! The on-disk full-text index of every recorded session, at `$NORI_HOME/index/v1/`.
//!
//! A raw transcript is mostly protocol traffic; the index keeps what a person remembers: what
//! was asked, what was answered, which tools ran with which commands and files. Per session it
//! stores a digest (`.nidx`, merged turns) and a trigram bloom filter (`.tri`). A search loads
//! the small filters of all sessions, prunes with them, and reads only the digests that might
//! match, so the cost follows the candidates, not the size of the history. The same digest
//! feeds the resume picker's preview, so opening a session's preview never re-parses a raw
//! transcript of tens of megabytes.
//!
//! Nothing here blocks the UI: every entry point is either `async` or meant for
//! `spawn_blocking`. Updates are incremental: a session is re-digested only when its
//! transcript's length or modification time differs from the stamp in its digest, which makes
//! "update on open" cheap and "update on session end" ([`SessionIndex::sync_after_session`])
//! exact. Digests whose transcript was deleted are removed by [`SessionIndex::sync_all`].

mod bloom;
mod digest;
mod extract;
mod lineage;
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
}

impl SessionIndex {
    pub fn open(nori_home: &Path) -> Self {
        Self {
            inner: Arc::new(Inner {
                nori_home: nori_home.to_path_buf(),
                root: nori_home.join(INDEX_DIR).join(INDEX_VERSION_DIR),
                catalog: RwLock::new(None),
            }),
        }
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
                if path.extension().is_none_or(|extension| extension != "nidx") {
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
        digest::read_digest(&digest::digest_path(
            &self.inner.root,
            project_id,
            session_id,
        ))
        .ok()
    }

    /// Bring one session's digest up to date with its transcript. Returns whether it was
    /// rebuilt.
    pub async fn sync_session(&self, project_id: &str, session_id: &str) -> io::Result<bool> {
        let loader = self.transcripts();
        let source = source_stamp(&loader, project_id, session_id)?;
        let index = self.clone();
        let (project, session) = (project_id.to_owned(), session_id.to_owned());
        let current = tokio::task::spawn_blocking(move || {
            index.catalog_entry(&session).is_some_and(|entry| {
                entry.header.source == source && entry.header.project_id == project
            })
        })
        .await
        .map_err(io::Error::other)?;
        if current {
            return Ok(false);
        }
        let transcript = loader.load_transcript(project_id, session_id).await?;
        let index = self.clone();
        let project = project_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let digest = extract::extract(&transcript, &project, source);
            index.store(&digest)
        })
        .await
        .map_err(io::Error::other)??;
        Ok(true)
    }

    fn store(&self, digest: &SessionDigest) -> io::Result<()> {
        let header = &digest.header;
        let bloom = Bloom::build(digest.texts());
        digest::write_atomically(
            &digest::bloom_path(&self.inner.root, &header.project_id, &header.session_id),
            &bloom.encode(),
        )?;
        digest::write_atomically(
            &digest::digest_path(&self.inner.root, &header.project_id, &header.session_id),
            &digest::encode(digest)?,
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
                Arc::new(CatalogEntry {
                    header: header.clone(),
                    bloom,
                }),
            );
        }
        Ok(())
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
        let loader = self.transcripts();
        let mut sessions: Vec<SessionMetadata> = Vec::new();
        for project in loader.list_projects().await? {
            sessions.extend(loader.list_session_metadata(&project.id).await?);
        }
        sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));
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
        let _ = std::fs::remove_file(digest::digest_path(
            &self.inner.root,
            project_id,
            session_id,
        ));
        let _ = std::fs::remove_file(digest::bloom_path(&self.inner.root, project_id, session_id));
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
    /// session ended. Does nothing when the path is not under a `$NORI_HOME` layout. Spawns
    /// onto the current runtime; failures are logged, never raised.
    pub fn sync_after_session(transcript_path: &Path) {
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
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let index = Self::open(nori_home);
        let (project_id, session_id) = (project_id.to_owned(), session_id.to_owned());
        runtime.spawn(async move {
            if let Err(error) = index.sync_session(&project_id, &session_id).await {
                tracing::warn!(target: "nori_index", %session_id, %error, "could not index ended session");
            }
        });
    }
}

fn source_stamp(
    loader: &TranscriptLoader,
    project_id: &str,
    session_id: &str,
) -> io::Result<SourceStamp> {
    let path = loader.session_path(project_id, session_id);
    let mut stamp = SourceStamp {
        len: 0,
        mtime_ms: 0,
    };
    let mut found = false;
    for candidate in [path.clone(), path.with_extension("pkl")] {
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        found = true;
        stamp.len += i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0));
        stamp.mtime_ms = stamp.mtime_ms.max(modified);
    }
    if found {
        Ok(stamp)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "transcript is gone",
        ))
    }
}
