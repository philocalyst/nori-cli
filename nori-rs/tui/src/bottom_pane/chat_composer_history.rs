use std::collections::HashMap;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

/// What stepping through the history of earlier sessions found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recall {
    Ready(String),
    /// The entry is being read; it arrives as a history-entry response.
    Pending,
    /// There is nothing further that way.
    Exhausted,
}

/// The prompts of earlier sessions, for walking past the oldest prompt of this conversation
/// (see the composer's `prompt_walk`). Entries are read from the persistent history log on
/// demand, one at a time, and cached.
pub(crate) struct ChatComposerHistory {
    /// Identifier of the history log as reported by `SessionConfiguredEvent`.
    history_log_id: Option<u64>,
    /// Number of entries in the persistent history file when the session started: everything
    /// before this session.
    history_entry_count: usize,
    /// Entries read so far, by offset.
    fetched_history: HashMap<usize, String>,
    /// The entry being shown, while walking. `None` when not walking the history.
    history_cursor: Option<usize>,
}

impl ChatComposerHistory {
    pub fn new() -> Self {
        Self {
            history_log_id: None,
            history_entry_count: 0,
            fetched_history: HashMap::new(),
            history_cursor: None,
        }
    }

    /// Update metadata when a new session is configured.
    pub fn set_metadata(&mut self, log_id: u64, entry_count: usize) {
        self.history_log_id = Some(log_id);
        self.history_entry_count = entry_count;
        self.fetched_history.clear();
        self.history_cursor = None;
    }

    /// Reset navigation tracking so the next walk starts from the newest entry.
    pub fn reset_navigation(&mut self) {
        self.history_cursor = None;
    }

    /// `↑` past the oldest prompt of the conversation: the entries of earlier sessions, newest
    /// first.
    pub fn navigate_up_before_session(&mut self, app_event_tx: &AppEventSender) -> Recall {
        let next = match self.history_cursor {
            None if self.history_entry_count > 0 => self.history_entry_count - 1,
            None | Some(0) => return Recall::Exhausted,
            Some(idx) => idx - 1,
        };
        self.history_cursor = Some(next);
        self.recall(next, app_event_tx)
    }

    /// `↓` while walking the entries of earlier sessions; [`Recall::Exhausted`] past the newest
    /// of them.
    pub fn navigate_down_before_session(&mut self, app_event_tx: &AppEventSender) -> Recall {
        let next = match self.history_cursor {
            Some(idx) if idx + 1 < self.history_entry_count => idx + 1,
            None | Some(_) => {
                self.reset_navigation();
                return Recall::Exhausted;
            }
        };
        self.history_cursor = Some(next);
        self.recall(next, app_event_tx)
    }

    /// Integrate a GetHistoryEntryResponse event: the entry's text when it is the one being
    /// shown.
    pub fn on_entry_response(
        &mut self,
        log_id: u64,
        offset: usize,
        entry: Option<String>,
    ) -> Option<String> {
        if self.history_log_id != Some(log_id) {
            return None;
        }
        let text = entry?;
        self.fetched_history.insert(offset, text.clone());
        (self.history_cursor == Some(offset)).then_some(text)
    }

    /// The entry at `offset` if it has been read, else ask for it.
    fn recall(&mut self, offset: usize, app_event_tx: &AppEventSender) -> Recall {
        if let Some(text) = self.fetched_history.get(&offset) {
            return Recall::Ready(text.clone());
        }
        if let Some(log_id) = self.history_log_id
            && let (Ok(log_id), Ok(offset)) = (i64::try_from(log_id), i64::try_from(offset))
        {
            app_event_tx.send(AppEvent::HarnessAction(
                crate::app_event::HarnessAction::HistoryEntry { log_id, offset },
            ));
        }
        Recall::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn walks_earlier_sessions_newest_first_reading_entries_on_demand() {
        let (tx, mut rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx);
        let mut history = ChatComposerHistory::new();
        history.set_metadata(1, 3);

        assert_eq!(history.navigate_up_before_session(&tx), Recall::Pending);
        let Ok(AppEvent::HarnessAction(crate::app_event::HarnessAction::HistoryEntry {
            log_id: 1,
            offset: 2,
        })) = rx.try_recv()
        else {
            panic!("expected a request for the newest entry");
        };
        assert_eq!(
            history.on_entry_response(1, 2, Some("latest".into())),
            Some("latest".to_owned())
        );

        assert_eq!(history.navigate_up_before_session(&tx), Recall::Pending);
        assert_eq!(
            history.on_entry_response(1, 1, Some("older".into())),
            Some("older".to_owned())
        );
        assert_eq!(
            history.navigate_down_before_session(&tx),
            Recall::Ready("latest".into())
        );
        assert_eq!(history.navigate_down_before_session(&tx), Recall::Exhausted);
    }

    #[test]
    fn nothing_before_the_session_is_exhausted_at_once() {
        let (tx, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx);
        let mut history = ChatComposerHistory::new();
        assert_eq!(history.navigate_up_before_session(&tx), Recall::Exhausted);
    }
}
