//! Pickers and panels for sessions, usage and forks.

use super::*;

impl App {
    pub(super) async fn handle_panels_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::ShowReferenceSessionPicker {
                sessions,
                nori_home,
                target,
            } => {
                let overlay = crate::resume_picker::helix_overlay::ResumeOverlay::new_sessions(
                    crate::resume_picker::helix_overlay::SessionPurpose::Reference { target },
                    sessions,
                    nori_home,
                );
                let _ = tui.set_mouse_capture(true);
                self.overlay = Some(Overlay::Resume(Box::new(overlay)));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::LoadSessionReference {
                project_id,
                session_id,
                agent,
                target,
            } => {
                self.load_session_reference(project_id, session_id, agent, target);
            }
            AppEvent::InsertSessionReference { text, target } => {
                let current = self.session_view.focused();
                if target != current {
                    self.session_view
                        .save_draft(current, self.chat_widget.take_composer_draft());
                    if let Some(draft) = self.session_view.take_draft(target) {
                        self.chat_widget.restore_composer_draft(draft);
                    }
                }
                self.chat_widget.insert_str(&text);
                if target != current {
                    self.session_view
                        .save_draft(target, self.chat_widget.take_composer_draft());
                    if let Some(draft) = self.session_view.take_draft(current) {
                        self.chat_widget.restore_composer_draft(draft);
                    }
                }
            }
            AppEvent::ShowViewonlySessionPicker {
                sessions,
                nori_home,
            } => {
                let overlay = crate::resume_picker::helix_overlay::ResumeOverlay::new_sessions(
                    crate::resume_picker::helix_overlay::SessionPurpose::ViewOnly,
                    sessions.into_iter().map(|info| (info, None)).collect(),
                    nori_home,
                );
                let _ = tui.set_mouse_capture(true);
                self.overlay = Some(Overlay::Resume(Box::new(overlay)));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::LoadViewonlyTranscript {
                nori_home,
                project_id,
                session_id,
            } => {
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let loader = nori_harness::transcript::TranscriptLoader::new(nori_home);
                    match loader.load_transcript(&project_id, &session_id).await {
                        Ok(transcript) => {
                            let entries =
                                crate::viewonly_transcript::transcript_to_entries(&transcript);
                            tx.send(AppEvent::DisplayViewonlyTranscript { entries });
                        }
                        Err(e) => {
                            tx.send(AppEvent::InsertHistoryCell(Box::new(
                                crate::history_cell::new_error_event(format!(
                                    "Failed to load transcript: {e}"
                                )),
                            )));
                        }
                    }
                });
            }
            AppEvent::DisplayViewonlyTranscript { entries } => {
                self.display_viewonly_transcript(entries);
            }
            AppEvent::ShowChoicePicker(spec) => {
                let overlay =
                    crate::choice_picker::ChoiceOverlay::new(*spec, self.app_event_tx.clone());
                let _ = tui.set_mouse_capture(true);
                self.overlay = Some(Overlay::Choice(Box::new(overlay)));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenUsage => {
                let overlay =
                    crate::usage::UsageOverlay::open(&self.config.nori_home, tui.frame_requester());
                let _ = tui.set_mouse_capture(true);
                self.overlay = Some(Overlay::Usage(Box::new(overlay)));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenForkPicker => {
                let messages =
                    crate::app::rewind::collect_all_user_messages(&self.transcript_cells);
                // Only agents that advertise ACP `session/fork` can branch at the
                // current head; without it, the picker only rewinds to a message.
                let supports_fork = self.chat_widget.agent_capabilities().session_fork;
                if !supports_fork && messages.is_empty() {
                    self.chat_widget
                        .add_info_message("No messages to fork from.".to_string(), None);
                } else {
                    let spec = crate::nori::fork_picker::fork_choices(messages, supports_fork);
                    self.chat_widget.show_choice(spec);
                }
                tui.frame_requester().schedule_frame();
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
