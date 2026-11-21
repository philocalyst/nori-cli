//! The sessions of a scope, as a listing needs them: the first record of each.
//!
//! A picker lists every session of a project, so what it reads per session has to stay the same
//! however long the session grew. The first record (the session's metadata) is that; the backend
//! bindings a provider handoff adds later are in the session index, which a listing merges in
//! (`SessionIndex::listing`). [`TranscriptLoader::load_session_meta`] is the exact read of one
//! session, bindings included.

use std::io;
use std::ops::ControlFlow;
use std::path::Path;

use super::SESSIONS_DIR;
use super::SessionMetaEntry;
use super::SessionMetadata;
use super::TranscriptEntry;
use super::TranscriptLine;
use super::TranscriptLoader;
use super::compute_project_id;

impl TranscriptLoader {
    /// Every session of the project `cwd` is in (of every project when `None`), most recently
    /// active first, each from its first record.
    pub async fn list_session_heads(&self, cwd: Option<&Path>) -> io::Result<Vec<SessionMetadata>> {
        let projects: Vec<String> = match cwd {
            Some(cwd) => vec![compute_project_id(cwd).await?.id],
            None => self
                .list_projects()
                .await?
                .into_iter()
                .map(|project| project.id)
                .collect(),
        };
        let base = self.transcripts_base();
        tokio::task::spawn_blocking(move || {
            let mut sessions = Vec::new();
            for project_id in projects {
                let Ok(entries) = std::fs::read_dir(base.join(&project_id).join(SESSIONS_DIR))
                else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path
                        .extension()
                        .is_none_or(|extension| extension != "jsonl")
                    {
                        continue;
                    }
                    match read_head(&path) {
                        Ok(meta) => sessions.push(SessionMetadata {
                            last_active_at: super::loader::last_recorded_at(&path)
                                .unwrap_or_else(|| meta.started_at.clone()),
                            session_id: meta.session_id,
                            project_id: project_id.clone(),
                            started_at: meta.started_at,
                            cwd: meta.cwd,
                            agent: meta.agent,
                            delegated: meta.delegated,
                        }),
                        Err(error) => tracing::warn!(
                            target: "nori_resume",
                            path = %path.display(),
                            %error,
                            "could not read a session's metadata",
                        ),
                    }
                }
            }
            sessions.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
            Ok(sessions)
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// The metadata a session recorded first. Blocking.
fn read_head(transcript: &Path) -> io::Result<SessionMetaEntry> {
    let journal = transcript.with_extension("pkl");
    let mut meta = None;
    if journal.exists() {
        nori_conversation::ConversationLog::scan(&journal, |record| {
            if let nori_conversation::Event::Operation { name, args } = &record.event
                && name == "transcript-entry"
                && let Ok(TranscriptLine {
                    entry: TranscriptEntry::SessionMeta(found),
                    ..
                }) = serde_json::from_str::<TranscriptLine>(args)
            {
                meta = Some(found);
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })?;
    } else {
        let mut first = String::new();
        io::BufRead::read_line(
            &mut io::BufReader::new(std::fs::File::open(transcript)?),
            &mut first,
        )?;
        if let Ok(TranscriptLine {
            entry: TranscriptEntry::SessionMeta(found),
            ..
        }) = serde_json::from_str::<TranscriptLine>(&first)
        {
            meta = Some(found);
        }
    }
    meta.ok_or_else(|| io::Error::other("the session recorded no metadata"))
}
