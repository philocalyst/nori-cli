//! TranscriptRecorder - Records transcript entries for a session.
//!
//! Uses async channel for non-blocking writes (same pattern as core RolloutRecorder).

use std::io;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use nori_conversation::ConversationLog;
use nori_conversation::Event;
use tokio::sync::Mutex;

use serde_json;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use super::BY_PROJECT_DIR;
use super::PROJECT_METADATA_FILE;
use super::SESSIONS_DIR;
use super::TRANSCRIPTS_DIR;
use super::project::ProjectId;
use super::project::compute_project_id;
use super::types::Attachment;
use super::types::GitInfo;
use super::types::SessionEventEntry;
use super::types::SessionMetaEntry;
use super::types::TranscriptEntry;
use super::types::TranscriptLine;
use super::types::UserEntry;
use super::types::now_iso8601;

/// Commands sent to the background writer task.
enum TranscriptCmd {
    /// One transcript line, already serialized (with its line end) for the journal.
    Write(String),
    Flush {
        ack: oneshot::Sender<()>,
    },
    Shutdown {
        ack: oneshot::Sender<()>,
    },
}

/// Records transcript entries for a session.
/// Uses async channel for non-blocking writes (same pattern as core RolloutRecorder).
#[derive(Clone)]
pub struct TranscriptRecorder {
    tx: Sender<TranscriptCmd>,
    conversation: Option<Arc<Mutex<ConversationLog>>>,
    /// Journal records a failed write held back, oldest first: written before anything newer.
    held: Arc<Mutex<Held>>,
    session_id: String,
    project_id: String,
    transcript_path: PathBuf,
    /// It appends to a session recorded before, which already holds the history a resume
    /// replays.
    continues_recorded_session: bool,
}

impl std::fmt::Debug for TranscriptRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TranscriptRecorder")
            .field("session_id", &self.session_id)
            .field("transcript_path", &self.transcript_path)
            .finish_non_exhaustive()
    }
}

impl TranscriptRecorder {
    /// Read the current canonical conversation as replay events, including durable response
    /// boundaries. Flush first so a caller can safely use those boundaries for history actions.
    pub async fn replay_session_events(&self) -> io::Result<Vec<nori_protocol::SessionEvent>> {
        self.flush().await?;
        let transcript = super::loader::load_transcript_from_path(&self.transcript_path).await?;
        Ok(crate::backend::transcript_to_replay_session_events(
            &transcript,
        ))
    }

    pub(crate) fn has_conversation_log(&self) -> bool {
        self.conversation.is_some()
    }

    /// Read a child journal with its exact terminal response receipts.
    pub async fn replay_child_session_events(
        &self,
    ) -> io::Result<Vec<nori_protocol::SessionEvent>> {
        self.flush().await?;
        let transcript = super::loader::load_transcript_from_path(&self.transcript_path).await?;
        Ok(crate::backend::transcript_to_child_replay_session_events(
            &transcript,
        ))
    }

    /// Initialize for a new session.
    ///
    /// - Detects project from cwd (git root or cwd path)
    /// - Creates project directory if needed
    /// - Opens new session JSONL file
    /// - Writes SessionMeta as first entry
    pub async fn new(
        nori_home: &Path,
        cwd: &Path,
        agent: Option<String>,
        cli_version: &str,
        acp_session_id: Option<String>,
    ) -> io::Result<Self> {
        Self::new_for(nori_home, cwd, agent, cli_version, acp_session_id, false).await
    }

    /// Like [`Self::new`] for a delegated child: its transcript says it belongs to another
    /// session's conversation, so it is never offered or resumed as a conversation of its own.
    pub async fn new_delegated(
        nori_home: &Path,
        cwd: &Path,
        agent: Option<String>,
        cli_version: &str,
        acp_session_id: Option<String>,
    ) -> io::Result<Self> {
        Self::new_for(nori_home, cwd, agent, cli_version, acp_session_id, true).await
    }

