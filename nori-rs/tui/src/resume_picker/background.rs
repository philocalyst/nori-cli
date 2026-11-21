//! The picker's work off the UI thread: keeping the index current, searching it, and loading
//! previews. Everything reports back through one channel; each result asks for a new frame.

use std::collections::HashSet;

use nori_harness::session_index::DigestHeader;
use nori_harness::session_index::SessionHit;
use nori_harness::session_index::SessionIndex;
use nori_harness::session_index::SessionSearch;
use nori_harness::session_index::SyncProgress;
use nori_tui_components::Preview;
use nori_tui_components::PreviewRequest;
use nori_tui_components::Theme;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use super::SessionPickerInfo;
use super::helix_entry::ResumeEntry;
use super::preview;
use crate::tui::FrameRequester;

pub(super) enum Message {
    /// What the index knows about every indexed session, refreshed as indexing proceeds.
    Headers(Vec<DigestHeader>),
    Progress(SyncProgress),
    Hits {
        generation: i64,
        hits: Vec<SessionHit>,
    },
    Preview(Preview<i64>),
    /// The sessions of the scope that was switched to, loaded off the UI thread.
    Rows {
        all_chats: bool,
        sessions: Vec<SessionPickerInfo>,
    },
}

pub(super) struct Search {
    pub(super) generation: i64,
    pub(super) terms: Vec<String>,
    pub(super) scope: HashSet<String>,
}

/// Handles on the picker's background work.
pub(super) struct Background {
    index: SessionIndex,
    tx: UnboundedSender<Message>,
    frames: FrameRequester,
    search: watch::Sender<Option<Search>>,
}

impl Background {
    /// Start keeping the index current and answering searches.
    pub(super) fn start(
        index: SessionIndex,
        first: Vec<String>,
        tx: UnboundedSender<Message>,
        frames: FrameRequester,
    ) -> Self {
        let (search, mut searches) = watch::channel::<Option<Search>>(None);
        let background = Self {
            index: index.clone(),
            tx: tx.clone(),
            frames: frames.clone(),
            search,
        };

        let (sync_index, sync_tx, sync_frames) = (index.clone(), tx.clone(), frames.clone());
        tokio::spawn(async move {
            let headers = sync_index.clone();
            let loaded = tokio::task::spawn_blocking(move || headers.headers()).await;
            if let Ok(headers) = loaded {
                let _ = sync_tx.send(Message::Headers(headers));
                sync_frames.schedule_frame();
            }
            let progress_index = sync_index.clone();
            let progress_tx = sync_tx.clone();
            let progress_frames = sync_frames.clone();
            let mut last_rebuilt = 0;
            let result = sync_index
                .sync_all(&first, move |progress| {
                    let _ = progress_tx.send(Message::Progress(progress));
                    // The first sessions are the newest and the ones people look for: show
                    // them as soon as they are indexed, then in batches.
                    if progress.rebuilt != last_rebuilt
                        && (progress.rebuilt <= 8 || progress.rebuilt % 5 == 0)
                    {
                        last_rebuilt = progress.rebuilt;
                        let index = progress_index.clone();
                        let tx = progress_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = tx.send(Message::Headers(index.headers()));
                        });
                    }
                    progress_frames.schedule_frame();
                })
                .await;
            if let Err(error) = result {
                tracing::warn!(target: "nori_index", %error, "session index sync failed");
            }
            let final_index = sync_index.clone();
            if let Ok(headers) = tokio::task::spawn_blocking(move || final_index.headers()).await {
                let _ = sync_tx.send(Message::Headers(headers));
            }
            sync_frames.schedule_frame();
        });

        tokio::spawn(async move {
            while searches.changed().await.is_ok() {
                let Some(job) = searches.borrow_and_update().as_ref().map(|search| {
                    (
                        search.generation,
                        SessionSearch {
                            terms: search.terms.clone(),
                            scope: Some(search.scope.clone()),
                        },
                    )
                }) else {
                    continue;
                };
                let (generation, query) = job;
                let index = index.clone();
                let Ok(hits) = tokio::task::spawn_blocking(move || index.search(&query)).await
                else {
                    continue;
                };
                let _ = tx.send(Message::Hits { generation, hits });
                frames.schedule_frame();
            }
        });
        background
    }

    /// Replace the pending search with `search`; an older one that has not started never runs.
    pub(super) fn search(&self, search: Search) {
        let _ = self.search.send(Some(search));
    }

    /// Build the preview of `entry` for `request` on a blocking task. A session that is not
    /// indexed yet is indexed now, ahead of the rest: the preview says so meanwhile, and its
    /// row's new header asks for the preview again once it is.
    pub(super) fn load_preview(
        &self,
        request: PreviewRequest<i64>,
        entry: ResumeEntry,
        theme: Theme,
    ) {
        let (index, tx, frames) = (self.index.clone(), self.tx.clone(), self.frames.clone());
        tokio::spawn(async move {
            let ids = entry.project_id.clone().zip(entry.local_session_id.clone());
            let read = index.clone();
            let lookup = ids.clone();
            let digest = tokio::task::spawn_blocking(move || {
                lookup.and_then(|(project, session)| read.digest(&project, &session))
            })
            .await
            .ok()
            .flatten();
            let built = preview::from_digest(&request, &entry, digest.as_ref(), &theme);
            let _ = tx.send(Message::Preview(built));
            frames.schedule_frame();
            let (None, Some((project, session))) = (digest, ids) else {
                return;
            };
            if !matches!(index.sync_session(&project, &session).await, Ok(true)) {
                return;
            }
            let headers = tokio::task::spawn_blocking(move || index.headers())
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|header| header.session_id == session)
                .collect();
            let _ = tx.send(Message::Headers(headers));
            frames.schedule_frame();
        });
    }
}
