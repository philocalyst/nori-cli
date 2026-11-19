//! Commits, file search, system info and harness plumbing.

use super::*;

impl App {
    pub(super) async fn handle_workspace_event(
        &mut self,
        _tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::CommitChanges { automatic } => {
                self.request_commit(automatic);
            }
            AppEvent::CommitChangesReady { automatic, changes } => {
                self.submit_commit_request(automatic, changes);
            }
            AppEvent::HarnessAction(action) => {
                if let crate::app_event::HarnessAction::RespondToAgent { request_id, .. } = &action
                {
                    self.program_status.answered(request_id);
                }
                self.chat_widget.submit_harness_action(action);
            }
            AppEvent::HistoryEntryLoaded {
                log_id,
                offset,
                entry,
            } => self
                .chat_widget
                .on_history_entry_loaded(log_id, offset, entry),
            AppEvent::HistorySearchLoaded(entries) => {
                self.chat_widget.on_history_search_loaded(entries);
            }
            AppEvent::CustomPromptsLoaded(prompts) => {
                self.chat_widget.on_custom_prompts_loaded(prompts);
            }
            AppEvent::UndoSnapshotsLoaded(snapshots) => {
                self.chat_widget.on_undo_snapshots_loaded(snapshots);
            }
            AppEvent::GoalLoaded(Some(goal)) => self.chat_widget.handle_thread_goal_updated(goal),
            AppEvent::GoalLoaded(None) => self.chat_widget.handle_thread_goal_cleared(),
            AppEvent::HarnessActionFailed(message) => self.chat_widget.add_error_message(message),
            AppEvent::PromptSubmitFailed(message) => {
                self.chat_widget.on_prompt_submit_failed(message);
            }
            AppEvent::StartFileSearch(query) => {
                if !query.is_empty() {
                    self.file_search.on_user_query(query);
                }
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.chat_widget.apply_file_search_result(query, matches);
            }
            AppEvent::SystemInfoRefreshed(info) => {
                self.chat_widget.apply_system_info_refresh(info);
            }
            AppEvent::RefreshSystemInfoForDirectory { dir, agent } => {
                self.request_system_info_refresh(dir, agent, self.chat_widget.first_prompt_text());
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
