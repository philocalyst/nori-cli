//! Actions chosen in the actor panel on one subagent: message it, steer it, stop it.
//!
//! The panel itself lives in [`crate::subagent_switcher`]; this module is the app's side of
//! its [`Transition`]s that act on a subagent instead of navigating. They keep the panel open
//! so several actors can be triaged in a row, and each answers with a toast.

use super::*;
use crate::subagent_switcher::Target;
use crate::subagent_switcher::Transition;

impl App {
    pub(super) fn run_actor_action(&mut self, tui: &mut tui::Tui, action: Transition) {
        match action {
            Transition::Message {
                target,
                text,
                steer,
            } => {
                self.app_event_tx.send(AppEvent::SendSubagentMessage {
                    key: target,
                    text,
                    image_paths: Vec::new(),
                    steer,
                });
                let verb = if steer { "Steering" } else { "Messaging" };
                let name = self
                    .subagent_label(target)
                    .unwrap_or_else(|| "the subagent".to_owned());
                tui.show_toast(format!("{verb} {name}"));
            }
            Transition::Cancel(key) => {
                let handle = self.chat_widget.harness_handle();
                let session = self.chat_widget.subagent_session(key);
                let (Some(handle), Some(session_id)) = (handle, session) else {
                    tui.show_toast("This subagent has no session available to stop.".to_owned());
                    return;
                };
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle.cancel_subagent(session_id).await {
                        tx.send(AppEvent::TransientNotice(error.to_string()));
                    }
                });
                tui.show_toast("Stopping its turn".to_owned());
            }
            Transition::Close | Transition::FocusMain | Transition::Focus(_) => {}
        }
        tui.frame_requester().schedule_frame();
    }

    /// Tell the open choice picker, if there is one, something the app just learned.
    pub(super) fn notify_choice(&mut self, notice: &dyn std::any::Any) {
        if let Some(crate::pager_overlay::Overlay::Choice(choice)) = &mut self.overlay {
            choice.notify(notice);
        }
    }

    fn subagent_label(&self, key: crate::subagent_switcher::SubagentKey) -> Option<String> {
        let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
        roster
            .entry(Target::Subagent(key))
            .map(|entry| entry.title.clone())
    }
}
