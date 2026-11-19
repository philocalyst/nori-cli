# Tool cell lifecycle

How a tool call becomes a transcript cell, and where it lives on the way. The cell itself is
`ClientToolCell` (`client_tool_cell.rs`); this is about who holds it and when it is committed.

## Where a cell can be

- `active_cell` (`layout_cache::CachedCell`): the one live cell, drawn in the viewport and laid
  out again every frame while it is `animated()` (a call that has not finished). It is a tool
  cell, the user's prompt being echoed, or a run of notices (`NoticeCell`).
- `pending_client_tool_cells`: shell calls, delegations and Nori's own tools that were still
  running when another call took `active_cell`. They are not drawn; a later snapshot for the
  same call id finds them here and, once finished, commits them straight to history.
- History: committed cells (`AppEvent::InsertHistoryCell`), written to native scrollback once.

`completed_client_tool_calls` remembers the call ids already committed this turn, so a late
update for one becomes `UpdateHistoryToolSnapshot` instead of a duplicate cell.

## What a snapshot does (`event_handlers/tools.rs`)

1. The call is already `active_cell`: it is updated in place, and committed when it finishes
   (a run of exploring calls stays open to take more).
2. The call id was already committed: the history cell is updated.
3. The call is in `pending_client_tool_cells`: it is updated there.
4. A read, search or listing while an exploring cell is active: merged into it (the bento
   grid).
5. Otherwise it starts a new cell. The answer being streamed is finished first, so the cell
   lands after the text that came before it. An unfinished shell call, delegation or native
   tool already in `active_cell` moves to `pending_client_tool_cells` instead of being
   committed half done.

## What ends it

- A finished call that is not exploring is committed at once.
- End of turn (`finish_task_presentation`): `pending_client_tool_cells` is dropped and
  `active_cell` is committed with any call still running marked failed
  (`finalize_active_cell_as_failed`), and the committed ids get `FinalizeHistoryTools`.
- A notice (`push_notice`) joins the notice cell if one is active; anything else ends the run.
  With no turn running a notice is committed immediately.

## Disclosure

Every card is collapsed by default (`Disclosure::Collapsed`) and has an expanded form;
`HistoryCell::expansion()` says whether a cell has one and `set_expanded()` flips it. The
transcript view keeps its own set of expanded call ids and asks for `compact_lines` or
`expanded_lines`.
