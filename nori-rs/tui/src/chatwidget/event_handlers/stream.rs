//! The model's words: streaming answers, reasoning, and the line between them and Nori's own
//! notices.

use super::*;

impl ChatWidget {
    pub(in crate::chatwidget) fn flush_answer_stream_with_separator(&mut self) -> bool {
        self.stream_ending = false;
        self.live_tail.set(None);
        self.active_answer_message_id = None;
        let mut consolidated = false;
        if let Some(mut controller) = self.stream_controller.take() {
            let (cell, source) = controller.finalize();
            if let Some(cell) = cell {
                self.add_boxed_history(cell);
            }
            if let Some(source) = source {
                self.app_event_tx.send(AppEvent::ConsolidateAgentMessage {
                    source,
                    cwd: self.config.cwd.clone(),
                });
                consolidated = true;
            }
        }
        // The message is whole: what arrived while it streamed goes after it.
        self.commit_deferred_cells();
        consolidated
    }

    pub(in crate::chatwidget) fn on_agent_message_delta(&mut self, delta: String) {
        self.handle_streaming_delta(delta);
    }

    pub(in crate::chatwidget) fn on_agent_reasoning_delta(&mut self, delta: String) {
        self.flush_answer_stream_with_separator();

        // For reasoning deltas, do not stream to history. Accumulate the
        // current reasoning block and extract the first bold element
        // (between **/**) as the chunk header. Show this header as status.
        self.reasoning_buffer.push_str(&delta);

        if let Some(header) = extract_first_bold(&self.reasoning_buffer) {
            // Update the shimmer header to the extracted reasoning chunk header.
            self.set_status_header(header);
        } else {
            // Fallback while we don't yet have a bold header: leave existing header as-is.
        }
        self.request_redraw();
    }

    #[cfg(test)]
    pub(crate) fn has_active_agent_stream(&self) -> bool {
        self.stream_controller.is_some()
    }

    #[inline]
    pub(in crate::chatwidget) fn handle_streaming_delta(&mut self, delta: String) {
        // Text after the end of a turn is not part of its answer.
        if self.stream_ending {
            self.flush_answer_stream_with_separator();
        }
        // Always flush the active cell before streaming agent text. This ensures
        // tool cells appear in the correct chronological position (before the text
        // that follows them), even when tool calls haven't completed yet.
        self.flush_active_cell();

        if self.stream_controller.is_none() {
            if self.needs_final_message_separator {
                // A replayed turn is not running; its update says how long it ran.
                let elapsed_seconds = self
                    .bottom_pane
                    .status_widget()
                    .map(crate::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds)
                    .or(self.replay_turn_elapsed.map(|elapsed| elapsed.as_secs()));
                // A turn that took a few seconds needs no rule: the status line says so.
                if history_cell::FinalMessageSeparator::marks(elapsed_seconds) {
                    self.add_to_history(history_cell::FinalMessageSeparator::new(elapsed_seconds));
                }
                self.needs_final_message_separator = false;
            }
            self.stream_controller = Some(StreamController::new(
                self.last_rendered_width.get().map(|w| w.saturating_sub(2)),
            ));
        }
        if let Some(controller) = self.stream_controller.as_mut() {
            controller.push(&delta);
        }
        self.request_redraw();
    }

    pub(super) fn handle_client_message_delta(
        &mut self,
        message_delta: crate::presentation::MessageDelta,
    ) {
        match message_delta.stream {
            crate::presentation::MessageStream::User => {
                if message_delta.delta.is_empty() {
                    return;
                }

                let continues_active_message =
                    self.active_user_message_id == message_delta.message_id
                        && self.active_cell.as_ref().is_some_and(|cell| {
                            cell.as_any().is::<history_cell::UserHistoryCell>()
                        });
                if continues_active_message {
                    if let Some(cell) = self.active_cell.as_mut().and_then(|cell| {
                        cell.as_any_mut()
                            .downcast_mut::<history_cell::UserHistoryCell>()
                    }) {
                        cell.message.push_str(&message_delta.delta);
                    }
                } else {
                    self.flush_active_cell();
                    if self.replay_source.is_none() {
                        self.session_stats.record_user_message();
                    }
                    self.active_user_message_id = message_delta.message_id;
                    self.active_cell
                        .set(Some(Box::new(history_cell::new_user_prompt(
                            message_delta.delta,
                        ))));
                }
                self.needs_final_message_separator = false;
                self.request_redraw();
            }
            crate::presentation::MessageStream::Answer => {
                self.assistant_stream_seen_for_stats = true;
                let changed_message = self
                    .active_answer_message_id
                    .as_deref()
                    .zip(message_delta.message_id.as_deref())
                    .is_some_and(|(active, incoming)| active != incoming);
                if changed_message {
                    self.flush_answer_stream_with_separator();
                }
                if message_delta.message_id.is_some() {
                    self.active_answer_message_id = message_delta.message_id.clone();
                }
                self.on_answer_text(&message_delta.delta);
            }
            crate::presentation::MessageStream::Reasoning => {
                self.on_agent_reasoning_delta(message_delta.delta);
            }
        }
    }

    /// Answer text from the agent. A notice the adapter carried in the text stream (marked by
    /// [`nori_protocol::AGENT_WARNING_PREFIX`]) ends the prose before it and is shown as Nori's
    /// own, never as part of what the model said.
    pub(in crate::chatwidget) fn on_answer_text(&mut self, text: &str) {
        let Some((prose, notice)) = text.split_once(nori_protocol::AGENT_WARNING_PREFIX) else {
            self.on_agent_message_delta(text.to_owned());
            return;
        };
        if !prose.is_empty() {
            self.on_agent_message_delta(prose.to_owned());
        }
        self.flush_answer_stream_with_separator();
        self.on_warning(notice.trim_end().to_owned());
    }
}
