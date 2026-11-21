//! Tool calls becoming cells: which cell a snapshot updates, merges into or starts.

use super::*;

impl ChatWidget {
    /// Observes the parent directories of file paths to update the effective CWD tracker.
    /// If the effective CWD changes (after debounce), triggers a system info refresh.
    pub(in crate::chatwidget) fn observe_directories_from_paths<'a>(
        &mut self,
        paths: impl Iterator<Item = &'a std::path::Path>,
    ) {
        for file_path in paths {
            let absolute_path = if file_path.is_absolute() {
                file_path.to_path_buf()
            } else {
                self.config.cwd.join(file_path)
            };

            if self.effective_cwd_tracker.observe_file_path(&absolute_path) {
                let refresh_dir = crate::effective_cwd_tracker::find_git_root(&absolute_path)
                    .or_else(|| {
                        absolute_path
                            .parent()
                            .filter(|p| p.exists())
                            .map(std::path::Path::to_path_buf)
                    });

                if let Some(dir) = refresh_dir {
                    self.app_event_tx
                        .send(AppEvent::RefreshSystemInfoForDirectory { dir });
                }
            }
        }
    }

    /// All ACP tool kinds route through ClientToolCell for native rendering.
    /// ClientToolCell auto-detects exploring tools (Read/Search) and renders
    /// them with "Explored" format, while Execute uses shell-style transcript.
    pub(super) fn handle_client_tool_snapshot(
        &mut self,
        tool_snapshot: crate::presentation::ToolSnapshot,
    ) {
        // NOTE: The answer stream is finalized only on paths that insert a new
        // history cell. No-op updates (e.g., progress notifications for a
        // long-running tool whose cell was already flushed) must not finalize
        // the stream, or one streaming assistant message fragments into many
        // separate `•` cells.
        self.session_stats
            .record_client_tool_snapshot(&tool_snapshot);

        // For completed Create/Edit/Delete/Move, observe directories for footer refreshes.
        if matches!(
            tool_snapshot.kind,
            crate::presentation::ToolKind::Create
                | crate::presentation::ToolKind::Edit
                | crate::presentation::ToolKind::Delete
                | crate::presentation::ToolKind::Move
        ) && tool_snapshot.phase == crate::presentation::ToolPhase::Completed
        {
            self.observe_directories_from_paths(
                tool_snapshot.locations.iter().map(|l| l.path.as_path()),
            );
        }

        // Update existing active ClientToolCell if same call_id
        if let Some(cell) = self
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ClientToolCell>())
            && cell.call_id() == tool_snapshot.call_id
        {
            // The stream cannot be open while a tool cell is active: every
            // path that sets active_cell flushes the stream first, and every
            // answer delta clears active_cell. This lets in-place updates
            // skip the answer-stream flush.
            debug_assert!(self.stream_controller.is_none());
            cell.apply_snapshot(tool_snapshot);
            if !cell.is_active() && !cell.is_exploring() {
                self.flush_active_cell();
            }
            return;
        }

        // If this call_id was already flushed to history (e.g., due to
        // interleaved text streaming), skip creating a duplicate cell.
        if self
            .completed_client_tool_calls
            .contains(&tool_snapshot.call_id)
            || self
                .settled_client_tool_calls
                .contains(&tool_snapshot.call_id)
        {
            self.app_event_tx.send(AppEvent::UpdateHistoryToolSnapshot {
                generation: self.session_generation,
                snapshot: Box::new(tool_snapshot),
            });
            return;
        }

        // Check if this snapshot is for a buffered incomplete execute cell.
        // This allows completions to reach cells that were displaced from
        // active_cell by subsequent tool snapshots (parallel ACP calls).
        if let Some(mut buffered_cell) = self
            .pending_client_tool_cells
            .remove(&tool_snapshot.call_id)
        {
            buffered_cell.apply_snapshot(tool_snapshot);
            if !buffered_cell.is_active() {
                self.flush_answer_stream_with_separator();
                // Insert directly into history without flushing active_cell.
                // The normal add_boxed_history path flushes active_cell first
                // (to maintain chronological order), but that would incorrectly
                // mark the current active Execute cell as completed and discard
                // its later completion event.
                self.completed_client_tool_calls
                    .insert(buffered_cell.call_id().to_owned());
                self.needs_final_message_separator = true;
                self.app_event_tx
                    .send(AppEvent::InsertHistoryCell(Box::new(buffered_cell)));
            } else {
                // Still incomplete — put it back in the buffer.
                let call_id = buffered_cell.call_id().to_owned();
                self.pending_client_tool_cells
                    .insert(call_id, buffered_cell);
            }
            return;
        }

        // Merge into existing exploring cell when possible
        let is_new_exploring = crate::client_event_format::is_exploring_snapshot(&tool_snapshot);
        if is_new_exploring
            && let Some(cell) = self
                .active_cell
                .as_mut()
                .and_then(|c| c.as_any_mut().downcast_mut::<ClientToolCell>())
            && cell.is_exploring()
        {
            // Same invariant as the in-place update above: no open stream
            // while an exploring cell is active, so no flush is needed.
            debug_assert!(self.stream_controller.is_none());
            cell.merge_exploring(tool_snapshot);
            // Don't track in completed_client_tool_calls here — non-terminal
            // snapshots (Pending/InProgress) arrive first with empty invocations,
            // and the real path/query comes in a later tool_call_update. Tracking
            // is deferred to flush_active_cell, which marks all exploring call_ids
            // as completed when the cell leaves active_cell.
            return;
        }

        // A genuinely new tool call starts a new visual cell: finalize the
        // streamed answer so far to keep history chronological.
        self.flush_answer_stream_with_separator();

        // Buffer incomplete Execute ClientToolCells instead of flushing
        // them to history with wrong content (description text as output),
        // incomplete delegations, which run in parallel and would stay
        // "Delegating…" in the scrollback, and Nori's own tools, which agents
        // call in parallel and which read as their result.
        if let Some(active) = self.active_cell.take() {
            if let Some(client_cell) = active.as_any().downcast_ref::<ClientToolCell>()
                && client_cell.is_active()
                && (*client_cell.snapshot_kind() == crate::presentation::ToolKind::Execute
                    || client_cell.is_delegation()
                    || client_cell.is_native())
            {
                let call_id = client_cell.call_id().to_owned();
                if let Ok(boxed) = active.into_any().downcast::<ClientToolCell>() {
                    self.pending_client_tool_cells.insert(call_id, *boxed);
                }
            } else {
                self.active_cell.set(Some(active));
                self.flush_active_cell();
            }
        }
        let should_flush = !matches!(
            tool_snapshot.phase,
            crate::presentation::ToolPhase::Pending
                | crate::presentation::ToolPhase::PendingApproval
                | crate::presentation::ToolPhase::InProgress
        ) && !is_new_exploring;
        let mut cell = ClientToolCell::new(
            tool_snapshot,
            self.config.cwd.clone(),
            self.config.animations,
        );
        if is_new_exploring {
            cell.mark_exploring();
        }
        self.active_cell.set(Some(Box::new(cell)));
        if should_flush {
            self.flush_active_cell();
        }
    }
}
