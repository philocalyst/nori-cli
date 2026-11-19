//! Which sessions are children of which: a session is a delegated child when its transcript
//! says so, or when it opens with Nori's delegation brief (transcripts recorded before children
//! were marked).
//! Children are never conversations of their own: they are not resumed, and the most recent
//! session is the most recent root.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;

use futures::StreamExt;

use super::DigestHeader;
use super::SessionIndex;
use crate::transcript::SessionMetadata;

/// How many transcripts are digested at once to answer a lineage question.
const CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Default)]
pub struct Lineage {
    /// Child session id to the root it was delegated from.
    parents: HashMap<String, String>,
    children: HashSet<String>,
}

impl Lineage {
    /// A child is a session flagged as delegated. Its parent is the root of the same project
    /// that was running when it started: the latest one started before it, preferring one still
    /// being written then. A child no root started before has no known parent.
    pub fn of(headers: &[DigestHeader]) -> Self {
        let mut lineage = Self::default();
        let roots: Vec<&DigestHeader> = headers.iter().filter(|header| !header.delegated).collect();
        for child in headers.iter().filter(|header| header.delegated) {
            lineage.children.insert(child.session_id.clone());
            // Roots of its project that had started; of those, one still being written when the
            // child began is preferred, then the latest to start.
            let candidates = roots.iter().filter(|root| {
                root.project_id == child.project_id && root.started_at <= child.started_at
            });
            let parent = candidates
                .max_by_key(|root| (root.source.mtime_ms >= started_ms(child), &root.started_at));
            if let Some(parent) = parent {
                lineage
                    .parents
                    .insert(child.session_id.clone(), parent.session_id.clone());
            }
        }
        lineage
    }

    /// The root this session was delegated from, when known.
    pub fn parent_of(&self, header: &DigestHeader) -> Option<&str> {
        self.parents.get(&header.session_id).map(String::as_str)
    }

    pub fn is_child(&self, header: &DigestHeader) -> bool {
        self.children.contains(&header.session_id)
    }
}

/// When the session started, in epoch milliseconds (0 when its timestamp is unreadable).
fn started_ms(header: &DigestHeader) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&header.started_at)
        .map_or(0, |time| time.timestamp_millis())
}

impl SessionIndex {
    /// Bring `sessions` up to date, newest first, so lineage can be read from their digests.
    async fn sync_many(&self, sessions: &[SessionMetadata]) {
        let mut results = std::pin::pin!(
            futures::stream::iter(sessions.iter().cloned())
                .map(|session| async move {
                    self.sync_session(&session.project_id, &session.session_id)
                        .await
                })
                .buffered(CONCURRENCY)
        );
        while results.next().await.is_some() {}
    }

    /// The conversations among `candidates`, most recently active first: delegated children are
    /// left out. This is what `--last` and top-level resume choose from.
    pub async fn roots(
        &self,
        candidates: Vec<SessionMetadata>,
    ) -> io::Result<Vec<SessionMetadata>> {
        self.sync_many(&candidates).await;
        let index = self.clone();
        let headers = tokio::task::spawn_blocking(move || index.headers())
            .await
            .map_err(io::Error::other)?;
        let lineage = Lineage::of(&headers);
        let by_id: HashMap<&str, &DigestHeader> = headers
            .iter()
            .map(|header| (header.session_id.as_str(), header))
            .collect();
        let mut roots: Vec<(i64, SessionMetadata)> = candidates
            .into_iter()
            .filter(|candidate| {
                !candidate.delegated
                    && by_id
                        .get(candidate.session_id.as_str())
                        .is_none_or(|header| !lineage.is_child(header))
            })
            .map(|candidate| {
                let active = by_id
                    .get(candidate.session_id.as_str())
                    .map_or(0, |header| header.source.mtime_ms);
                (active, candidate)
            })
            .collect();
        // Most recently active first; sessions not indexed yet fall back to when they started.
        roots.sort_by(|(a_active, a), (b_active, b)| {
            b_active
                .cmp(a_active)
                .then_with(|| b.started_at.cmp(&a.started_at))
        });
        Ok(roots.into_iter().map(|(_, session)| session).collect())
    }

    /// `Ok(())` when `session` may be resumed as a conversation; otherwise why it may not. The
    /// sessions of its project are indexed first, since a child is known by its parent's record.
    pub async fn refuse_child(&self, project_id: &str, session_id: &str) -> Result<(), String> {
        let siblings = self
            .transcripts()
            .list_session_metadata(project_id)
            .await
            .map_err(|error| error.to_string())?;
        self.sync_many(&siblings).await;
        let index = self.clone();
        let headers = tokio::task::spawn_blocking(move || index.headers())
            .await
            .map_err(|error| error.to_string())?;
        let lineage = Lineage::of(&headers);
        let Some(header) = headers
            .iter()
            .find(|header| header.session_id == session_id && header.project_id == project_id)
        else {
            return Ok(());
        };
        if !lineage.is_child(header) {
            return Ok(());
        }
        let resume_instead = match lineage.parent_of(header) {
            Some(parent) => format!("Resume its parent session {parent} instead."),
            None => "Resume the session that delegated it instead.".to_owned(),
        };
        Err(format!(
            "Session {session_id} is a delegated child session, not a conversation of its own. {resume_instead}"
        ))
    }
}
