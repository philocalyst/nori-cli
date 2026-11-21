//! Session-level loads and answers that need no transcript cell.

use super::*;

impl ChatWidget {
    pub(crate) fn on_history_entry_loaded(
        &mut self,
        log_id: i64,
        offset: i64,
        entry: Option<nori_harness::HistoryEntry>,
    ) {
        let (Ok(log_id), Ok(offset)) = (u64::try_from(log_id), usize::try_from(offset)) else {
            return;
        };
        self.bottom_pane
            .on_history_entry_response(log_id, offset, entry.map(|e| e.text));
    }

    pub(in crate::chatwidget) fn on_prompt_summary(&mut self, summary: String) {
        self.bottom_pane.set_prompt_summary(Some(summary));
    }

    /// Mirror the merged session title into the footer and the `/status` card.
    /// This is the part of a session-info update worth showing on every build,
    /// including the stable releases that suppress the metadata dump.
    pub(in crate::chatwidget) fn publish_session_title(&mut self) {
        let title = self
            .session_info_state
            .title()
            .map(|title| {
                crate::nori::session_info::sanitize(
                    title,
                    crate::nori::session_info::MAX_TITLE_DISPLAY_CHARS,
                )
                .trim()
                .to_string()
            })
            .filter(|title| !title.is_empty());
        self.bottom_pane.set_session_title(title);
        // The terminal's tab shows the same title.
        self.refresh_terminal_title();
    }

    pub(crate) fn on_undo_snapshots_loaded(&mut self, snapshots: Vec<nori_harness::UndoSnapshot>) {
        if self.offer_rewind(&snapshots) {
            return;
        }
        if snapshots.is_empty() {
            self.add_info_message("No undo snapshots available.".to_string(), None);
            return;
        }

        let items: Vec<SelectionItem> = snapshots
            .into_iter()
            .map(|snap| {
                let index = snap.index;
                let label = truncate_text(&snap.label, 60);
                let name = format!("[{}] {label}", snap.short_id);
                let tx = self.app_event_tx.clone();
                SelectionItem {
                    name,
                    display_shortcut: None,
                    description: None,
                    selected_description: None,
                    is_current: false,
                    actions: vec![Box::new(move |_| {
                        tx.send(AppEvent::HarnessAction(
                            crate::app_event::HarnessAction::UndoTo(index),
                        ));
                    })],
                    dismiss_on_select: true,
                    search_value: None,
                    is_header: false,
                    menu_tone: nori_tui_components::MenuItemTone::Default,
                }
            })
            .collect();

        self.bottom_pane.show_selection_view(
            SelectionViewParams {
                title: Some("Undo to snapshot".to_string()),
                subtitle: None,
                footer_hint: Some(standard_popup_hint_line()),
                items,
                header: Box::new(()),
                is_searchable: false,
                ..Default::default()
            }
            .picker(),
        );
        self.request_redraw();
    }

    pub(crate) fn on_custom_prompts_loaded(
        &mut self,
        custom_prompts: Vec<nori_harness::CustomPrompt>,
    ) {
        let len = custom_prompts.len();
        tracing::debug!("received {len} custom prompts");
        // Forward to bottom pane so the slash popup can show them now.
        self.bottom_pane.set_custom_prompts(custom_prompts);
    }

    pub(crate) fn on_history_search_loaded(&mut self, entries: Vec<nori_harness::HistoryEntry>) {
        self.bottom_pane.on_search_history_response(entries);
        self.request_redraw();
    }
}
