//! TranscriptLoader - Loads and lists transcripts for viewing.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncBufReadExt;

use super::BY_PROJECT_DIR;
use super::PROJECT_METADATA_FILE;
use super::SESSIONS_DIR;
use super::TRANSCRIPTS_DIR;
use super::project::compute_project_id;
use super::types::ContentBlock;
use super::types::SessionMetaEntry;
use super::types::TranscriptEntry;
use super::types::TranscriptLine;

/// Information about a project with transcripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    /// Hash-based project identifier
    pub id: String,
    /// Human-readable project name
    pub name: String,
    /// Git remote URL if available
    pub git_remote: Option<String>,
    /// Working directory
    pub cwd: PathBuf,
    /// Number of sessions in this project
    pub session_count: usize,
    /// Timestamp of most recent session
    pub last_session_at: Option<String>,
}

/// Information about a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Session identifier (UUID)
    pub session_id: String,
    /// Project identifier
    pub project_id: String,
    /// When the session started
    pub started_at: String,
    /// Working directory for the session
    pub cwd: PathBuf,
    /// ACP agent used for the session (e.g., "claude-code", "codex", "gemini")
    pub agent: Option<String>,
    /// Number of entries in the transcript (approximate)
    pub entry_count: usize,
}

/// Cheap session metadata loaded from the first transcript line only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Session identifier (UUID)
    pub session_id: String,
    /// Project identifier
    pub project_id: String,
    /// When the session started
    pub started_at: String,
    /// When anything was last recorded in the session (ISO 8601): what "most recent" means for
    /// a conversation that was resumed after it started. Falls back to `started_at`.
    pub last_active_at: String,
    /// Working directory for the session
    pub cwd: PathBuf,
    /// ACP agent used for the session (e.g., "claude-code", "codex", "gemini")
    pub agent: Option<String>,
    /// A delegated child: not a conversation of its own (see `SessionMetaEntry::delegated`).
    pub delegated: bool,
}

/// A loaded transcript with all entries.
#[derive(Debug, Clone)]
pub struct Transcript {
    /// Session metadata (first entry)
    pub meta: SessionMetaEntry,
    /// Storage entries, including the metadata.
    ///
    /// Kept private so transcript schema compatibility does not become part of
    /// the Harness API. Consumers should use [`Transcript::records`].
    pub(crate) entries: Vec<TranscriptLine>,
    pub(crate) backend_binding: Option<String>,
    /// A rewind was committed but no subsequent provider binding was recorded.
    pub(crate) requires_fresh_binding: bool,
    /// Canonical record IDs, aligned with entries for Pkl-backed sessions.
    pub(crate) event_ids: Vec<i64>,
    /// Visible entries older than [`Self::entries`] holds, when only the newest were read.
    pub(crate) left_out: i64,
}

/// A schema-independent record exposed by a loaded transcript.
#[derive(Debug, Clone, Copy)]
pub enum TranscriptRecord<'a> {
    /// User-authored text.
    User { content: &'a str },
    /// Agent-authored text.
    Assistant { content: &'a str },
    /// Agent reasoning text retained by legacy transcripts.
    Thinking { content: &'a str },
    /// The exact public event stored by schema-v3 transcripts.
    SessionEvent(&'a nori_protocol::SessionEvent),
}

impl PartialEq for TranscriptRecord<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::User { content: left }, Self::User { content: right })
            | (Self::Assistant { content: left }, Self::Assistant { content: right })
            | (Self::Thinking { content: left }, Self::Thinking { content: right }) => {
                left == right
            }
            (Self::SessionEvent(left), Self::SessionEvent(right)) => {
                serde_json::to_value(left).ok() == serde_json::to_value(right).ok()
            }
            _ => false,
        }
    }
}

impl Transcript {
    /// How many of the conversation's entries before the ones read were left on disk: a resume
    /// reads only the newest turns of a long conversation.
    pub fn left_out(&self) -> i64 {
        self.left_out
    }

    /// The agent session to resume: the one started last in this transcript. A resumed session
    /// goes on recording into the transcript it was resumed from, so a session started since
    /// (by a replay into a new session, or a compaction) can be newer than the metadata's.
    pub fn latest_acp_session_id(&self) -> Option<String> {
        if self.requires_fresh_binding {
            return None;
        }
        self.backend_binding.clone().or_else(|| {
            self.entries
                .iter()
                .rev()
                .find_map(|line| match &line.entry {
                    TranscriptEntry::SessionEvent(entry) if !entry.historical_evidence => {
                        match &entry.event {
                            nori_protocol::SessionEvent::Nori(
                                nori_protocol::NoriEvent::SessionStarted(started),
                            ) => Some(started.acp_session_id.to_string()),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .or_else(|| self.meta.acp_session_id.clone())
        })
    }

    /// The last usage the agent reported for this conversation: its context use and the cost
    /// so far. A resumed session shows these at once instead of waiting for the next turn.
    pub fn last_usage_update(&self) -> Option<nori_protocol::acp::v1::UsageUpdate> {
        use nori_protocol::AcpEvent;
        use nori_protocol::SessionEvent;
        use nori_protocol::acp::v1 as acp;
        let mut last = None;
        for record in self.records() {
            if let TranscriptRecord::SessionEvent(SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            ))) = record
                && let acp::SessionUpdate::UsageUpdate(usage) = &notification.update
            {
                last = Some(usage.clone());
            }
        }
        last
    }

    /// Iterate over records without exposing the versioned storage schema.
    pub fn records(&self) -> impl Iterator<Item = TranscriptRecord<'_>> {
        TranscriptRecords {
            entries: self.entries.iter(),
            assistant_blocks: None,
        }
    }
}

struct TranscriptRecords<'a> {
    entries: std::slice::Iter<'a, TranscriptLine>,
    assistant_blocks: Option<std::slice::Iter<'a, ContentBlock>>,
}

impl<'a> Iterator for TranscriptRecords<'a> {
    type Item = TranscriptRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(blocks) = &mut self.assistant_blocks {
                if let Some(block) = blocks.next() {
                    return Some(match block {
                        ContentBlock::Text { text } => {
                            TranscriptRecord::Assistant { content: text }
                        }
                        ContentBlock::Thinking { thinking } => {
                            TranscriptRecord::Thinking { content: thinking }
                        }
                    });
                }
                self.assistant_blocks = None;
            }

            match &self.entries.next()?.entry {
                TranscriptEntry::User(user) => {
                    return Some(TranscriptRecord::User {
                        content: &user.content,
                    });
                }
                TranscriptEntry::Assistant(assistant) => {
                    self.assistant_blocks = Some(assistant.content.iter());
                }
                TranscriptEntry::SessionEvent(entry) => {
                    if entry.historical_evidence {
                        continue;
                    }
                    return Some(TranscriptRecord::SessionEvent(&entry.event));
                }
                TranscriptEntry::SessionMeta(_)
                | TranscriptEntry::ClientEvent(_)
                | TranscriptEntry::ToolCall(_)
                | TranscriptEntry::ToolResult(_)
                | TranscriptEntry::PatchApply(_) => {}
            }
        }
    }
}

