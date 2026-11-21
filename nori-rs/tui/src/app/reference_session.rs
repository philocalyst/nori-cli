//! Load a selected conversation before adding its reference to the current draft.

use super::App;
use crate::app_event::AppEvent;

impl App {
    pub(super) fn load_session_reference(
        &self,
        project_id: String,
        session_id: String,
        agent: String,
        target: crate::subagent_switcher::Target,
    ) {
        let nori_home = self.config.nori_home.clone();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let index = nori_harness::session_index::SessionIndex::open(&nori_home);
            let loaded = match index.sync_session(&project_id, &session_id).await {
                Ok(_) => {
                    let (project, session) = (project_id.clone(), session_id.clone());
                    tokio::task::spawn_blocking(move || index.digest(&project, &session))
                        .await
                        .ok()
                        .flatten()
                        .ok_or_else(|| "its digest could not be read".to_owned())
                }
                Err(error) => Err(error.to_string()),
            };
            match loaded {
                Ok(digest) => {
                    let text = crate::nori::reference_session_picker::format_reference(
                        &project_id,
                        &session_id,
                        &agent,
                        &digest.turns,
                    );
                    tx.send(AppEvent::InsertSessionReference { text, target });
                }
                Err(error) => {
                    let message = format!("Failed to load session {session_id}: {error}");
                    match target {
                        crate::subagent_switcher::Target::Main => {
                            tx.send(AppEvent::InsertHistoryCell(Box::new(
                                crate::history_cell::new_error_event(message),
                            )))
                        }
                        crate::subagent_switcher::Target::Subagent(key) => {
                            tx.send(AppEvent::ChildSessionOutput {
                                key,
                                text: format!("{message}\n"),
                            })
                        }
                    }
                }
            }
        });
    }
}
