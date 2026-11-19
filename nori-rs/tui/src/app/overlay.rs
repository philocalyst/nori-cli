//! The overlays that take the whole region: the transcript pager and the resume picker. Events
//! go to the overlay until it is done; what it chose is carried out once the event that chose it
//! has been handled.

use color_eyre::eyre::Result;

use super::App;
use crate::app_event::AppEvent;
use crate::pager_overlay::Overlay;
use crate::resume_picker::helix_overlay::ResumeAction;
use crate::resume_picker::helix_overlay::ResumeOutcome;
use crate::tui::Tui;
use crate::tui::TuiEvent;

impl App {
    pub(super) fn handle_overlay_event(&mut self, tui: &mut Tui, event: TuiEvent) -> Result<()> {
        if let Some(Overlay::Resume(resume)) = &mut self.overlay {
            resume.handle_event(tui, event)?;
            match resume.take_outcome() {
                ResumeOutcome::Pending => {}
                ResumeOutcome::Cancelled { cancel_candidate } => {
                    self.close_transcript_overlay(tui);
                    if cancel_candidate {
                        self.app_event_tx.send(AppEvent::CancelAgentCandidate);
                    }
                }
                ResumeOutcome::Selected(action) => {
                    self.close_transcript_overlay(tui);
                    self.app_event_tx.send(match action {
                        ResumeAction::StartFresh => AppEvent::NewSession,
                        ResumeAction::Local {
                            nori_home,
                            project_id,
                            session_id,
                        } => AppEvent::ResumeSession {
                            nori_home,
                            project_id,
                            session_id,
                        },
                        ResumeAction::Acp {
                            acp_session_id,
                            title,
                        } => AppEvent::ResumeAcpSession {
                            acp_session_id,
                            title,
                        },
                        ResumeAction::ViewOnly {
                            nori_home,
                            project_id,
                            session_id,
                        } => AppEvent::LoadViewonlyTranscript {
                            nori_home,
                            project_id,
                            session_id,
                        },
                        ResumeAction::Reference {
                            project_id,
                            session_id,
                            agent,
                            target,
                        } => AppEvent::LoadSessionReference {
                            project_id,
                            session_id,
                            agent,
                            target,
                        },
                    });
                }
            }
            tui.frame_requester().schedule_frame();
            return Ok(());
        }

        if let Some(overlay) = &mut self.overlay {
            overlay.handle_event(tui, event)?;
            if overlay.is_done() {
                self.close_transcript_overlay(tui);
            }
        }
        let chosen = match &mut self.overlay {
            Some(Overlay::Transcript(transcript)) => transcript.take_chosen(),
            Some(
                Overlay::Resume(_)
                | Overlay::Static(_)
                | Overlay::Subagents(_)
                | Overlay::Usage(_)
                | Overlay::Choice(_),
            )
            | None => None,
        };
        if let Some(action) = chosen {
            self.on_message_action(tui, action);
        }
        Ok(())
    }

    pub(crate) fn open_transcript_overlay(&mut self, tui: &mut Tui) {
        // Clicks select messages and the wheel scrolls while it is open.
        let _ = tui.set_mouse_capture(true);
        self.overlay = Some(Overlay::new_transcript(self.transcript_cells.clone()));
        tui.frame_requester().schedule_frame();
    }

    /// Close the overlay and draw the conversation again.
    pub(crate) fn close_transcript_overlay(&mut self, tui: &mut Tui) {
        if self.overlay.take().is_none() {
            return;
        }
        let _ = tui.set_mouse_capture(false);
        tui.request_full_repaint();
    }
}