/// Loads and lists transcripts for viewing.
pub struct TranscriptLoader {
    nori_home: PathBuf,
}

impl TranscriptLoader {
    /// Create a new TranscriptLoader.
    pub fn new(nori_home: PathBuf) -> Self {
        Self { nori_home }
    }

    /// Get the base path for transcripts.
    pub(super) fn transcripts_base(&self) -> PathBuf {
        self.nori_home.join(TRANSCRIPTS_DIR).join(BY_PROJECT_DIR)
    }

    /// List all projects that have transcripts.
    pub async fn list_projects(&self) -> io::Result<Vec<ProjectInfo>> {
        let base_path = self.transcripts_base();

        if !base_path.exists() {
            return Ok(Vec::new());
        }

        let mut projects = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&base_path).await?;

        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Try to read project metadata
            let meta_path = path.join(PROJECT_METADATA_FILE);
            if let Ok(content) = tokio::fs::read_to_string(&meta_path).await
                && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content)
            {
                // Count sessions
                let sessions_path = path.join(SESSIONS_DIR);
                let session_count = count_sessions(&sessions_path).await.unwrap_or(0);

                // Get last session timestamp
                let last_session_at = get_last_session_timestamp(&sessions_path).await;

                projects.push(ProjectInfo {
                    id: meta["id"].as_str().unwrap_or_default().to_string(),
                    name: meta["name"].as_str().unwrap_or_default().to_string(),
                    git_remote: meta["git_remote"].as_str().map(String::from),
                    cwd: PathBuf::from(meta["cwd"].as_str().unwrap_or_default()),
                    session_count,
                    last_session_at,
                });
            }
        }

        // Sort by last session timestamp (most recent first)
        projects.sort_by(|a, b| b.last_session_at.as_ref().cmp(&a.last_session_at.as_ref()));

        Ok(projects)
    }

    /// List all sessions for a specific project.
    pub async fn list_sessions(&self, project_id: &str) -> io::Result<Vec<SessionInfo>> {
        let started = Instant::now();
        let sessions_path = self.transcripts_base().join(project_id).join(SESSIONS_DIR);

        if !sessions_path.exists() {
            tracing::info!(
                target: "nori_resume",
                phase = "transcript_loader.list_sessions.missing_dir",
                project_id,
                sessions_path = %sessions_path.display(),
                elapsed_ms = started.elapsed().as_millis(),
                "no transcript sessions directory found",
            );
            return Ok(Vec::new());
        }

        tracing::info!(
            target: "nori_resume",
            phase = "transcript_loader.list_sessions.start",
            project_id,
            sessions_path = %sessions_path.display(),
            "listing transcript sessions",
        );

        let mut sessions = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&sessions_path).await?;
        let mut file_count = 0usize;

        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                file_count += 1;
                match load_session_info(&path, project_id).await {
                    Ok(info) => sessions.push(info),
                    Err(error) => tracing::warn!(
                        target: "nori_resume",
                        phase = "transcript_loader.list_sessions.session_info_error",
                        project_id,
                        path = %path.display(),
                        error = %error,
                        "failed to load transcript session info",
                    ),
                }
            }
        }

        // Sort by started_at (most recent first)
        sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));

        tracing::info!(
            target: "nori_resume",
            phase = "transcript_loader.list_sessions.done",
            project_id,
            file_count,
            loaded_session_count = sessions.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "finished listing transcript sessions",
        );

        Ok(sessions)
    }

    /// List cheap session metadata for a specific project.
    ///
    /// This reads only the first line of each transcript, so it is suitable for
    /// building picker rows without blocking on giant transcript bodies.
    pub async fn list_session_metadata(
        &self,
        project_id: &str,
    ) -> io::Result<Vec<SessionMetadata>> {
        let started = Instant::now();
        let sessions_path = self.transcripts_base().join(project_id).join(SESSIONS_DIR);

        if !sessions_path.exists() {
            return Ok(Vec::new());
        }

        tracing::info!(
            target: "nori_resume",
            phase = "transcript_loader.list_session_metadata.start",
            project_id,
            sessions_path = %sessions_path.display(),
            "listing transcript session metadata",
        );

        let mut sessions = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&sessions_path).await?;
        let mut file_count = 0usize;

        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                file_count += 1;
                match load_session_metadata(&path, project_id).await {
                    Ok(info) => sessions.push(info),
                    Err(error) => tracing::warn!(
                        target: "nori_resume",
                        phase = "transcript_loader.list_session_metadata.error",
                        project_id,
                        path = %path.display(),
                        error = %error,
                        "failed to load transcript session metadata",
                    ),
                }
            }
        }

        sessions.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));

        tracing::info!(
            target: "nori_resume",
            phase = "transcript_loader.list_session_metadata.done",
            project_id,
            file_count,
            loaded_session_count = sessions.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "finished listing transcript session metadata",
        );

        Ok(sessions)
    }

    /// Find sessions for the current working directory.
    /// Useful for showing "recent sessions in this project".
    pub async fn find_sessions_for_cwd(&self, cwd: &Path) -> io::Result<Vec<SessionInfo>> {
        let started = Instant::now();
        tracing::info!(
            target: "nori_resume",
            phase = "transcript_loader.find_sessions_for_cwd.start",
            cwd = %cwd.display(),
            "finding transcript sessions for cwd",
        );

        // Compute project ID for the cwd
        let project_started = Instant::now();
        let project_id = compute_project_id(cwd).await?;
        tracing::info!(
            target: "nori_resume",
            phase = "transcript_loader.find_sessions_for_cwd.project_id",
            cwd = %cwd.display(),
            project_id = %project_id.id,
            project_name = %project_id.name,
            git_remote = project_id.git_remote.as_deref().unwrap_or("<none>"),
            elapsed_ms = project_started.elapsed().as_millis(),
            total_elapsed_ms = started.elapsed().as_millis(),
            "computed transcript project id",
        );

        let list_started = Instant::now();
        let sessions = self.list_sessions(&project_id.id).await?;
        tracing::info!(
            target: "nori_resume",
            phase = "transcript_loader.find_sessions_for_cwd.done",
            cwd = %cwd.display(),
            project_id = %project_id.id,
            session_count = sessions.len(),
            list_elapsed_ms = list_started.elapsed().as_millis(),
            total_elapsed_ms = started.elapsed().as_millis(),
            "finished finding transcript sessions for cwd",
        );
        Ok(sessions)
    }

    /// Find cheap session metadata for the current working directory.
    pub async fn find_session_metadata_for_cwd(
        &self,
        cwd: &Path,
    ) -> io::Result<Vec<SessionMetadata>> {
        let project_id = compute_project_id(cwd).await?;
        self.list_session_metadata(&project_id.id).await
    }

    /// Find one session by its transcript session id across all known projects.
    pub async fn find_session_metadata_by_id(
        &self,
        session_id: &str,
    ) -> io::Result<Option<SessionMetadata>> {
        // The session's file names it: only the project that has it is read, and only it.
        let base = self.transcripts_base();
        let mut projects = match tokio::fs::read_dir(&base).await {
            Ok(projects) => projects,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        while let Some(project) = projects.next_entry().await? {
            let Some(project_id) = project.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = self.session_path(&project_id, session_id);
            if tokio::fs::try_exists(&path).await.unwrap_or(false)
                || tokio::fs::try_exists(path.with_extension("pkl"))
                    .await
                    .unwrap_or(false)
            {
                return load_session_metadata(&path, &project_id).await.map(Some);
            }
        }
        Ok(None)
    }

    /// Load a complete transcript for display.
    pub async fn load_transcript(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> io::Result<Transcript> {
        let path = self
            .transcripts_base()
            .join(project_id)
            .join(SESSIONS_DIR)
            .join(format!("{session_id}.jsonl"));

        load_transcript_from_path(&path).await
    }

    /// Load the newest turns of a transcript, about `budget` bytes of entries (see
    /// [`RESUME_TAIL_BYTES`]), with its whole-history facts: what resuming it needs. A long
    /// conversation resumes in the time its last turns take to read.
    pub async fn load_transcript_tail(
        &self,
        project_id: &str,
        session_id: &str,
        budget: i64,
    ) -> io::Result<Transcript> {
        load_transcript_tail_from_path(&self.session_path(project_id, session_id), budget).await
    }

    /// Load just the session metadata (for quick listing).
    pub async fn load_session_meta(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> io::Result<SessionMetaEntry> {
        let path = self
            .transcripts_base()
            .join(project_id)
            .join(SESSIONS_DIR)
            .join(format!("{session_id}.jsonl"));

        load_session_meta_from_path(&path).await
    }

    /// Load the first user message preview without reading the whole transcript.
    pub async fn load_first_user_preview(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> io::Result<Option<String>> {
        let path = self.session_path(project_id, session_id);
        load_first_user_preview_from_path(&path).await
    }

    /// Count user turns by streaming transcript lines.
    pub async fn count_user_turns(&self, project_id: &str, session_id: &str) -> io::Result<usize> {
        let path = self.session_path(project_id, session_id);
        count_user_turns_from_path(&path).await
    }

    /// Get the path to a session's transcript file.
    pub fn session_path(&self, project_id: &str, session_id: &str) -> PathBuf {
        self.transcripts_base()
            .join(project_id)
            .join(SESSIONS_DIR)
            .join(format!("{session_id}.jsonl"))
    }
}

/// Read a transcript file's entries for seeding a forked transcript.
///
/// Drops the leading `SessionMeta` line (the fork mints its own metadata) and
/// returns the remaining entries in order. Reuses the tolerant transcript
/// loader so unrecognized legacy entries are skipped rather than aborting the
/// fork.
pub(crate) async fn read_seed_entries(path: &Path) -> io::Result<Vec<TranscriptEntry>> {
    let transcript = load_transcript_from_path(path).await?;
    Ok(transcript
        .entries
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut line)| {
            if let TranscriptEntry::SessionEvent(entry) = &mut line.entry {
                entry
                    .observation_id
                    .get_or_insert_with(|| format!("{}:{index}", transcript.meta.session_id));
                entry.observed_at.get_or_insert_with(|| line.ts.clone());
            }
            if let TranscriptEntry::SessionMeta(_) = line.entry {
                None
            } else {
                Some(line.entry)
            }
        })
        .collect())
}

/// Count the number of session files in a directory.
async fn count_sessions(sessions_path: &Path) -> io::Result<usize> {
    if !sessions_path.exists() {
        return Ok(0);
    }

    let mut count = 0;
    let mut read_dir = tokio::fs::read_dir(sessions_path).await?;

    while let Some(entry) = read_dir.next_entry().await? {
        if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            count += 1;
        }
    }

    Ok(count)
}