    /// [`Self::new`] or [`Self::new_delegated`], by `delegated`.
    pub(crate) async fn new_for(
        nori_home: &Path,
        cwd: &Path,
        agent: Option<String>,
        cli_version: &str,
        acp_session_id: Option<String>,
        delegated: bool,
    ) -> io::Result<Self> {
        // Both only need the cwd, and each waits on git, so they run together.
        let (project_id_info, git_info) =
            tokio::join!(compute_project_id(cwd), collect_git_info(cwd));
        let project_id_info = project_id_info?;

        // Create session ID (UUID)
        let session_id = generate_session_id();

        // Create directory structure
        let project_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(&project_id_info.id);
        let sessions_dir = project_dir.join(SESSIONS_DIR);
        tokio::fs::create_dir_all(&sessions_dir).await?;

        // Write/update project metadata
        let project_meta_path = project_dir.join(PROJECT_METADATA_FILE);
        write_project_metadata(&project_meta_path, &project_id_info).await?;

        // Create session transcript file
        let transcript_path = sessions_dir.join(format!("{session_id}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&transcript_path)
            .await?;

        // Set file permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&transcript_path, perms).await?;
        }

        // Create channel and spawn writer task
        let (tx, rx) = mpsc::channel::<TranscriptCmd>(256);

        // Create session metadata
        let session_meta = SessionMetaEntry {
            session_id: session_id.clone(),
            project_id: project_id_info.id.clone(),
            started_at: now_iso8601(),
            cwd: cwd.to_path_buf(),
            agent,
            cli_version: cli_version.to_string(),
            git: git_info,
            acp_session_id,
            forked_from: None,
            delegated,
        };

        let mut log = ConversationLog::open(transcript_path.with_extension("pkl"))?;
        let meta_line = TranscriptLine::new(TranscriptEntry::SessionMeta(session_meta.clone()));
        log.append(Event::Operation {
            name: "transcript-entry".into(),
            args: serde_json::to_string(&meta_line)?,
        })?;
        let conversation = Some(Arc::new(Mutex::new(log)));

        // Spawn background writer
        tokio::spawn(transcript_writer(file, rx, session_meta));

        Ok(Self {
            held: Arc::default(),
            tx,
            conversation,
            session_id,
            project_id: project_id_info.id,
            transcript_path,
            continues_recorded_session: false,
        })
    }

    /// Initialize a forked session seeded from a parent transcript.
    ///
    /// Mints a fresh session UUID, opens a new transcript file, and writes the
    /// `SessionMeta` (recording `forked_from` lineage and the new ACP session
    /// id) followed by `seed_entries` in a single batched write before
    /// returning, so the fork does not incur a per-line fsync storm on large
    /// parents.
    pub async fn new_forked(
        nori_home: &Path,
        cwd: &Path,
        agent: Option<String>,
        cli_version: &str,
        new_acp_session_id: String,
        forked_from: String,
        seed_entries: Vec<TranscriptEntry>,
    ) -> io::Result<Self> {
        let project_id_info = compute_project_id(cwd).await?;
        let session_id = generate_session_id();

        let project_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(&project_id_info.id);
        let sessions_dir = project_dir.join(SESSIONS_DIR);
        tokio::fs::create_dir_all(&sessions_dir).await?;

        let project_meta_path = project_dir.join(PROJECT_METADATA_FILE);
        write_project_metadata(&project_meta_path, &project_id_info).await?;

        let transcript_path = sessions_dir.join(format!("{session_id}.jsonl"));
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&transcript_path)
            .await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&transcript_path, perms).await?;
        }

        let git_info = collect_git_info(cwd).await;
        let session_meta = SessionMetaEntry {
            session_id: session_id.clone(),
            project_id: project_id_info.id.clone(),
            started_at: now_iso8601(),
            cwd: cwd.to_path_buf(),
            agent,
            cli_version: cli_version.to_string(),
            git: git_info,
            acp_session_id: Some(new_acp_session_id),
            forked_from: Some(forked_from.clone()),
            delegated: false,
        };

        let mut log = ConversationLog::open(transcript_path.with_extension("pkl"))?;
        let parent_path = sessions_dir.join(format!("{forked_from}.pkl"));
        if parent_path.exists() {
            let parent = ConversationLog::open(parent_path)?;
            if let (Some(start), Some(end)) =
                (parent.visible_ids().first(), parent.visible_ids().last())
            {
                let payload =
                    serde_json::json!({ "session_id": forked_from, "start": start, "end": end })
                        .to_string();
                log.append(Event::Command {
                    name: "fork-source".into(),
                    payload,
                })?;
            }
        }
        let meta_line = TranscriptLine::new(TranscriptEntry::SessionMeta(session_meta.clone()));
        log.append(Event::Operation {
            name: "transcript-entry".into(),
            args: serde_json::to_string(&meta_line)?,
        })?;
        for entry in &seed_entries {
            let line = TranscriptLine::new(entry.clone());
            log.append(Event::Operation {
                name: "transcript-entry".into(),
                args: serde_json::to_string(&line)?,
            })?;
        }
        let conversation = Some(Arc::new(Mutex::new(log)));

        // Batch the metadata line plus every seeded entry into a single write.
        let mut batch = serialize_line(&TranscriptLine::new(TranscriptEntry::SessionMeta(
            session_meta,
        )))?;
        for entry in seed_entries {
            batch.push_str(&serialize_line(&TranscriptLine::new(entry))?);
        }
        file.write_all(batch.as_bytes()).await?;
        file.flush().await?;

        let (tx, rx) = mpsc::channel::<TranscriptCmd>(256);
        tokio::spawn(run_writer_loop(file, rx));

        Ok(Self {
            held: Arc::default(),
            tx,
            conversation,
            session_id,
            project_id: project_id_info.id,
            transcript_path,
            continues_recorded_session: false,
        })
    }

    /// Continue recording the session that `transcript_path` holds, whose metadata is `meta`.
    ///
    /// What is recorded from now on is appended to that file under the session's own id, so a
    /// resumed session keeps one transcript instead of a new copy of it per resume.
    pub async fn continue_session(
        transcript_path: PathBuf,
        meta: &SessionMetaEntry,
    ) -> io::Result<Self> {
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&transcript_path)
            .await?;
        // A session that stopped in the middle of a line (a crash, a full disk) has that line
        // ended first, so that what is appended starts on a line of its own.
        if file.metadata().await?.len() > 0 {
            file.seek(io::SeekFrom::End(-1)).await?;
            let mut last = [0];
            file.read_exact(&mut last).await?;
            if last != *b"\n" {
                file.write_all(b"\n").await?;
            }
        }
        let log_path = transcript_path.with_extension("pkl");
        let conversation = if log_path.exists() {
            Some(Arc::new(Mutex::new(ConversationLog::open(log_path)?)))
        } else {
            None
        };
        let (tx, rx) = mpsc::channel::<TranscriptCmd>(256);
        tokio::spawn(run_writer_loop(file, rx));
        Ok(Self {
            held: Arc::default(),
            tx,
            conversation,
            session_id: meta.session_id.clone(),
            project_id: meta.project_id.clone(),
            transcript_path,
            continues_recorded_session: true,
        })
    }

    /// Whether this recorder appends to a session recorded before (see [`Self::continue_session`]).
    pub fn continues_recorded_session(&self) -> bool {
        self.continues_recorded_session
    }

    /// Record a user message.
    pub async fn record_user_message(
        &self,
        id: &str,
        content: &str,
        attachments: Vec<Attachment>,
    ) -> io::Result<()> {
        let entry = TranscriptEntry::User(UserEntry {
            id: id.to_string(),
            content: content.to_string(),
            attachments,
        });
        self.send_entry(entry).await
    }

    /// Record the exact public event delivered across the Harness boundary.
    pub(crate) async fn record_session_event(
        &self,
        event: &SessionEventEntry,
    ) -> io::Result<Option<i64>> {
        self.send_entry_with_id(TranscriptEntry::SessionEvent(event.clone()))
            .await
            .map(|ids| ids.map(|(_, id)| id))
    }

    /// Record an event the agent sent, with Nori's observation of it: the one journal record of
    /// that event. Returns the first visible record's id and the new record's, the range a
    /// response that ends here spans.
    pub(crate) async fn record_received_event(
        &self,
        event: &SessionEventEntry,
    ) -> io::Result<Option<(i64, i64)>> {
        self.send_entry_with_id(TranscriptEntry::SessionEvent(event.clone()))
            .await
    }

    /// Resolve a stable range within this session's current visible projection.
    pub(crate) async fn projected_fork_records(
        &self,
        session_id: &str,
        start: i64,
        end: i64,
    ) -> io::Result<Vec<nori_conversation::Record>> {
        if session_id != self.session_id {
            return Err(io::Error::other("fork source is not the active session"));
        }
        let log = self
            .conversation
            .as_ref()
            .ok_or_else(|| io::Error::other("legacy session has no event log"))?;
        log.lock()
            .await
            .slice(nori_conversation::SliceRef { start, end })?
            .ok_or_else(|| io::Error::other("fork source range is not visible"))
    }

    /// Capture the current visible conversation as provider fallback context.
    pub async fn visible_context(&self) -> io::Result<String> {
        self.visible_context_at_head()
            .await
            .map(|(context, _)| context)
    }

    /// The visible conversation as fallback context, and the head it was read at.
    pub async fn visible_context_at_head(&self) -> io::Result<(String, Option<i64>)> {
        let head = self.conversation_head().await;
        let context = self.projected_context(head).await?;
        Ok((context, head))
    }

    /// Compute the text fallback for a prospective head without modifying the log. Reading the
    /// whole conversation takes as long as it is long, so it runs on a blocking thread.
    pub(crate) async fn projected_context(&self, head: Option<i64>) -> io::Result<String> {
        let log = self
            .conversation
            .clone()
            .ok_or_else(|| io::Error::other("legacy session has no event log"))?;
        tokio::task::spawn_blocking(move || Self::project_context(&log, head))
            .await
            .map_err(io::Error::other)?
    }

    fn project_context(log: &Mutex<ConversationLog>, head: Option<i64>) -> io::Result<String> {
        let log = log.blocking_lock();
        let visible = log.visible_ids();
        let length = match head {
            Some(id) => visible
                .binary_search(&id)
                .map(|index| index + 1)
                .map_err(|_| io::Error::other("rewind head is not visible"))?,
            None => 0,
        };
        let visible = &visible[..length];
        let entry_ids = visible
            .iter()
            .copied()
            .filter(|id| log.name(*id) == Some("transcript-entry"))
            .collect::<Vec<_>>();
        // Only the newest history fits in any agent's context, so only that much is read: a
        // switch in a conversation of hundreds of megabytes is as quick as in a short one.
        let (_, entries) = super::loader::newest_entries(&log, &entry_ids, HANDOFF_TAIL_BYTES)?;
        let left_out = entry_ids.len() - entries.len();
        let meta = super::loader::session_meta_of(&log)?;
        let transcript = super::loader::Transcript {
            meta,
            entries,
            backend_binding: None,
            requires_fresh_binding: false,
            event_ids: Vec::new(),
            left_out: i64::try_from(left_out).unwrap_or(i64::MAX),
        };
        let earlier = if left_out > 0 {
            "Earlier turns of this conversation are left out; this is its newest part.\n"
        } else {
            ""
        };
        let has_fork_seed = visible
            .iter()
            .any(|id| log.name(*id) == Some("fork-context"));
        if has_fork_seed {
            // The archived source is provenance, not live context: a rewind may select an
            // earlier inherited response. Build from visible replay-safe entries so discarded
            // source events cannot leak into the replacement provider session.
            let updates: Vec<_> = crate::backend::transcript_to_replay_session_events(&transcript)
                .into_iter()
                .filter_map(|event| match event {
                    nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                        nori_protocol::acp::v1::AgentNotification::SessionNotification(
                            notification,
                        ),
                    )) => Some(notification.update),
                    _ => None,
                })
                .collect();
            Ok(format!(
                "{earlier}Visible conversation history:\n{}",
                serde_json::to_string(&updates).map_err(io::Error::other)?
            ))
        } else {
            Ok(format!(
                "{earlier}{}",
                crate::backend::transcript_to_summary(&transcript)
            ))
        }
    }

    /// The payloads of every visible record of the command `name`, oldest first.
    pub(crate) async fn journaled_commands(&self, name: &str) -> io::Result<Vec<String>> {
        Ok(self
            .journaled_commands_of(&[name])
            .await?
            .into_iter()
            .map(|(_, payload)| payload)
            .collect())
    }

    /// Every visible record of any of the commands `names`, as `(name, payload)`, oldest first:
    /// for readers whose records mean something relative to another command's.
    pub(crate) async fn journaled_commands_of(
        &self,
        names: &[&str],
    ) -> io::Result<Vec<(String, String)>> {
        let Some(log) = &self.conversation else {
            return Ok(Vec::new());
        };
        let log = log.lock().await;
        let ids = log.visible_where(|kind, name| {
            kind == nori_conversation::Kind::Command
                && name.is_some_and(|name| names.contains(&name))
        });
        log.records(ids)
            .filter_map(|record| match record {
                Ok(record) => match &record.event {
                    Event::Command { name, payload } => Some(Ok((name.clone(), payload.clone()))),
                    _ => None,
                },
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Most recent visible payload for a previously recorded command.
    pub(crate) async fn recorded_command_payload(&self, name: &str) -> Option<String> {
        let log = self.conversation.as_ref()?.lock().await;
        let newest = *log
            .visible_named(nori_conversation::Kind::Command, name)
            .last()?;
        match &log.record(newest).ok()?.event {
            Event::Command { payload, .. } => Some(payload.clone()),
            _ => None,
        }
    }

    /// Record the identity of an engine bound to this conversation.
    pub(crate) async fn record_backend_bound(&self, payload: String) -> io::Result<()> {
        self.record_command("backend-bound", payload).await
    }

    /// The id of the newest visible record: it changes with everything recorded, so equal heads
    /// mean an unchanged conversation without reading it again.
    pub async fn conversation_head(&self) -> Option<i64> {
        match &self.conversation {
            Some(log) => log.lock().await.head(),
            None => None,
        }
    }

    /// The ordered records in this journal's current visible projection.
    pub(crate) async fn visible_records(&self) -> io::Result<Vec<nori_conversation::Record>> {
        let log = self
            .conversation
            .as_ref()
            .ok_or_else(|| io::Error::other("legacy session has no event log"))?;
        let log = log.lock().await;
        log.visible()
            .map(|record| record.map(Arc::unwrap_or_clone))
            .collect()
    }

    /// The visible records `pick` chooses by kind and name, oldest first. Only those are read:
    /// the journal's index knows every record's kind and name, so asking for a few commands of a
    /// long conversation costs those commands.
    pub(crate) async fn visible_records_where(
        &self,
        pick: impl Fn(nori_conversation::Kind, Option<&str>) -> bool,
    ) -> io::Result<Vec<nori_conversation::Record>> {
        let log = self
            .conversation
            .as_ref()
            .ok_or_else(|| io::Error::other("legacy session has no event log"))?;
        let log = log.lock().await;
        let ids = log.visible_where(pick);
        log.records(ids)
            .map(|record| record.map(Arc::unwrap_or_clone))
            .collect()
    }

    /// The visible session events whose record mentions `needle` (an id, written literally into
    /// its line), oldest first. Lines are searched as written and only the matches decoded, so
    /// what a long conversation says about one child costs one read of it.
    pub(crate) async fn visible_session_events_mentioning(
        &self,
        needle: &str,
    ) -> io::Result<Vec<nori_protocol::SessionEvent>> {
        let log = self
            .conversation
            .as_ref()
            .ok_or_else(|| io::Error::other("legacy session has no event log"))?;
        let log = log.lock().await;
        let ids = log.visible_containing(needle)?;
        let mut events = Vec::new();
        for record in log.records(ids) {
            if let Event::Operation { name, args } = &record?.event
                && name == "transcript-entry"
                && let Ok(TranscriptLine {
                    entry: TranscriptEntry::SessionEvent(entry),
                    ..
                }) = serde_json::from_str::<TranscriptLine>(args)
                && !entry.historical_evidence
            {
                events.push(entry.event);
            }
        }
        Ok(events)
    }

    /// The first and last visible record ids, read from the journal's index.
    pub(crate) async fn visible_span(&self) -> io::Result<Option<(i64, i64)>> {
        let log = self
            .conversation
            .as_ref()
            .ok_or_else(|| io::Error::other("legacy session has no event log"))?;
        let log = log.lock().await;
        let ids = log.visible_ids();
        Ok(ids.first().copied().zip(ids.last().copied()))
    }

    /// Move the visible head while retaining all previous records.
    pub(crate) async fn rewind_head(&self, head: Option<i64>) -> io::Result<()> {
        let log = self
            .conversation
            .as_ref()
            .ok_or_else(|| io::Error::other("legacy session has no event log"))?;
        log.lock().await.append(Event::Rewind { head })?;
        Ok(())
    }

    /// Anchor a child session to an inclusive range in its source session.
    pub(crate) async fn record_fork_source(
        &self,
        session_id: String,
        start: i64,
        end: i64,
        context: String,
        entries: Vec<super::types::TranscriptLine>,
    ) -> io::Result<()> {
        self.record_command(
            "fork-source",
            serde_json::json!({"session_id": session_id, "start": start, "end": end}).to_string(),
        )
        .await?;
        self.record_command("fork-context", context).await?;
        for line in entries {
            self.send_entry(line.entry).await?;
        }
        Ok(())
    }

    /// Persist a submitted harness command before executing it.
    pub(crate) async fn record_command(&self, name: &str, payload: String) -> io::Result<()> {
        if let Some(log) = &self.conversation {
            log.lock().await.append(Event::Command {
                name: name.into(),
                payload,
            })?;
        }
        Ok(())
    }

    /// The typed-ahead prompts the journal says were queued when it ended, oldest first.
    pub(crate) async fn journaled_queue(
        &self,
    ) -> io::Result<Vec<crate::normalized::session_runtime::QueuedPrompt>> {
        let Some(log) = &self.conversation else {
            return Ok(Vec::new());
        };
        let log = log.lock().await;
        let ids = log.visible_named(
            nori_conversation::Kind::Command,
            crate::backend::queue_journal::QUEUE_COMMAND,
        );
        let records = log
            .records(ids)
            .filter_map(|record| match &record.ok()?.event {
                Event::Command { payload, .. } => {
                    serde_json::from_str::<crate::backend::queue_journal::QueueRecord>(payload).ok()
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        Ok(crate::backend::queue_journal::fold(records))
    }

    /// Flush all pending writes.
    pub async fn flush(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TranscriptCmd::Flush { ack: tx })
            .await
            .map_err(|e| IoError::other(format!("failed to queue transcript flush: {e}")))?;
        rx.await
            .map_err(|e| IoError::other(format!("failed waiting for transcript flush: {e}")))
    }

    /// Graceful shutdown.
    pub async fn shutdown(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TranscriptCmd::Shutdown { ack: tx })
            .await
            .map_err(|e| IoError::other(format!("failed to queue transcript shutdown: {e}")))?;
        rx.await
            .map_err(|e| IoError::other(format!("failed waiting for transcript shutdown: {e}")))?;
        // Records reach the disk in the background as they are appended; a clean end is where
        // the rest is made durable.
        if let Some(log) = self.conversation.clone() {
            let synced = tokio::task::spawn_blocking(move || log.blocking_lock().sync()).await;
            if let Ok(Err(error)) | Err(error) = synced.map_err(IoError::other) {
                tracing::warn!(%error, "could not flush the conversation journal");
            }
        }
        // The transcript is complete: bring its full-text index digest up to date, so the next
        // picker lists the session with its title.
        crate::session_index::SessionIndex::sync_after_session(
            &self.transcript_path,
            std::time::Duration::from_millis(1500),
        )
        .await;
        Ok(())
    }

    /// Get the path to this session's transcript file.
    pub fn transcript_path(&self) -> &Path {
        &self.transcript_path
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the project ID.
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Send an entry to the background writer.
    async fn send_entry(&self, entry: TranscriptEntry) -> io::Result<()> {
        self.send_entry_with_id(entry).await.map(drop)
    }

    /// Journal `entry` and queue it for the JSONL view. Returns the first visible record's id and
    /// the new record's, when the session has a journal.
    async fn send_entry_with_id(&self, entry: TranscriptEntry) -> io::Result<Option<(i64, i64)>> {
        // One serialization serves the journal and the JSONL view.
        let line = serialize_line(&TranscriptLine::new(entry))?;
        let id = if let Some(log) = &self.conversation {
            let mut log = log.lock().await;
            let args = line.trim_end_matches('\n').to_owned();
            let mut held = self.held.lock().await;
            match append_in_order(&mut log, &mut held, args)? {
                Some(record) => {
                    let start = log.visible_ids().first().copied().unwrap_or(record.id);
                    Some((start, record.id))
                }
                // Held back: written once writing works again; the conversation goes on.
                None => None,
            }
        } else {
            None
        };
        self.tx
            .send(TranscriptCmd::Write(line))
            .await
            .map_err(|e| IoError::other(format!("failed to queue transcript entry: {e}")))?;
        Ok(id)
    }
}

/// Background writer task that processes commands and writes to file.
async fn transcript_writer(
    mut file: File,
    rx: mpsc::Receiver<TranscriptCmd>,
    session_meta: SessionMetaEntry,
) -> io::Result<()> {
    // Write session metadata as the first line
    let meta_entry = TranscriptEntry::SessionMeta(session_meta);
    let line = TranscriptLine::new(meta_entry);
    write_line(&mut file, &line).await?;

    run_writer_loop(file, rx).await
}

/// Journal records a failed write held back, and their size.
#[derive(Default)]
struct Held {
    records: std::collections::VecDeque<String>,
    bytes: usize,
}

/// How much of a conversation's newest history a switch to another agent reads: comfortably more
/// than the most any agent is given (`ContextBudget::CEILING`), as journal lines carry more than
/// the summary keeps of them.
const HANDOFF_TAIL_BYTES: i64 = 8 << 20;

/// The most a session holds in memory while writes fail; past it the oldest records go (and the
/// log says so), so a disk that stays full cannot take the process down with it.
const MAX_HELD_BYTES: usize = 256 * 1024 * 1024;

/// Append what was held back, then `args`, in order. `Ok(None)` when the journal cannot be
/// written now (the disk is full, the write failed): the record waits in `held` and the session
/// goes on. A write cut short leaves the log needing a re-read, which happens first. Any other
/// error (another writer got there first) is returned as before, and nothing is held for it.
fn append_in_order(
    log: &mut ConversationLog,
    held: &mut Held,
    args: String,
) -> io::Result<Option<std::sync::Arc<nori_conversation::Record>>> {
    held.bytes += args.len();
    held.records.push_back(args);
    let mut last = None;
    while let Some(next) = held.records.front() {
        let event = Event::Operation {
            name: "transcript-entry".into(),
            args: next.clone(),
        };
        let appended = match log.append(event.clone()) {
            Err(error) if needs_refresh(&error) => log.refresh().and_then(|()| log.append(event)),
            other => other,
        };
        match appended {
            Ok(record) => {
                if let Some(done) = held.records.pop_front() {
                    held.bytes -= done.len();
                }
                last = Some(record);
            }
            Err(error) if is_write_failure(&error) => {
                crate::storage_health::failed("this conversation's journal", &error);
                while held.bytes > MAX_HELD_BYTES {
                    let Some(dropped) = held.records.pop_front() else {
                        break;
                    };
                    held.bytes -= dropped.len();
                    tracing::error!("dropped a journal record held while writes fail");
                }
                return Ok(None);
            }
            Err(error) => {
                if let Some(new) = held.records.pop_back() {
                    held.bytes -= new.len();
                }
                return Err(error);
            }
        }
    }
    crate::storage_health::recovered();
    Ok(last)
}

/// The journal refuses appends after a write was cut short, until it is re-read.
fn needs_refresh(error: &io::Error) -> bool {
    error.to_string().contains("refreshed")
}

/// A failure of the disk, not of the journal's own rules: what holding the record waits out.
fn is_write_failure(error: &io::Error) -> bool {
    crate::storage_health::is_full(error)
        || needs_refresh(error)
        || matches!(
            error.kind(),
            io::ErrorKind::WriteZero
                | io::ErrorKind::StorageFull
                | io::ErrorKind::ReadOnlyFilesystem
        )
        || error.raw_os_error() == Some(5)
}

/// Process writer commands against an already-opened, already-seeded file. The JSONL file is a
/// view of the journal: a line that cannot be written is skipped (the journal keeps it), and
/// whatever a failed write left of it is cut off so the next line starts clean.
async fn run_writer_loop(mut file: File, mut rx: mpsc::Receiver<TranscriptCmd>) -> io::Result<()> {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            TranscriptCmd::Write(line) => {
                let before = file.metadata().await.map(|meta| meta.len()).ok();
                let written = async {
                    file.write_all(line.as_bytes()).await?;
                    file.flush().await
                }
                .await;
                if let Err(error) = written {
                    crate::storage_health::failed("this conversation's transcript", &error);
                    if let Some(before) = before {
                        let _ = file.set_len(before).await;
                        let _ = file.seek(std::io::SeekFrom::Start(before)).await;
                    }
                }
            }
            TranscriptCmd::Flush { ack } => {
                if let Err(e) = file.flush().await {
                    let _ = ack.send(());
                    return Err(e);
                }
                let _ = ack.send(());
            }
            TranscriptCmd::Shutdown { ack } => {
                let _ = file.flush().await;
                let _ = ack.send(());
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Serialize a transcript line into its newline-terminated JSONL form.
fn serialize_line(line: &TranscriptLine) -> io::Result<String> {
    let mut json = serde_json::to_string(line)?;
    json.push('\n');
    Ok(json)
}

/// Write a single JSONL line to the file.
async fn write_line(file: &mut File, line: &TranscriptLine) -> io::Result<()> {
    let json = serialize_line(line)?;
    file.write_all(json.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

/// Generate a UUID for the session ID.
fn generate_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Write project metadata to project.json.
async fn write_project_metadata(path: &Path, project_id: &ProjectId) -> io::Result<()> {
    use serde_json::json;

    let now = now_iso8601();

    let metadata = json!({
        "id": project_id.id,
        "name": project_id.name,
        "git_remote": project_id.git_remote,
        "git_root": project_id.git_root,
        "cwd": project_id.cwd,
        "created_at": now,
        "updated_at": now,
    });

    let content = serde_json::to_string_pretty(&metadata)?;
    tokio::fs::write(path, content).await
}

/// Collect git info for session metadata.
async fn collect_git_info(cwd: &Path) -> Option<GitInfo> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let repo = nori_config::GitRepo::discover(&cwd)?;
        Some(GitInfo {
            branch: repo.branch(),
            commit_hash: repo.head_commit(),
        })
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_transcript_recorder_creates_directory_structure() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let cwd = temp_dir.path();

        let recorder = TranscriptRecorder::new(
            nori_home,
            cwd,
            Some("claude-code".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();

        // Verify directory structure was created
        let project_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(recorder.project_id());
        assert!(project_dir.exists());
        assert!(project_dir.join(SESSIONS_DIR).exists());
        assert!(project_dir.join(PROJECT_METADATA_FILE).exists());

        // Verify transcript file was created
        assert!(recorder.transcript_path().exists());

        recorder.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_resumed_session_goes_on_recording_into_its_own_transcript() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let first = TranscriptRecorder::new(nori_home, nori_home, None, "0.1.0", None)
            .await
            .unwrap();
        first
            .record_user_message("msg-1", "first turn", vec![])
            .await
            .unwrap();
        first.shutdown().await.unwrap();
        // The session stopped in the middle of writing a line.
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(first.transcript_path())
            .await
            .unwrap()
            .write_all(b"{\"ts\":\"2026")
            .await
            .unwrap();
        let loader = super::super::TranscriptLoader::new(nori_home.to_path_buf());
        let transcript = loader
            .load_transcript(first.project_id(), first.session_id())
            .await
            .unwrap();

        let resumed = TranscriptRecorder::continue_session(
            first.transcript_path().to_path_buf(),
            &transcript.meta,
        )
        .await
        .unwrap();
        resumed
            .record_user_message("msg-2", "second turn", vec![])
            .await
            .unwrap();
        resumed.shutdown().await.unwrap();

        assert!(resumed.continues_recorded_session());
        assert_eq!(resumed.session_id(), first.session_id());
        assert_eq!(resumed.transcript_path(), first.transcript_path());
        let sessions = std::fs::read_dir(first.transcript_path().parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(sessions, 2);
        let reloaded = loader
            .load_transcript(first.project_id(), first.session_id())
            .await
            .unwrap();
        let turns: Vec<&str> = reloaded
            .entries
            .iter()
            .filter_map(|line| match &line.entry {
                TranscriptEntry::User(user) => Some(user.content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(turns, vec!["first turn", "second turn"]);
    }

    #[tokio::test]
    async fn projected_backend_binding_tracks_rewind() {
        let dir = TempDir::new().unwrap();
        let recorder = TranscriptRecorder::new(
            dir.path(),
            dir.path(),
            Some("first".into()),
            "0.1.0",
            Some("first-id".into()),
        )
        .await
        .unwrap();
        recorder
            .record_backend_bound(
                serde_json::json!({"agent":"first", "acp_session_id":"first-id"}).to_string(),
            )
            .await
            .unwrap();
        let head = recorder.conversation_head().await.unwrap();
        recorder
            .record_backend_bound(
                serde_json::json!({"agent":"second", "acp_session_id":"second-id"}).to_string(),
            )
            .await
            .unwrap();
        let loader = super::super::TranscriptLoader::new(dir.path().to_path_buf());
        let before = loader
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        assert_eq!(
            (
                before.meta.agent.as_deref(),
                before.latest_acp_session_id().as_deref()
            ),
            (Some("second"), Some("second-id"))
        );
        let listed = loader
            .load_session_meta(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        assert_eq!(
            (listed.agent.as_deref(), listed.acp_session_id.as_deref()),
            (Some("second"), Some("second-id"))
        );
        recorder.rewind_head(Some(head)).await.unwrap();
        let listed = loader
            .load_session_meta(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        assert_eq!(
            (listed.agent.as_deref(), listed.acp_session_id.as_deref()),
            (Some("first"), None)
        );
        let after = loader
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        assert_eq!(
            (
                after.meta.agent.as_deref(),
                after.latest_acp_session_id().as_deref()
            ),
            (Some("first"), None)
        );
        assert!(after.requires_fresh_binding);
        recorder
            .record_backend_bound(
                serde_json::json!({"agent":"first", "acp_session_id":"replacement-id"}).to_string(),
            )
            .await
            .unwrap();
        let rebound = loader
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        assert_eq!(
            rebound.latest_acp_session_id().as_deref(),
            Some("replacement-id")
        );
        assert!(!rebound.requires_fresh_binding);
        recorder.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn handoff_audit_is_persisted_but_hidden_from_transcript_replay() {
        let dir = TempDir::new().unwrap();
        let recorder = TranscriptRecorder::new(
            dir.path(),
            dir.path(),
            Some("mock-model".into()),
            "0.1.0",
            Some("old-engine".into()),
        )
        .await
        .unwrap();
        recorder
            .record_user_message("request-1", "Keep working on this", Vec::new())
            .await
            .unwrap();
        recorder
            .record_command(
                "handoff-attempt",
                serde_json::json!({
                    "status": "failed",
                    "provider": "mock-model-alt",
                    "model": "secret-ranked-candidate",
                })
                .to_string(),
            )
            .await
            .unwrap();

        recorder.shutdown().await.unwrap();
        let log = ConversationLog::open(recorder.transcript_path().with_extension("pkl")).unwrap();
        assert!(log.visible().any(|record| matches!(
            &record.unwrap().event,
            Event::Command { name, payload }
                if name == "handoff-attempt" && payload.contains("secret-ranked-candidate")
        )));

        let transcript = super::super::TranscriptLoader::new(dir.path().to_path_buf())
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        let summary = crate::backend::transcript_to_summary(&transcript);
        let replay_client_events = crate::backend::transcript_to_replay_client_events(&transcript);
        let replay_session_events =
            crate::backend::transcript_to_replay_session_events(&transcript);
        let replay =
            serde_json::to_string(&(summary, replay_client_events, replay_session_events)).unwrap();
        assert!(!replay.contains("handoff-attempt"), "{replay}");
        assert!(!replay.contains("mock-model-alt"), "{replay}");
        assert!(!replay.contains("secret-ranked-candidate"), "{replay}");
    }

    #[tokio::test]
    async fn projected_context_keeps_visible_tool_input_and_output() {
        use nori_protocol::acp::v1 as acp;
        let dir = TempDir::new().unwrap();
        let recorder = TranscriptRecorder::new(dir.path(), dir.path(), None, "0.1.0", None)
            .await
            .unwrap();
        recorder
            .record_user_message("one", "inspect", vec![])
            .await
            .unwrap();
        let call = acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new("tool-1"), "Read")
                .raw_input(serde_json::json!({"path": "example.txt"})),
        );
        let result = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new("tool-1"),
            acp::ToolCallUpdateFields::new()
                .status(acp::ToolCallStatus::Completed)
                .raw_output(serde_json::json!({"text": "visible result"})),
        ));
        for update in [call, result] {
            let event = nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(acp::SessionNotification::new(
                    "session", update,
                )),
            ));
            recorder
                .record_session_event(&SessionEventEntry::new(event))
                .await
                .unwrap();
        }
        let head = recorder.conversation_head().await.unwrap();
        recorder
            .record_user_message("two", "discarded text", vec![])
            .await
            .unwrap();
        let context = recorder.projected_context(Some(head)).await.unwrap();
        let user = context.find("User: inspect").unwrap();
        let input = context.find("example.txt").unwrap();
        let output = context.find("visible result").unwrap();
        assert!(user < input && input < output);
        assert!(!context.contains("discarded text"));
        recorder.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reopened_replay_retains_response_range_and_hidden_history() {
        use nori_protocol::acp::v1 as acp;
        let dir = TempDir::new().unwrap();
        let recorder = TranscriptRecorder::new(dir.path(), dir.path(), None, "0.1.0", None)
            .await
            .unwrap();
        recorder
            .record_user_message("one", "first", vec![])
            .await
            .unwrap();
        let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
            acp::ContentBlock::Text(acp::TextContent::new("answer")),
        ));
        let event = nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
            acp::AgentNotification::SessionNotification(acp::SessionNotification::new(
                "session", update,
            )),
        ));
        let end = recorder
            .record_session_event(&SessionEventEntry::new(event))
            .await
            .unwrap()
            .unwrap();
        recorder
            .record_user_message("two", "hidden", vec![])
            .await
            .unwrap();
        recorder.rewind_head(Some(end)).await.unwrap();
        let live_replay = recorder.replay_session_events().await.unwrap();
        assert!(
            matches!(live_replay.last(), Some(nori_protocol::SessionEvent::Nori(
            nori_protocol::NoriEvent::ConversationResponseBoundary(boundary)
        )) if boundary.transcript_id == recorder.session_id() && boundary.end == end)
        );
        assert!(
            !serde_json::to_string(&live_replay)
                .unwrap()
                .contains("hidden")
        );
        recorder.shutdown().await.unwrap();
        let loader = super::super::TranscriptLoader::new(dir.path().to_path_buf());
        let loaded = loader
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        let replay = crate::backend::transcript_to_replay_session_events(&loaded);
        assert!(
            matches!(replay.last(), Some(nori_protocol::SessionEvent::Nori(
            nori_protocol::NoriEvent::ConversationResponseBoundary(boundary)
        )) if boundary.transcript_id == recorder.session_id() && boundary.end == end)
        );
        let log = ConversationLog::open(recorder.transcript_path().with_extension("pkl")).unwrap();
        assert!(log.all().any(|record| match &record.unwrap().event {
            Event::Operation { args, .. } => args.contains("hidden"),
            _ => false,
        }));
        assert!(!loaded.entries.iter().any(
            |line| matches!(&line.entry, TranscriptEntry::User(user) if user.content == "hidden")
        ));
    }

    #[tokio::test]
    async fn canonical_projection_survives_rewind_and_reload() {
        let dir = TempDir::new().unwrap();
        let recorder = TranscriptRecorder::new(dir.path(), dir.path(), None, "0.1.0", None)
            .await
            .unwrap();
        recorder
            .record_user_message("one", "first", vec![])
            .await
            .unwrap();
        recorder
            .record_user_message("two", "hidden", vec![])
            .await
            .unwrap();
        recorder.shutdown().await.unwrap();

        let path = recorder.transcript_path().with_extension("pkl");
        let mut log = ConversationLog::open(&path).unwrap();
        let head = log.visible_ids()[1];
        log.append(Event::Rewind { head: Some(head) }).unwrap();
        drop(log);
        let loader = super::super::TranscriptLoader::new(dir.path().to_path_buf());
        let loaded = loader
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
        let messages: Vec<_> = loaded
            .entries
            .iter()
            .filter_map(|line| match &line.entry {
                TranscriptEntry::User(user) => Some(user.content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(messages, vec!["first"]);
        assert_eq!(
            loader
                .count_user_turns(recorder.project_id(), recorder.session_id())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            loader
                .load_first_user_preview(recorder.project_id(), recorder.session_id())
                .await
                .unwrap()
                .as_deref(),
            Some("first")
        );
        let reopened = ConversationLog::open(path).unwrap();
        assert_eq!(reopened.len(), 4);
    }

    #[tokio::test]
    async fn test_transcript_recorder_writes_session_meta() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let cwd = temp_dir.path();

        let recorder = TranscriptRecorder::new(
            nori_home,
            cwd,
            Some("claude-code".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();

        // Give the writer a moment to write the session meta
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Read the transcript file
        let content = tokio::fs::read_to_string(recorder.transcript_path())
            .await
            .unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert!(!lines.is_empty());

        // First line should be session_meta
        let first_line: TranscriptLine = serde_json::from_str(lines[0]).unwrap();
        match first_line.entry {
            TranscriptEntry::SessionMeta(meta) => {
                assert_eq!(meta.session_id, recorder.session_id());
                assert_eq!(meta.project_id, recorder.project_id());
                assert_eq!(meta.cli_version, "0.1.0");
                assert_eq!(meta.agent, Some("claude-code".to_string()));
            }
            _ => panic!("Expected SessionMeta entry"),
        }
    }

    #[tokio::test]
    async fn test_transcript_recorder_records_user_message() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let cwd = temp_dir.path();

        let recorder = TranscriptRecorder::new(nori_home, cwd, None, "0.1.0", None)
            .await
            .unwrap();

        recorder
            .record_user_message("msg-001", "Hello, world!", vec![])
            .await
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Read the transcript file
        let content = tokio::fs::read_to_string(recorder.transcript_path())
            .await
            .unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(lines.len(), 2); // SessionMeta + User message

        let user_line: TranscriptLine = serde_json::from_str(lines[1]).unwrap();
        match user_line.entry {
            TranscriptEntry::User(user) => {
                assert_eq!(user.id, "msg-001");
                assert_eq!(user.content, "Hello, world!");
            }
            _ => panic!("Expected User entry"),
        }
    }
}
