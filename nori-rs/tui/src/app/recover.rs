//! The agent died under a running session (its process exited, its connection broke): restart
//! it and continue the conversation, the way a resume would (see `docs/resilience.md`). An agent
//! that keeps dying is not restarted forever: after three deaths in ten minutes the person is
//! told, and `/resume` continues the conversation when the agent works again.

use std::time::Duration;
use std::time::Instant;

use super::App;
use crate::app_event::AppEvent;

const WINDOW: Duration = Duration::from_secs(600);
const MAX_IN_WINDOW: usize = 3;

impl App {
    pub(super) fn recover_conversation(&mut self) {
        let now = Instant::now();
        self.recoveries
            .retain(|at| now.duration_since(*at) < WINDOW);
        let Some(conversation) = self.chat_widget.conversation_id() else {
            self.chat_widget.add_error_message(
                "The agent stopped unexpectedly. Start a new session or /resume one.".to_owned(),
            );
            return;
        };
        if self.recoveries.len() >= MAX_IN_WINDOW {
            self.chat_widget.add_error_message(format!(
                "The agent stopped unexpectedly again ({MAX_IN_WINDOW} times in ten minutes), so Nori is not restarting it. Nothing is lost: /resume continues this conversation."
            ));
            return;
        }
        self.recoveries.push(now);
        self.chat_widget.add_info_message(
            "The agent stopped unexpectedly. Restarting it and continuing this conversation…"
                .to_owned(),
            None,
        );
        let nori_home = self.config.nori_home.clone();
        let tx = self.app_event_tx.clone();
        let session_id = conversation.to_string();
        tokio::spawn(async move {
            let found = nori_harness::transcript::TranscriptLoader::new(nori_home.clone())
                .find_session_metadata_by_id(&session_id)
                .await;
            match found {
                Ok(Some(meta)) => tx.send(AppEvent::ResumeSession {
                    nori_home,
                    project_id: meta.project_id,
                    session_id,
                }),
                Ok(None) | Err(_) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                    crate::history_cell::new_error_event(
                        "The conversation's record could not be found to continue it.".to_owned(),
                    ),
                ))),
            }
        });
    }
}