/// Get the timestamp of the most recent session.
async fn get_last_session_timestamp(sessions_path: &Path) -> Option<String> {
    if !sessions_path.exists() {
        return None;
    }

    let mut read_dir = tokio::fs::read_dir(sessions_path).await.ok()?;
    let mut latest: Option<String> = None;

    while let Some(entry) = read_dir.next_entry().await.ok()? {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(meta) = load_session_meta_from_path(&path).await
            && latest.as_ref().is_none_or(|l| meta.started_at > *l)
        {
            latest = Some(meta.started_at);
        }
    }

    latest
}

/// Load session info from a transcript file.
async fn load_session_info(path: &Path, project_id: &str) -> io::Result<SessionInfo> {
    let started = Instant::now();
    let transcript_bytes = tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .ok();
    tracing::info!(
        target: "nori_resume",
        phase = "transcript_loader.load_session_info.start",
        project_id,
        path = %path.display(),
        transcript_bytes,
        "loading transcript session metadata and line count",
    );

    let meta_started = Instant::now();
    let meta = load_session_meta_from_path(path).await?;
    tracing::info!(
        target: "nori_resume",
        phase = "transcript_loader.load_session_info.meta_loaded",
        project_id,
        path = %path.display(),
        session_id = %meta.session_id,
        agent = meta.agent.as_deref().unwrap_or("<unknown>"),
        elapsed_ms = meta_started.elapsed().as_millis(),
        total_elapsed_ms = started.elapsed().as_millis(),
        "loaded transcript session metadata",
    );

    // Count entries (approximate - just count lines)
    let read_started = Instant::now();
    let content = tokio::fs::read(path).await?;
    let read_elapsed_ms = read_started.elapsed().as_millis();
    let count_started = Instant::now();
    // Approximate: every line is an entry. A trailing line without a newline counts too.
    let entry_count = memchr::memchr_iter(b'\n', &content).count()
        + usize::from(content.last().is_some_and(|last| *last != b'\n'));
    tracing::info!(
        target: "nori_resume",
        phase = "transcript_loader.load_session_info.done",
        project_id,
        path = %path.display(),
        session_id = %meta.session_id,
        agent = meta.agent.as_deref().unwrap_or("<unknown>"),
        entry_count,
        transcript_bytes,
        read_elapsed_ms,
        count_elapsed_ms = count_started.elapsed().as_millis(),
        total_elapsed_ms = started.elapsed().as_millis(),
        "loaded transcript session info",
    );

    Ok(SessionInfo {
        session_id: meta.session_id,
        project_id: project_id.to_string(),
        started_at: meta.started_at,
        cwd: meta.cwd,
        agent: meta.agent,
        entry_count,
    })
}

