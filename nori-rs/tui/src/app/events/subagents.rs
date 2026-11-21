//! Subagents: their journals, controls and messages.

use super::*;

impl App {
    pub(super) async fn handle_subagents_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::ChildHistoryReady {
                request_id,
                key,
                focus_responses,
                result,
            } => {
                let keys = if focus_responses {
                    let Some(pending) = self.pending_child_focus.take() else {
                        return Ok(true);
                    };
                    if pending.request_id != request_id {
                        self.pending_child_focus = Some(pending);
                        return Ok(true);
                    }
                    if pending.key != key
                        || self.session_view.focused()
                            != crate::subagent_switcher::Target::Subagent(key)
                        || pending.generation != self.chat_widget.session_generation()
                        || self.chat_widget.subagent_session(key).as_ref()
                            != Some(&pending.session_id)
                    {
                        return Ok(true);
                    }
                    pending.keys
                } else {
                    Vec::new()
                };
                match result {
                    Ok(events) => {
                        self.show_child_history(tui, key, focus_responses, events);
                        for key in keys {
                            if !self.handle_focus_key(tui, key) {
                                break;
                            }
                        }
                    }
                    Err(error) => self.show_child_history_error(tui, key, error),
                }
            }
            AppEvent::ChildHistoryActionFinished {
                key,
                rewound,
                result,
            } => match result {
                Ok(events) if rewound => {
                    self.chat_widget.replace_subagent_history(key, &events);
                    tui.frame_requester().schedule_frame();
                }
                Ok(_) => {}
                Err(error) => self.show_child_history_error(tui, key, error),
            },
            AppEvent::SubagentControlsReady {
                key,
                revision,
                handle,
                config_options,
                error,
            } => {
                if self.session_view.controls_revision(key) != revision {
                    return Ok(true);
                }
                if let Some(error) = error {
                    tracing::warn!(%error, "failed to load child session controls");
                    return Ok(true);
                }
                self.session_view.set_controls(key, handle, &config_options);
                if self.session_view.focused() == crate::subagent_switcher::Target::Subagent(key) {
                    let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                    let name = roster
                        .entry(crate::subagent_switcher::Target::Subagent(key))
                        .and_then(|entry| entry.agent.as_deref());
                    self.chat_widget.set_focused_session_config(
                        Some(
                            self.session_view
                                .controlled_config(key)
                                .cloned()
                                .unwrap_or_default(),
                        ),
                        name,
                    );
                }
            }
            AppEvent::SubagentConfigSetResult {
                key,
                result,
                config_id,
                value,
                route_fallback_eligible,
                route_token,
            } => {
                if self.stale_model_route_token(route_token) {
                    return Ok(true);
                }
                if self.is_model_child_route(route_token, key, &config_id, &value) {
                    if let Err(why) = &result
                        && route_fallback_eligible
                        && self.advance_model_route(why)
                    {
                        return Ok(true);
                    }
                    self.model_route_fallback = None;
                }
                let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                let crate::subagent_switcher::Target::Subagent(key) =
                    roster.resolve(crate::subagent_switcher::Target::Subagent(key))
                else {
                    return Ok(true);
                };
                match result {
                    Ok(config_options) => {
                        let handle = self.session_view.controlled_handle(key);
                        self.session_view.set_controls(key, handle, &config_options);
                        if self.session_view.focused()
                            == crate::subagent_switcher::Target::Subagent(key)
                        {
                            let roster =
                                self.chat_widget.subagent_roster(std::time::Instant::now());
                            let name = roster
                                .entry(crate::subagent_switcher::Target::Subagent(key))
                                .and_then(|entry| entry.agent.as_deref());
                            self.chat_widget.set_focused_session_config(
                                self.session_view.controlled_config(key).cloned(),
                                name,
                            );
                        }
                    }
                    Err(error) => tui.show_toast(error),
                }
            }
            AppEvent::ContextTimelineReady {
                generation,
                conversation_id,
                result,
            } => {
                if conversation_id.is_none()
                    && (generation != self.chat_widget.session_generation()
                        || self.chat_widget.conversation_id().is_some())
                {
                    // The initial, sessionless widget may be replaced while the local
                    // inventory is being read. Reissue against the activated session.
                    self.chat_widget.add_context_output();
                } else {
                    self.chat_widget
                        .handle_context_timeline(generation, conversation_id, result);
                }
            }
            AppEvent::SendSubagentMessage {
                key,
                text,
                image_paths,
                steer,
            } => {
                if let Some(session_id) = self.chat_widget.subagent_session(key) {
                    self.chat_widget.submit_harness_action(
                        crate::app_event::HarnessAction::SendSubagentMessage {
                            key,
                            session_id,
                            text,
                            image_paths,
                            steer,
                        },
                    );
                } else {
                    self.restore_subagent_draft(tui, key, &text, image_paths);
                    tui.show_toast(
                        "This subagent has no session available for follow-up.".to_owned(),
                    );
                }
            }
            AppEvent::SubagentMessageFailed {
                key,
                text,
                image_paths,
                error,
            } => {
                self.restore_subagent_draft(tui, key, &text, image_paths);
                tui.show_toast(error);
            }
            AppEvent::ChildSessionOutput { key, text } => {
                let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                let crate::subagent_switcher::Target::Subagent(key) =
                    roster.resolve(crate::subagent_switcher::Target::Subagent(key))
                else {
                    unreachable!("child target remains a child")
                };
                self.session_view.append_child_output(key, &text);
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ForkResponseSubagent { context, source } => {
                self.chat_widget.submit_harness_action(
                    crate::app_event::HarnessAction::ForkHistory { context, source },
                );
            }
            AppEvent::BranchFromCurrent => {
                self.chat_widget
                    .submit_harness_action(crate::app_event::HarnessAction::Branch);
            }
            AppEvent::OpenActorPanel => self.open_actor_panel(tui),
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
