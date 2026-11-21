//! The sessions a resume picker lists: the transcripts of one scope, each with what the index
//! knows about it. The picker opens with this already in hand, so its rows are right from the
//! first frame: the newest sessions (the ones people resume) are brought up to date first, and
//! sessions nobody asked anything in, and delegated children, which are never conversations of
//! their own, are left out.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::time::Duration;

use futures::StreamExt;

use super::DigestHeader;
use super::SessionIndex;
use crate::transcript::SessionMetadata;

/// How many of the newest sessions are brought up to date before listing.
const FRESHEN: usize = 24;
/// How many transcripts are digested at once.
const CONCURRENCY: usize = 4;

/// One session of a listing: its transcript's metadata and, when indexed, its digest header.
#[derive(Debug, Clone)]
pub struct ListedSession {
    pub meta: SessionMetadata,
    pub header: Option<DigestHeader>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionListing {
    /// Most recently active first.
    pub sessions: Vec<ListedSession>,
    /// Every agent session id any recorded conversation has run in, so an agent's own listing
    /// can be told apart from what is already listed.
    pub known_acp_session_ids: HashSet<String>,
}

impl SessionIndex {
    /// List `candidates` (most recently active first, as the transcript loader returns them).
    /// Digests of the newest are brought up to date first, for at most `budget`; the rest are
    /// listed with whatever digest they have.
    pub async fn listing(
        &self,
        candidates: Vec<SessionMetadata>,
        budget: Duration,
    ) -> io::Result<SessionListing> {
        let newest: Vec<SessionMetadata> = candidates.iter().take(FRESHEN).cloned().collect();
        let index = self.clone();
        let freshen = tokio::spawn(async move {
            futures::stream::iter(newest)
                .map(|session| {
                    let index = index.clone();
                    async move {
                        let _ = index
                            .sync_session(&session.project_id, &session.session_id)
                            .await;
                    }
                })
                .buffered(CONCURRENCY)
                .for_each(|()| async {})
                .await;
        });
        // Out of time: what was indexed so far is used. The rest goes on in the background
        // (the task is detached, not cancelled, so no transcript is read twice) and the picker
        // finishes the other sessions.
        let _ = tokio::time::timeout(budget, freshen).await;

        let index = self.clone();
        let headers = tokio::task::spawn_blocking(move || index.headers())
            .await
            .map_err(io::Error::other)?;
        let known_acp_session_ids = headers
            .iter()
            .flat_map(|header| header.acp_session_ids.iter().cloned())
            .collect();
        let by_id: HashMap<&str, &DigestHeader> = headers
            .iter()
            .map(|header| (header.session_id.as_str(), header))
            .collect();
        let sessions = candidates
            .into_iter()
            .filter_map(|meta| {
                let header = by_id
                    .get(meta.session_id.as_str())
                    .map(|header| (*header).clone());
                let hidden = meta.delegated
                    || header
                        .as_ref()
                        .is_some_and(|header| header.delegated || header.user_turns == 0);
                (!hidden).then_some(ListedSession { meta, header })
            })
            .collect();
        Ok(SessionListing {
            sessions,
            known_acp_session_ids,
        })
    }
}