async fn load_session_metadata(path: &Path, project_id: &str) -> io::Result<SessionMetadata> {
    let meta = load_session_meta_from_path(path).await?;
    let last_active_at = last_recorded_at(path).unwrap_or_else(|| meta.started_at.clone());
    Ok(SessionMetadata {
        session_id: meta.session_id,
        project_id: project_id.to_string(),
        started_at: meta.started_at,
        last_active_at,
        cwd: meta.cwd,
        agent: meta.agent,
        delegated: meta.delegated,
    })
}

/// When the transcript at `path` (or its canonical journal) was last written to.
pub(super) fn last_recorded_at(path: &Path) -> Option<String> {
    [path.to_path_buf(), path.with_extension("pkl")]
        .iter()
        .filter_map(|candidate| std::fs::metadata(candidate).ok()?.modified().ok())
        .max()
        .map(|modified| {
            chrono::DateTime::<chrono::Utc>::from(modified)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
}

/// Load session metadata from the first line of a transcript file.
async fn load_session_meta_from_path(path: &Path) -> io::Result<SessionMetaEntry> {
    let log_path = path.with_extension("pkl");
    if log_path.exists() {
        // The metadata as the full load derives it: the first metadata entry and the bindings,
        // found through the journal's index without decoding the conversation.
        return tokio::task::spawn_blocking(move || {
            let log = nori_conversation::ConversationLog::open(&log_path)?;
            journal_meta(&log).map(|(meta, _, _)| meta)
        })
        .await
        .map_err(io::Error::other)?;
    }
    let file = tokio::fs::File::open(path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();

    if let Some(first_line) = lines.next_line().await? {
        let parsed: TranscriptLine = serde_json::from_str(&first_line)
            .map_err(|e| io::Error::other(format!("failed to parse transcript line: {e}")))?;

        match parsed.entry {
            TranscriptEntry::SessionMeta(meta) => Ok(meta),
            _ => Err(io::Error::other(
                "first line of transcript is not session metadata",
            )),
        }
    } else {
        Err(io::Error::other("transcript file is empty"))
    }
}

#[derive(Deserialize)]
struct TranscriptLineKind {
    #[serde(rename = "type")]
    kind: String,
}

const FIRST_USER_PREVIEW_MAX_BYTES: usize = 1024 * 1024;

async fn load_first_user_preview_from_path(path: &Path) -> io::Result<Option<String>> {
    let log_path = path.with_extension("pkl");
    if log_path.exists() {
        // Read up to the first user message only.
        return tokio::task::spawn_blocking(move || {
            let mut preview = None;
            nori_conversation::ConversationLog::scan(&log_path, |record| {
                if let Some(Ok(line)) = transcript_entry(&record)
                    && let TranscriptEntry::User(user) = line.entry
                    && let Some(words) = preview_of(&user.content)
                {
                    preview = Some(words);
                    return std::ops::ControlFlow::Break(());
                }
                std::ops::ControlFlow::Continue(())
            })?;
            Ok(preview)
        })
        .await
        .map_err(io::Error::other)?;
    }
    let started = Instant::now();
    let file = tokio::fs::File::open(path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut bytes_seen = 0usize;
    let mut line_count = 0usize;

    while let Some(line) = lines.next_line().await? {
        line_count += 1;
        bytes_seen = bytes_seen.saturating_add(line.len() + 1);
        if bytes_seen > FIRST_USER_PREVIEW_MAX_BYTES {
            tracing::info!(
                target: "nori_resume",
                phase = "transcript_loader.load_first_user_preview.capped",
                path = %path.display(),
                line_count,
                bytes_seen,
                elapsed_ms = started.elapsed().as_millis(),
                "stopped first user preview scan at byte cap",
            );
            return Ok(None);
        }

        if line.trim().is_empty() {
            continue;
        }

        let Ok(kind) = serde_json::from_str::<TranscriptLineKind>(&line) else {
            continue;
        };
        if kind.kind != "user" {
            continue;
        }

        let parsed: TranscriptLine = serde_json::from_str(&line)
            .map_err(|e| io::Error::other(format!("failed to parse user line: {e}")))?;
        if let TranscriptEntry::User(user) = parsed.entry
            && let Some(words) = preview_of(&user.content)
        {
            return Ok(Some(words));
        }
    }

    Ok(None)
}

async fn count_user_turns_from_path(path: &Path) -> io::Result<usize> {
    let log_path = path.with_extension("pkl");
    if log_path.exists() {
        // Count what is visible: a rewind hides what came after its head. Only the ids and
        // whether each is a user message are kept, never the conversation itself.
        return tokio::task::spawn_blocking(move || {
            let mut visible: Vec<(i64, bool)> = Vec::new();
            nori_conversation::ConversationLog::scan(&log_path, |record| {
                match &record.event {
                    nori_conversation::Event::Rewind { head } => {
                        let keep = head.map_or(0, |head| {
                            visible
                                .iter()
                                .position(|(id, _)| *id == head)
                                .map_or(visible.len(), |index| index + 1)
                        });
                        visible.truncate(keep);
                    }
                    nori_conversation::Event::Operation { name, args }
                        if name == "transcript-entry" =>
                    {
                        let user = serde_json::from_str::<TranscriptLineKind>(args)
                            .is_ok_and(|kind| kind.kind == "user");
                        visible.push((record.id, user));
                    }
                    _ => visible.push((record.id, false)),
                }
                std::ops::ControlFlow::Continue(())
            })?;
            Ok(visible.iter().filter(|(_, user)| *user).count())
        })
        .await
        .map_err(io::Error::other)?;
    }
    let started = Instant::now();
    let file = tokio::fs::File::open(path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut count = 0usize;
    let mut line_count = 0usize;
    let mut bytes_seen = 0usize;

    while let Some(line) = lines.next_line().await? {
        line_count += 1;
        bytes_seen = bytes_seen.saturating_add(line.len() + 1);
        if line.trim().is_empty() {
            continue;
        }

        let Ok(kind) = serde_json::from_str::<TranscriptLineKind>(&line) else {
            continue;
        };
        if kind.kind == "user" {
            count += 1;
        }
    }

    tracing::info!(
        target: "nori_resume",
        phase = "transcript_loader.count_user_turns.done",
        path = %path.display(),
        user_turn_count = count,
        line_count,
        bytes_seen,
        elapsed_ms = started.elapsed().as_millis(),
        "counted user turns in transcript",
    );

    Ok(count)
}

/// The agent and ACP session a `backend-bound` command record names.
fn backend_bound(record: &nori_conversation::Record) -> Option<(i64, serde_json::Value)> {
    match &record.event {
        nori_conversation::Event::Command { name, payload } if name == "backend-bound" => {
            serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .map(|value| (record.id, value))
        }
        _ => None,
    }
}

/// Bring the first metadata of a conversation up to date with its last visible backend binding:
/// a rewind after the binding means the next run needs a fresh one. Returns the bound ACP
/// session and whether a fresh binding is required.
fn bind_meta(
    meta: &mut SessionMetaEntry,
    last_rewind: Option<i64>,
    visible_binding: Option<(i64, serde_json::Value)>,
) -> (Option<String>, bool) {
    let backend_binding = visible_binding
        .as_ref()
        .filter(|(id, _)| last_rewind.is_none_or(|rewind| *id > rewind))
        .and_then(|(_, value)| value.get("acp_session_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let requires_fresh_binding = last_rewind.is_some() && backend_binding.is_none();
    if requires_fresh_binding {
        meta.acp_session_id = None;
    }
    if let Some(agent) = visible_binding
        .as_ref()
        .and_then(|(_, value)| value.get("agent"))
        .and_then(serde_json::Value::as_str)
    {
        meta.agent = Some(agent.to_owned());
    }
    if let Some(session_id) = &backend_binding {
        meta.acp_session_id = Some(session_id.clone());
    }
    (backend_binding, requires_fresh_binding)
}

/// The transcript line a conversation-log record carries, if it carries one.
fn transcript_entry(record: &nori_conversation::Record) -> Option<io::Result<TranscriptLine>> {
    match &record.event {
        nori_conversation::Event::Operation { name, args } if name == "transcript-entry" => {
            Some(serde_json::from_str(args).map_err(io::Error::other))
        }
        _ => None,
    }
}

/// The start of what the user asked in `content`, without the context Nori injected; `None` when
/// the message was only that context.
fn preview_of(content: &str) -> Option<String> {
    let words = crate::injected_context::user_words(content);
    if words.is_empty() {
        None
    } else if words.chars().count() > 50 {
        let truncated: String = words.chars().take(50).collect();
        Some(format!("{truncated}..."))
    } else {
        Some(words)
    }
}

/// Load a complete transcript from a file.
///
/// Lines that fail to deserialize (e.g. unknown entry types from older or newer
/// versions) are silently skipped so that transcripts remain loadable across
/// schema changes.
/// A session belongs to the project it is filed under: one that moved between machines (or
/// homes) carries the project id of where it was recorded, and continues where it is now.
fn locate(meta: &mut SessionMetaEntry, path: &Path) {
    if let Some(project) = path
        .parent()
        .filter(|dir| {
            dir.file_name()
                .is_some_and(|name| name == super::SESSIONS_DIR)
        })
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    {
        meta.project_id = project.to_owned();
    }
}

/// The first session metadata a journal records (a fork's comes after its source anchor).
pub(super) fn session_meta_of(
    log: &nori_conversation::ConversationLog,
) -> io::Result<SessionMetaEntry> {
    let entries = (1..=i64::try_from(log.len()).unwrap_or(i64::MAX))
        .filter(|id| log.name(*id) == Some(TRANSCRIPT_ENTRY));
    for record in log.records(entries) {
        if let TranscriptEntry::SessionMeta(meta) = parse_entry(record)?.entry {
            return Ok(meta);
        }
    }
    Err(io::Error::other(
        "conversation log does not contain session metadata",
    ))
}

fn parse_entry(
    record: io::Result<std::sync::Arc<nori_conversation::Record>>,
) -> io::Result<super::types::TranscriptLine> {
    match &record?.event {
        nori_conversation::Event::Operation { args, .. } => {
            serde_json::from_str(args).map_err(io::Error::other)
        }
        _ => Err(io::Error::other("transcript entry is not an operation")),
    }
}

/// Whether `line` opens a turn: the person's prompt, in either transcript schema.
fn starts_turn(line: &super::types::TranscriptLine) -> bool {
    match &line.entry {
        TranscriptEntry::User(_) => true,
        TranscriptEntry::SessionEvent(entry) => {
            !entry.historical_evidence
                && matches!(
                    &entry.event,
                    nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                        nori_protocol::acp::v1::AgentNotification::SessionNotification(
                            notification
                        ),
                    )) if matches!(
                        notification.update,
                        nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(_)
                    )
                )
        }
        TranscriptEntry::SessionMeta(_)
        | TranscriptEntry::Assistant(_)
        | TranscriptEntry::ClientEvent(_)
        | TranscriptEntry::ToolCall(_)
        | TranscriptEntry::ToolResult(_)
        | TranscriptEntry::PatchApply(_) => false,
    }
}

/// A journal's session metadata brought up to date with its last visible backend binding, the
/// bound ACP session, and whether a rewind since requires a fresh binding.
fn journal_meta(
    log: &nori_conversation::ConversationLog,
) -> io::Result<(SessionMetaEntry, Option<String>, bool)> {
    let mut meta = session_meta_of(log)?;
    let last_rewind = (1..=i64::try_from(log.len()).unwrap_or(i64::MAX))
        .rev()
        .find(|id| log.kind(*id) == Some(nori_conversation::Kind::Rewind));
    let visible_binding = match log
        .visible_named(nori_conversation::Kind::Command, "backend-bound")
        .last()
    {
        Some(id) => backend_bound(log.record(*id)?.as_ref()),
        None => None,
    };
    let (backend_binding, requires_fresh_binding) =
        bind_meta(&mut meta, last_rewind, visible_binding);
    Ok((meta, backend_binding, requires_fresh_binding))
}

/// Load a journaled transcript: every visible entry, or with `tail` only the newest of them.
fn load_journal(log_path: &Path, path: &Path, tail: Option<i64>) -> io::Result<Transcript> {
    let started = Instant::now();
    let log = nori_conversation::ConversationLog::open(log_path)?;
    let indexed = started.elapsed();
    let entry_ids = log.visible_named(nori_conversation::Kind::Operation, TRANSCRIPT_ENTRY);
    let (event_ids, entries) = match tail {
        None => {
            let entries = log
                .records(entry_ids.iter().copied())
                .map(parse_entry)
                .collect::<io::Result<Vec<_>>>()?;
            (entry_ids.clone(), entries)
        }
        Some(budget) => newest_entries(&log, &entry_ids, budget)?,
    };
    let left_out = i64::try_from(entry_ids.len() - entries.len()).unwrap_or(i64::MAX);
    let (mut meta, backend_binding, requires_fresh_binding) = journal_meta(&log)?;
    locate(&mut meta, path);
    tracing::info!(
        target: "nori_resume",
        phase = "transcript_loader.load_journal",
        path = %path.display(),
        records = log.len(),
        entries = entries.len(),
        left_out,
        indexed_ms = indexed.as_millis(),
        elapsed_ms = started.elapsed().as_millis(),
        "loaded journal",
    );
    Ok(Transcript {
        meta,
        entries,
        backend_binding,
        requires_fresh_binding,
        event_ids,
        left_out,
    })
}

/// The newest of the transcript entries `ids` whose journal lines come to `budget` bytes,
/// rounded up to the start of a turn: newest first until the budget is spent and a turn starts;
/// a turn longer than a few budgets (an agent at work for hours) is cut inside, where its events
/// go on. Oldest first, with their ids.
pub(crate) fn newest_entries(
    log: &nori_conversation::ConversationLog,
    ids: &[nori_conversation::EventId],
    budget: i64,
) -> io::Result<(
    Vec<nori_conversation::EventId>,
    Vec<super::types::TranscriptLine>,
)> {
    let mut taken = Vec::new();
    let mut bytes = 0;
    for (id, record) in ids
        .iter()
        .rev()
        .copied()
        .zip(log.records(ids.iter().rev().copied()))
    {
        let line = parse_entry(record)?;
        let opens_turn = starts_turn(&line);
        bytes += log.line_len(id).unwrap_or(0);
        taken.push((id, line));
        if bytes >= budget && (opens_turn || bytes >= budget.saturating_mul(4)) {
            break;
        }
    }
    taken.reverse();
    Ok(taken.into_iter().unzip())
}

/// How much of a long conversation's newest history a resume reads: the bytes of its newest
/// entries, rounded up to the start of a turn. The screen shows its last screens of it; the
/// rest of a conversation is left on disk until something asks for it.
pub const RESUME_TAIL_BYTES: i64 = 512 << 10;

/// The name of the journal operation that holds a transcript entry.
const TRANSCRIPT_ENTRY: &str = "transcript-entry";

pub(super) async fn load_transcript_from_path(path: &Path) -> io::Result<Transcript> {
    load_from_path(path, None).await
}

/// The transcript at `path` with only its newest entries, about `budget` bytes of them from the
/// start of a turn: what a resume shows and continues from. Whole-history facts (its metadata,
/// binding) are read as for a full load.
pub(super) async fn load_transcript_tail_from_path(
    path: &Path,
    budget: i64,
) -> io::Result<Transcript> {
    load_from_path(path, Some(budget)).await
}

async fn load_from_path(path: &Path, tail: Option<i64>) -> io::Result<Transcript> {
    let log_path = path.with_extension("pkl");
    if log_path.exists() {
        let path = path.to_path_buf();
        // Indexing and decoding are work proportional to what is read: off the async workers.
        return tokio::task::spawn_blocking(move || load_journal(&log_path, &path, tail))
            .await
            .map_err(io::Error::other)?;
    }
    let started = Instant::now();
    let bytes = tokio::fs::read(path).await?;
    let transcript_bytes = bytes.len();
    tracing::info!(
        target: "nori_resume",
        phase = "transcript_loader.load_transcript.start",
        path = %path.display(),
        transcript_bytes,
        read_elapsed_ms = started.elapsed().as_millis(),
        "loading full transcript",
    );

    // Parsing is CPU work proportional to the file; it runs off the async workers, on every core.
    let parsed = tokio::task::spawn_blocking(move || super::parse::parse_transcript(&bytes))
        .await
        .map_err(io::Error::other)?;
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(super::parse::ParseFailure::BeforeMeta(error)) => {
            tracing::warn!(
                target: "nori_resume",
                phase = "transcript_loader.load_transcript.first_line_error",
                path = %path.display(),
                elapsed_ms = started.elapsed().as_millis(),
                %error,
                "failed to parse first transcript line",
            );
            return Err(io::Error::other(format!(
                "failed to parse transcript line: {error}"
            )));
        }
        Err(super::parse::ParseFailure::NoMeta) => {
            return Err(io::Error::other(
                "transcript does not contain session metadata",
            ));
        }
    };

    tracing::info!(
        target: "nori_resume",
        phase = "transcript_loader.load_transcript.done",
        path = %path.display(),
        session_id = %parsed.meta.session_id,
        agent = parsed.meta.agent.as_deref().unwrap_or("<unknown>"),
        line_count = parsed.line_count,
        parsed_entry_count = parsed.entries.len(),
        skipped_count = parsed.skipped_count,
        transcript_bytes,
        elapsed_ms = started.elapsed().as_millis(),
        "loaded full transcript",
    );

    let mut meta = parsed.meta;
    locate(&mut meta, path);
    Ok(Transcript {
        meta,
        entries: parsed.entries,
        backend_binding: None,
        requires_fresh_binding: false,
        event_ids: Vec::new(),
        left_out: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::recorder::TranscriptRecorder;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[tokio::test]
    async fn a_transcript_resumes_the_agent_session_it_started_last() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let recorder = TranscriptRecorder::new(
            nori_home,
            nori_home,
            None,
            "0.1.0",
            Some("acp-first".to_string()),
        )
        .await
        .unwrap();
        let started = |acp_session_id: &str| {
            super::super::SessionEventEntry::new(nori_protocol::SessionEvent::Nori(
                nori_protocol::NoriEvent::SessionStarted(nori_protocol::SessionStarted {
                    transcript_id: Some(recorder.session_id().to_string()),
                    acp_session_id: acp_session_id.to_string().into(),
                    cwd: "/work".into(),
                    transcript_path: None,
                    history_log_id: 0,
                    history_entry_count: 0,
                    config_options: Vec::new(),
                    swap: None,
                }),
            ))
        };
        recorder
            .record_session_event(&started("acp-first"))
            .await
            .unwrap();
        // Resumed into a new agent session, recorded into the same transcript.
        recorder
            .record_session_event(&started("acp-second"))
            .await
            .unwrap();
        // Evidence copied from another transcript names a session that is not this one's.
        let mut evidence = started("acp-elsewhere");
        evidence.historical_evidence = true;
        recorder.record_session_event(&evidence).await.unwrap();
        recorder.shutdown().await.unwrap();

        let transcript = TranscriptLoader::new(nori_home.to_path_buf())
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();

        assert_eq!(transcript.meta.acp_session_id.as_deref(), Some("acp-first"));
        assert_eq!(
            transcript.latest_acp_session_id().as_deref(),
            Some("acp-second")
        );
    }

    #[tokio::test]
    async fn test_list_projects_empty() {
        let temp_dir = TempDir::new().unwrap();
        let loader = TranscriptLoader::new(temp_dir.path().to_path_buf());

        let projects = loader.list_projects().await.unwrap();
        assert!(projects.is_empty());
    }

    #[tokio::test]
    async fn test_list_projects_with_sessions() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();

        // Create a session to populate the project
        let recorder = TranscriptRecorder::new(nori_home, nori_home, None, "0.1.0", None)
            .await
            .unwrap();
        recorder
            .record_user_message("msg-001", "Hello", vec![])
            .await
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Now list projects
        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let projects = loader.list_projects().await.unwrap();

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].session_count, 1);
    }

    #[tokio::test]
    async fn test_list_sessions_for_project() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();

        // Create two sessions
        let recorder1 = TranscriptRecorder::new(nori_home, nori_home, None, "0.1.0", None)
            .await
            .unwrap();
        let project_id = recorder1.project_id().to_string();
        recorder1.flush().await.unwrap();
        recorder1.shutdown().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let recorder2 = TranscriptRecorder::new(nori_home, nori_home, None, "0.1.0", None)
            .await
            .unwrap();
        recorder2.flush().await.unwrap();
        recorder2.shutdown().await.unwrap();

        // List sessions
        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let sessions = loader.list_sessions(&project_id).await.unwrap();

        assert_eq!(sessions.len(), 2);
        // Should be sorted by started_at descending
        assert!(sessions[0].started_at >= sessions[1].started_at);
    }

    #[tokio::test]
    async fn test_find_sessions_for_cwd() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();

        // Create a session
        let recorder = TranscriptRecorder::new(nori_home, nori_home, None, "0.1.0", None)
            .await
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Find sessions for same cwd
        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let sessions = loader.find_sessions_for_cwd(nori_home).await.unwrap();

        assert_eq!(sessions.len(), 1);
    }

    #[tokio::test]
    async fn test_load_transcript() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();

        // Create a session with entries
        let recorder = TranscriptRecorder::new(
            nori_home,
            nori_home,
            Some("claude".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();
        let project_id = recorder.project_id().to_string();
        let session_id = recorder.session_id().to_string();

        recorder
            .record_user_message("msg-001", "Hello", vec![])
            .await
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Load the transcript
        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let transcript = loader
            .load_transcript(&project_id, &session_id)
            .await
            .unwrap();

        assert_eq!(transcript.meta.session_id, session_id);
        assert_eq!(transcript.entries.len(), 2); // SessionMeta + User
    }

    #[tokio::test]
    async fn test_load_session_meta() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();

        // Create a session
        let recorder = TranscriptRecorder::new(
            nori_home,
            nori_home,
            Some("claude".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();
        let project_id = recorder.project_id().to_string();
        let session_id = recorder.session_id().to_string();
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Load just the metadata
        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let meta = loader
            .load_session_meta(&project_id, &session_id)
            .await
            .unwrap();

        assert_eq!(meta.session_id, session_id);
        assert_eq!(meta.agent, Some("claude".to_string()));
        assert_eq!(meta.cli_version, "0.1.0");
    }

    #[tokio::test]
    async fn test_list_sessions_empty_project() {
        let temp_dir = TempDir::new().unwrap();
        let loader = TranscriptLoader::new(temp_dir.path().to_path_buf());

        let sessions = loader.list_sessions("nonexistent").await.unwrap();
        assert!(sessions.is_empty());
    }

    /// Helper to write a raw JSONL transcript file with the given lines.
    async fn write_raw_transcript(
        nori_home: &Path,
        project_id: &str,
        session_id: &str,
        lines: &[&str],
    ) {
        let sessions_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(project_id)
            .join(SESSIONS_DIR);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

        // Write project metadata
        let project_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(project_id);
        let meta_path = project_dir.join(PROJECT_METADATA_FILE);
        let meta_json = serde_json::json!({
            "id": project_id,
            "name": "test-project",
            "cwd": "/tmp/test",
        });
        tokio::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&meta_json).unwrap(),
        )
        .await
        .unwrap();

        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let content = lines.join("\n") + "\n";
        tokio::fs::write(&path, content).await.unwrap();
    }

    async fn write_raw_transcript_bytes(
        nori_home: &Path,
        project_id: &str,
        session_id: &str,
        bytes: &[u8],
    ) {
        let sessions_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(project_id)
            .join(SESSIONS_DIR);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        tokio::fs::write(&path, bytes).await.unwrap();
    }

    #[tokio::test]
    async fn find_session_metadata_for_cwd_reads_only_session_meta() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let cwd = temp_dir.path().join("repo");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let recorder =
            TranscriptRecorder::new(nori_home, &cwd, Some("codex".to_string()), "0.1.0", None)
                .await
                .unwrap();
        let project_id = recorder.project_id().to_string();
        let session_id = recorder.session_id().to_string();
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        let transcript_path = recorder.transcript_path().to_path_buf();
        let meta_line = tokio::fs::read_to_string(&transcript_path).await.unwrap();
        let mut bytes = meta_line.into_bytes();
        bytes.extend_from_slice(&[0xff, b'\n']);
        tokio::fs::write(&transcript_path, bytes).await.unwrap();

        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let sessions = loader.find_session_metadata_for_cwd(&cwd).await.unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
        assert_eq!(sessions[0].project_id, project_id);
        assert_eq!(sessions[0].agent.as_deref(), Some("codex"));
    }

    #[tokio::test]
    async fn find_session_metadata_by_id_finds_sessions_across_projects() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let first_cwd = nori_home.join("first");
        let second_cwd = nori_home.join("second");
        tokio::fs::create_dir_all(&first_cwd).await.unwrap();
        tokio::fs::create_dir_all(&second_cwd).await.unwrap();

        let first_recorder = TranscriptRecorder::new(
            nori_home,
            &first_cwd,
            Some("claude-code".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();
        first_recorder.shutdown().await.unwrap();

        let second_recorder = TranscriptRecorder::new(
            nori_home,
            &second_cwd,
            Some("codex".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();
        let second_session_id = second_recorder.session_id().to_string();
        let second_project_id = second_recorder.project_id().to_string();
        second_recorder.shutdown().await.unwrap();

        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let found = loader
            .find_session_metadata_by_id(&second_session_id)
            .await
            .unwrap()
            .expect("session should be found");

        assert_eq!(found.session_id, second_session_id);
        assert_eq!(found.project_id, second_project_id);
        assert_eq!(found.agent.as_deref(), Some("codex"));
    }

    #[tokio::test]
    async fn find_session_metadata_by_id_returns_none_for_missing_session() {
        let temp_dir = TempDir::new().unwrap();
        let loader = TranscriptLoader::new(temp_dir.path().to_path_buf());

        let found = loader
            .find_session_metadata_by_id("missing-session")
            .await
            .unwrap();

        assert!(found.is_none());
    }

    #[tokio::test]
    async fn load_first_user_preview_stops_before_later_invalid_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let project_id = "test-project-preview";
        let session_id = "test-session-preview";

        let meta_line = serde_json::json!({
            "ts": "2025-01-27T12:00:00.000Z",
            "v": 2,
            "type": "session_meta",
            "session_id": session_id,
            "project_id": project_id,
            "started_at": "2025-01-27T12:00:00.000Z",
            "cwd": "/tmp/test",
            "agent": "codex",
            "cli_version": "0.1.0"
        });
        let user_line = serde_json::json!({
            "ts": "2025-01-27T12:00:01.000Z",
            "v": 2,
            "type": "user",
            "id": "msg-001",
            "content": "first prompt",
            "attachments": []
        });
        let mut bytes = format!("{meta_line}\n{user_line}\n").into_bytes();
        bytes.extend_from_slice(&[0xff, b'\n']);
        write_raw_transcript_bytes(nori_home, project_id, session_id, &bytes).await;

        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let preview = loader
            .load_first_user_preview(project_id, session_id)
            .await
            .unwrap();

        assert_eq!(preview.as_deref(), Some("first prompt"));
    }

    #[tokio::test]
    async fn count_user_turns_counts_only_user_messages() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let project_id = "test-project-count";
        let session_id = "test-session-count";

        let meta_line = serde_json::json!({
            "ts": "2025-01-27T12:00:00.000Z",
            "v": 2,
            "type": "session_meta",
            "session_id": session_id,
            "project_id": project_id,
            "started_at": "2025-01-27T12:00:00.000Z",
            "cwd": "/tmp/test",
            "agent": "codex",
            "cli_version": "0.1.0"
        });
        let user_one = serde_json::json!({
            "ts": "2025-01-27T12:00:01.000Z",
            "v": 2,
            "type": "user",
            "id": "msg-001",
            "content": "first",
            "attachments": []
        });
        let assistant = serde_json::json!({
            "ts": "2025-01-27T12:00:02.000Z",
            "v": 2,
            "type": "assistant",
            "id": "msg-002",
            "content": [{"type": "text", "text": "reply"}]
        });
        let tool_call = serde_json::json!({
            "ts": "2025-01-27T12:00:03.000Z",
            "v": 2,
            "type": "tool_call",
            "call_id": "call-001",
            "name": "shell",
            "input": {"cmd": "ls"}
        });
        let user_two = serde_json::json!({
            "ts": "2025-01-27T12:00:04.000Z",
            "v": 2,
            "type": "user",
            "id": "msg-003",
            "content": "second",
            "attachments": []
        });

        write_raw_transcript(
            nori_home,
            project_id,
            session_id,
            &[
                &meta_line.to_string(),
                &user_one.to_string(),
                &assistant.to_string(),
                &tool_call.to_string(),
                &user_two.to_string(),
            ],
        )
        .await;

        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let turn_count = loader
            .count_user_turns(project_id, session_id)
            .await
            .unwrap();

        assert_eq!(turn_count, 2);
    }

    #[tokio::test]
    async fn test_load_transcript_skips_unknown_entry_types() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let project_id = "test-project-123";
        let session_id = "test-session-456";

        // Valid session meta line
        let meta_line = serde_json::json!({
            "ts": "2025-01-27T12:00:00.000Z",
            "v": 2,
            "type": "session_meta",
            "session_id": session_id,
            "project_id": project_id,
            "started_at": "2025-01-27T12:00:00.000Z",
            "cwd": "/tmp/test",
            "cli_version": "0.1.0"
        });

        // Unknown entry type (simulates old turn_lifecycle from removed ClientEvent variant)
        let unknown_line = serde_json::json!({
            "ts": "2025-01-27T12:00:01.000Z",
            "v": 2,
            "type": "client_event",
            "event": {
                "event_type": "turn_lifecycle",
                "data": {"phase": "started"}
            }
        });

        // Valid user entry
        let user_line = serde_json::json!({
            "ts": "2025-01-27T12:00:02.000Z",
            "v": 2,
            "type": "user",
            "id": "msg-001",
            "content": "Hello",
            "attachments": []
        });

        write_raw_transcript(
            nori_home,
            project_id,
            session_id,
            &[
                &meta_line.to_string(),
                &unknown_line.to_string(),
                &user_line.to_string(),
            ],
        )
        .await;

        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let transcript = loader
            .load_transcript(project_id, session_id)
            .await
            .unwrap();

        // Should have loaded meta + user, skipping the unknown entry
        assert_eq!(transcript.meta.session_id, session_id);
        assert_eq!(transcript.entries.len(), 2);
        assert!(matches!(
            transcript.entries[0].entry,
            TranscriptEntry::SessionMeta(_)
        ));
        assert!(matches!(
            transcript.entries[1].entry,
            TranscriptEntry::User(_)
        ));
    }

    #[tokio::test]
    async fn test_load_transcript_with_only_unknown_entries() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let project_id = "test-project-789";
        let session_id = "test-session-abc";

        // Valid session meta line
        let meta_line = serde_json::json!({
            "ts": "2025-01-27T12:00:00.000Z",
            "v": 2,
            "type": "session_meta",
            "session_id": session_id,
            "project_id": project_id,
            "started_at": "2025-01-27T12:00:00.000Z",
            "cwd": "/tmp/test",
            "cli_version": "0.1.0"
        });

        // Only unknown entries after the meta
        let unknown1 = serde_json::json!({
            "ts": "2025-01-27T12:00:01.000Z",
            "v": 2,
            "type": "client_event",
            "event": {
                "event_type": "turn_lifecycle",
                "data": {"phase": "started"}
            }
        });
        let unknown2 = serde_json::json!({
            "ts": "2025-01-27T12:00:02.000Z",
            "v": 2,
            "type": "some_future_type",
            "payload": "whatever"
        });

        write_raw_transcript(
            nori_home,
            project_id,
            session_id,
            &[
                &meta_line.to_string(),
                &unknown1.to_string(),
                &unknown2.to_string(),
            ],
        )
        .await;

        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let transcript = loader
            .load_transcript(project_id, session_id)
            .await
            .unwrap();

        // Should load with just the meta entry, all unknown entries skipped
        assert_eq!(transcript.meta.session_id, session_id);
        assert_eq!(transcript.entries.len(), 1);
        assert!(matches!(
            transcript.entries[0].entry,
            TranscriptEntry::SessionMeta(_)
        ));
    }

    #[tokio::test]
    async fn test_load_transcript_fails_on_corrupt_first_line() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let project_id = "test-project-fail";
        let session_id = "test-session-fail";

        // First line is not valid session metadata
        let bad_first_line = serde_json::json!({
            "ts": "2025-01-27T12:00:00.000Z",
            "v": 2,
            "type": "some_future_type",
            "data": {}
        });

        write_raw_transcript(
            nori_home,
            project_id,
            session_id,
            &[&bad_first_line.to_string()],
        )
        .await;

        let loader = TranscriptLoader::new(nori_home.to_path_buf());
        let result = loader.load_transcript(project_id, session_id).await;
        assert!(result.is_err());
    }
}
