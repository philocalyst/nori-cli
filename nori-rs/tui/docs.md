# Noridoc: nori-tui

Path: @/nori-rs/tui

### Overview

- `nori-tui` is the Ratatui frontend for the headless Nori harness. It owns
  input, rendering, presentation-only state, pickers, approvals, and terminal
  lifecycle.
- The crate adapts terminal events and shared component outcomes into
  application events. It owns runtime policy and lifecycle for the optional
  remote ACP surface, while WebSocket/ACP wire mechanics remain in
  `nori-acp-host`; it does not expose a second protocol vocabulary.

### How it fits into the larger codebase

`/context` queries the Harness locally and writes a responsive, unboxed
snapshot into history without prompting or interrupting the agent. The
[`ChatWidget adapter`](src/chatwidget/context.rs) rejects asynchronous results
from a replaced session generation or conversation. The
[`context view`](src/context_view.rs) renders the Harness's detached
[timeline](../harness/src/context_timeline/docs.md), so printed snapshots do
not change as later updates arrive.

The main shared [`BlockMeter`](../tui-components/src/block_meter.rs) alternates
`▦` and `▤` across observed-growth segments and highlights the latest turn's
segment in green. A compact `Current` row combines baseline and turn sizes;
these segments describe growth observed during a turn, not which tokens from
that turn remain in context. Reset state appears in that row, and the checkpoint
section carries a brief reminder that start readings use the latest prior
observation. Shrink, compaction, a capacity change, or a
replacement context starts a new baseline. The summary includes the current
turn and comparable observed change; the checkpoint section lists immutable
start/end readings, running turns, missing readings, and carried readings.
Wide terminals place the summary beside the map and use checkpoint columns;
narrow terminals stack these sections or show numbers without the map.

Agent-reported ACP observations supply the timeline and take precedence for
its usage summary. When no timeline evidence or reset is available, the
existing latest-usage summary can fall back to the provider transcript's latest
context count and estimated window capacity. Cumulative billing totals never
stand in for context occupancy, and fallback estimates never create turn
segments. Missing usage is explicitly unknown; reported zero is valid. Partial
history labels distinguish known checkpoints from the broader displayed turn
count, and no delta is inferred across a context reset or an unchanged reading
identity.

The local instruction inventory follows the active agent's discovery rules at
the effective working directory. Its token counts are estimates of local file
sizes, not a category allocation or proof that those files remain in context
after compaction. Cloud sessions never display the client machine's files as
remote instructions. Observed skills and subagents are session activity only.
The view does not infer category allocations or automatic compaction thresholds.

Run `cargo run -p nori-tui --features storybook --example context_storybook`
for a production-rendered specimen with fixture usage, including zero and
unknown states. Press `u` to cycle usage and resize the terminal to inspect
the responsive layout. `--json` exports styled spans for browser review.

```text
nori-tui
   |  typed HarnessHandle methods
   |  SessionEvent::{Acp, Nori}
   v
nori-harness -> nori-acp-host -> ACP agent
   ^
   |
nori-config
```

The TUI depends on `nori-harness`, `nori-config`, and `nori-protocol`; it does
not import the ACP host or ACP schema crate directly. Its ACP types arrive
through `nori_protocol::acp`.

Shared pickers and overlay menus receive
[`nori-tui-components`](../tui-components/docs.md) styles through
[`component_theme`](src/style.rs). That adapter supplies the terminal's
reported RGB background only when stdout supports true color, so neutral
surfaces remain terminal-relative while the component library owns the green
pointer, cyan information, foreground title, and neutral selected-copy grammar.
Bottom-pane selection adapters, launch-time pickers, and full-screen action
prompts use this path; raw key routing and application actions remain owned by
the TUI.

At launch, the TUI supplies the harness with two Nori CLI context-envelope
variants. Both identify the first prompt as coming from Nori CLI; the
non-HTTP-MCP variant also explains the ACP fallback and unavailable MCP-backed
affordances. The harness chooses between them from the connected agent's
reported HTTP MCP capability and injects the selected envelope once. The CLI
does not add a user identity because authenticated Nori Sessions identity is
owned outside this layer. When that first prompt carries active goal context,
the HTTP-MCP envelope also states that Nori CLI owns the goal and routes reads
and updates to `get_goal` and `update_goal` from the `nori-client` MCP server,
never to similarly named native agent tools.

### Core Implementation

#### Context inspection and profiling

The [context adapter](src/chatwidget/context.rs) captures the active agent and
working directory, then collects the complete instruction inventory in one
`spawn_blocking` task alongside the asynchronous Harness snapshot query. File
discovery, reads, and token encoding stay outside the TUI event loop. Both
results join into a completion event carrying the initiating session
generation and conversation identity; stale results cannot insert a snapshot
into a replacement session. A worker failure is returned as a local command
error rather than an empty inventory.

[`token_count.rs`](src/nori/token_count.rs) shares a process-wide `OnceLock`
containing the `o200k_base` tokenizer initialization result. This avoids rebuilding
the vocabulary for every instruction file. Initialization failure is cached as
well and retains the heuristic fallback. File contents and counts are not
cached: every inventory request reads current files and encodes them again, so
instruction edits appear on the next `/context` request. Existing provider
approximation flags are preserved.

Run the built CLI with `RUST_LOG=nori_context_profile=debug` to record stage
timings in the configured Nori home's rolling `log/nori-acp.YYYY-MM-DD` file.
The CLI initializes that subscriber before the TUI, so `nori-tui.log` can remain
empty. These debug events separate Harness lock wait and snapshot cloning from
tokenizer setup, encoding, inventory collection, rendering, and history queueing.
`context_ready.elapsed_us` covers dispatch through background completion;
`context_history_queued` measures only insertion work, including its `total_us`
field. Rendering is timed separately. Events record durations and sizes, not
instruction text or ACP metadata. A live query clones existing Harness state;
it does not load JSONL or reconstruct checkpoint history.

The ignored [inventory profile](src/nori/session_header/profile.rs) measures
cold and repeated real-file discovery, then verifies an edited file produces a
fresh count. Run it in a fresh process from `nori-rs`, with an explicit output
directory so artifacts survive fixture cleanup:

```sh
NORI_CONTEXT_PROFILE_DIR="$(pwd)/../.local/context-profile" cargo test -p nori-tui profile_context_instruction_inventory -- --ignored --nocapture --test-threads=1
```

The [Harness profiling guide](../harness/src/context_timeline/docs.md) measures
JSONL load, checkpoint reduction, and live snapshot queries independently.
[PTY coverage](../tui-pty-e2e/tests/context_instructions.rs) exercises repeated
`/context` commands against real instruction files and checks changed counts
within the warm interactive latency budget.

#### Prompt-bound authenticated analytics

[`run_main`](src/lib.rs) receives an optional
[`AnalyticsReporter`](../installed/docs.md) from the binary and carries one
clone through [`App`](src/app/mod.rs) and each [`ChatWidgetInit`](src/chatwidget/mod.rs).
Every new or resumed agent path in [`chatwidget/agent.rs`](src/chatwidget/agent.rs)
attaches that reporter to the exact launched harness handle before exposing the
handle to the widget. Agent replacement and `/new` therefore create a fresh
logical-session boundary while cloned handles for one session share one
first-prompt guard.

The ordinary TUI uses `session_mode = interactive`; the explicit cloud route
uses `session_mode = cloud`. Preparation, session listing, picker display,
resume, and connection establishment do not emit authenticated activity;
visible connection feedback is independent of analytics. Capture begins in
the [harness](../harness/docs.md) only when the first user prompt receives its
ACP wire request ID. Leaving the application performs one bounded reporter
flush after [`App::run`](src/app/mod.rs) completes and before terminal
restoration; analytics failures do not replace the application result.

#### Source-first event dispatch

The application event loop matches `SessionEvent::Acp` and
`SessionEvent::Nori` before projecting either branch into UI state.

- ACP notifications drive private message, thought, plan, tool, usage, mode,
  live config, capability, and available-command presentation.
- User input is rendered from the harness's canonical ACP
  `user_message_chunk` notifications, not inserted by the submitting widget.
  Chunks with one message id are accumulated into one user history cell, and
  non-text image, audio, resource-link, and embedded-resource blocks receive
  attachment placeholders. The submitter and observing frontends therefore
  follow the same event path.
- Request-scoped updates belong to the active local prompt or load when one
  exists; otherwise the TUI preserves and renders each update as unowned
  activity without dropping or invented attribution, regardless of source or
  transport. The harness also emits one warning for the unowned burst without
  changing that presentation. Any unowned user, agent, thought, plan,
  tool-call, or tool-update content starts or confirms unowned
  presentation. Without a status hint, a later locally owned prompt or load
  start flushes and separates that output without inventing completion.
- The chat widget tracks local prompt ownership as
  `owned_prompt_request_id` and keeps a separate private
  `proactive_turn_active` presentation bit. The legacy identifier groups
  unowned presentation or presentation of an agent-owned turn, plus statistics
  and explicit-idle notifications. It does not set the bottom pane's
  operational task-running flag, so Ctrl-C cancellation, interrupt hints, and
  in-task command gating remain tied to locally owned requests. It never drives
  queue, cancel, loop, ACP phase, or request state.
- Ordinary metadata does not imply a turn. The Nori Sessions broker's optional
  `_meta.nori.status` extension establishes an agent-owned turn only when no
  locally owned prompt or load is active: exact `working` starts or confirms
  the turn and exact `idle` ends it. The status affects presentation only;
  unknown values remain ordinary session metadata. Known status-only frames
  are hidden; a known status combined with `title` or `updated_at` still
  displays those fields. This distinction lives in
  [`event_handlers.rs`](src/chatwidget/event_handlers.rs) and
  [`presentation/mod.rs`](src/presentation/mod.rs).
- Handroll's `_meta.nori.connection.status` extension is also presentation
  only. Exact `reconnecting` and `connected` status-only frames render as
  “Cloud connection lost. Reconnecting…” and “Cloud connection restored.”
  info entries instead of generic redacted metadata. They do not change turn
  ownership, replay ACP methods, or enter the session-info state reducer.
- An initialize response with exact `_meta.nori.attachmentMode = "follow"`
  marks the live attachment as follow-only. The chat widget keeps typing
  enabled, but ordinary submission restores the text to the composer without
  draining attachments or emitting a prompt or session action. Slash-command
  dispatch remains active so `/providers`, `/new`, and `/resume` can leave the
  attachment; local input handlers also run before a centralized user-message
  guard suppresses every path that would submit an ACP prompt, including
  prompt-generating commands and programmatic callers. Every later initialize
  response recomputes this policy, so a missing, malformed, or different value
  restores normal submission. This boundary lives in
  [`event_handlers.rs`](src/chatwidget/event_handlers.rs) and
  [`key_handling.rs`](src/chatwidget/key_handling.rs), with the ACP-bound guard
  in [`user_input.rs`](src/chatwidget/user_input.rs).
- ACP requests drive the permission overlay and retain their raw `RequestId`.
- Initialize responses and prompt responses matching the active request update
  UI lifecycle. For prompt errors, the correlated `NoriEvent::RequestFailed`
  drives completion, loop disposition, and user-visible display; the matching
  raw ACP error remains observable but is not rendered or completed a second
  time. Failures unrelated to the active prompt do not complete it. Other typed
  operations complete through their
  `HarnessHandle` return values while their raw responses remain observable on
  the stream. When an unpaired ACP error reaches the active widget,
  [`event_handlers.rs`](src/chatwidget/event_handlers.rs) logs its code and safe
  diagnostic fields, then renders its message plus a distinct, non-empty string
  `data.detail` when present. Other machine-readable error data stays out of
  history, keeping the user-facing failure actionable and screenshot-friendly
  without dumping opaque metadata.
- Nori events drive lifecycle, queue, replay, compaction, goals, undo,
  user-shell output, hooks, summaries, notices, and classified failures.

The private modules under `tui/src/presentation/` assemble ACP streaming values
into display cells and friendly labels. These view models are allowed to be
lossy and UI-specific; they are not fed back into the harness or exported from
`nori-protocol`.

#### Picker and overlay-menu presentation

Searchable pickers use two explicit interaction states instead of interpreting
every printable key as a query. They open in navigation state, where arrows and
`j`/`k` move the cursor. `f`, `/`, or Ctrl-F activates search without inserting
the activation key; active search accepts printable characters, including the
navigation and activation characters. Escape first clears and exits search,
then dismisses the picker if pressed again. The input row is present only while
search is active. Inactive footers intentionally show only the compact
`/ search` affordance; `f` and Ctrl-F remain supported aliases rather than
additional visible hint text. Active footers describe typing and search exit.

- [`BottomPane`](src/bottom_pane/mod.rs) routes Escape through the active
  [`BottomPaneView::on_escape`](src/bottom_pane/bottom_pane_view.rs) hook before
  ordinary key handling. The hook defaults to the existing Ctrl-C cancellation
  behavior, while searchable component and generic selection views override it
  to consume the first Escape without completing the view. This keeps
  multi-stage Escape state transitions independent from Ctrl-C cancellation;
  the next Escape follows the view's normal dismissal path.
- [`SelectionViewParams`](src/bottom_pane/list_selection_view.rs) keeps the
  caller-owned selection model and makes presentation explicit. Searchable or
  browsable entity collections opt into `picker()`, bounded action sets opt
  into `menu()`, and content that depends on an application-owned rich header
  can retain `ListSelectionView`.
  [`BottomPane::show_selection_view`](src/bottom_pane/mod.rs) selects the
  corresponding adapter without moving configuration types, application
  events, or callbacks into the component crate.
- [`ComponentPickerView`](src/bottom_pane/component_picker_view.rs) maps
  Crossterm events into the domain-free `PickerAction` vocabulary from
  [`nori-tui-components`](../tui-components/docs.md). The adapter preserves
  current and initial selection, callbacks, keep-open actions, Shift-Tab
  actions, and typed footer hints while projecting names into one primary
  column and descriptions into supporting rows. The shared renderer suppresses
  that single column's heading and numbers the first nine actionable choices;
  the adapter routes an inactive-search digit to the same visible choice and
  submit path. Direct multi-column consumers, notably resume pickers, retain
  table headings. The component's `search_active`, query, filtering, and typed
  outcomes remain the source of truth.
- The picker adapter's [`Renderable::desired_height`](src/render/renderable.rs)
  measures visible rows at the selected density together with optional
  subtitle, category, active-search, and multi-column-heading chrome before the
  bottom pane allocates its bounded height. These presentation rows therefore
  do not silently consume the capacity reported for compact result rows.
- [`ComponentOverlayMenuView`](src/bottom_pane/component_overlay_menu_view.rs)
  projects the same caller-owned rows into `MenuState`, assigning number
  shortcuts only to enabled action rows and preserving current-state markers,
  consequence tone, callbacks, keep-open behavior, and dismissal callbacks.
  It renders the shared dense, zebra-striped overlay with symmetric focus rails
  and left-aligned hints inside the bottom-pane rectangle.
- [`overlay_menu.rs`](src/overlay_menu.rs) is the common raw-key adapter for
  bottom-pane and true full-screen menus. It maps arrows, `j`/`k`, paging,
  Home/End, Enter, digits, character mnemonics, Escape, Ctrl-C, and Ctrl-D into
  domain-free menu actions and ignores key-release events. Each caller retains
  the meaning of activation and cancellation.
- Specialized picker state machines use the same renderer only for their
  browsing states. For example, the hotkey and MCP views retain rebinding,
  form input, validation, and application-event routing locally while their
  selectable rows use the shared columns, surfaces, hints, and focus rails.
- Transcript search remains composer-owned because it loads `HistoryEntry`
  values asynchronously. [`HistorySearchPopup`](src/bottom_pane/history_search_popup.rs)
  owns its search and filtered-selection state, while
  [`ChatComposer`](src/bottom_pane/chat_composer/key_handling.rs) applies the
  shared keyboard contract and routes an accepted entry back into the composer.

The pre-TUI [`resume_picker`](src/resume_picker/) cannot use a bottom-pane
adapter, but it mirrors the same transitions and projects its active state and
query into the reusable picker renderer. Full-screen CLI picker surfaces use
the component's opt-in symmetric selection rails, including this launch-time
picker; embedded or copyable output does not inherit the rail treatment. The
directory-trust onboarding step, worktree ask and blocked screens, and release
update prompt are bounded full-screen actions, so they hold typed `MenuState`
locally and use the same [`OverlayMenu`](../tui-components/src/menu/render.rs)
and raw-key adapter instead of maintaining parallel row renderers.

#### Structured session information

Ordinary ACP `SessionInfoUpdate` normalization retains `title`, `updatedAt`,
and the complete `_meta` object as a private `SessionInfoPatch` alongside the
legacy text projection. The chat widget captures the agent identity reported
by ACP initialization and whether the current emission is live, agent replay,
or transcript replay, then sends every retained patch to two presentation
consumers:

- The history renderer emits a visible entry for every update. Known Codex
  status, goal, error, archived, and closed fields receive friendly labels only
  when the initialized agent identity is `codex-acp`. Unknown fields, fields
  from other agents, and malformed known fields are rendered recursively in
  deterministic path order as path/type pairs; their values are never
  displayed. Agent identity and other header components are sanitized and
  length-bounded, while fallback depth, assignment work, path length, and
  output count are capped with an explicit omission marker.
- The latest-state reducer recursively merges partial metadata for future
  footer and status-card consumers. Live values outrank agent replay, which
  outranks transcript replay. Empty metadata objects are merge no-ops and only
  an explicit nested `null` clears a path. Patch traversal, retained field
  count, and retained values are bounded; a new ACP initialization resets the
  accumulated state and its provenance.

This projection remains TUI-private. Raw ACP notifications continue to be the
only transcript records for these updates, so rendering session information
does not create derived transcript events or change the persisted schema.

#### Commands and approvals

User actions call typed `HarnessHandle` methods for prompting, cancellation,
history, prompt discovery, compaction, branching, undo, shell, goals, session
config, session listing, close, and shutdown. The old generic operation bus is
gone.

The `/approvals` preset chooser and the replace-goal confirmation are bounded
bottom-pane actions and therefore use the overlay-menu presentation. Warning
tone is carried only by the consequential action; selection, number shortcuts,
and cancellation still flow through the shared adapter. Rich confirmations
that include explanatory cards or diffs retain their application-owned list
composition rather than forcing ordinary content into the menu component.

The `/fork` picker (`tui/src/nori/fork_picker.rs`) offers a "Branch from current
point" entry as its first item when the agent advertises ACP `session/fork`
(`AgentCapabilitiesView.session_fork`, sourced from the `InitializeResponse`);
with fork support it opens even with no earlier user messages. Selecting it
dispatches `AppEvent::BranchFromCurrent` -> `HarnessAction::Branch` ->
`handle.branch()`, which forks the current head via ACP `session/fork` and swaps
the active session. The remaining picker entries are the earlier user messages
and drive the unchanged rewind-to-message local fork path; agents without fork
support see only those.

`Esc` on an empty composer while idle (or `[` in normal mode) focuses the last
sent prompt in place: it becomes an amber card, everything after it is dimmed,
`[`/`]` move between prompts, `Enter` edits the prompt where it is, `r` rewinds
to it (restoring files), `R` rewinds keeping files, `f` forks a subagent from its
response and `Esc` leaves. A running turn is interrupted by `Esc` instead. The
whole model lives in `app/focus.rs` and is drawn by `stage/focus.rs`; see
`docs/tui-stage.md`. The same model walks the prompts of a focused child with a
conversation journal, where the actions are the journal's rollback and fork; a
child without one shows a notice in its chat. Rollback requires the recorded
response boundary and does not silently substitute a different action when it is
absent. Every rewind ends in `App::fork_conversation_at`.

The transcript overlay (Ctrl+T) captures the mouse while it is open
(`Tui::set_mouse_capture`, released when it closes and by `tui::restore`), so
the wheel scrolls it and clicks reach it; the terminal then selects text only
with its modifier (Shift on most). A click selects the cell under it, mapped
through the pager's row index for the last frame's width and scroll
(`PagerView::item_at`), and a second click on the same cell within 400 ms opens
that message's actions (`pager_overlay/message_menu.rs`). `[` and `]` step the
selection between messages (prompts and the first cell of each answer) and
Enter opens the actions; Esc keeps stepping back through prompts, from the
selection, as before. A prompt offers "Edit this prompt" (`ForkToMessage` before
it, with its text in the composer) and "Rewind here and restore files"; any
other message offers "Fork from here", which is the agent's own
`session/fork` when nothing was said after it and the agent supports it, and
otherwise `ForkToMessage` at the end of the message (the summary-context fork
the `/fork` picker uses). Every action is refused while a turn runs. A rewind
loads the edit journal and matches its undoable turns to the prompts from the
selected one on, newest first (`chatwidget/rewind.rs::turns_to_undo`; turns
the harness opens without a prompt go with the prompt before them); the
confirmation names those turns and either restores them with `undo_to` and
then forks before the prompt, or only forks. Prompts from an earlier process
have no journal, so nothing is restored for them.

`/continue` takes up the last turn when it ended cancelled, failed, or stopped
by `max_tokens` or `max_turn_requests` (`chatwidget/continue_turn.rs`): if the
agent had sent no text, reasoning, or tool call for it, the prompt is sent
again; otherwise "Continue where you left off." is. Such a turn ends with an
info line saying so.

When an ACP permission request arrives, the overlay renders the ACP subject and
options, then calls `respond_to_agent(request_id, response)` with the exact ID
from the request. Filesystem requests do not enter this path because the host
handles them. Terminal and extension request families are not advertised by
the current host.

#### Settings and session-config pickers

The agent's advertised ACP configuration is tracked once, in advertised order,
by `AgentConfigState` (`@/nori-rs/tui/src/nori/agent_config_state.rs`).
`ChatWidget` (`@/nori-rs/tui/src/chatwidget/agent_status.rs`) updates it from
`ConfigOptionUpdate` announcements, explicit `session/get_config` snapshots, and
the options a successful `set_session_config_option` echoes back. That one state
drives the `/config` and model pickers (which open from it instead of
re-fetching), the mode cycle behind the footer label and Shift+Tab, and the
status card — so every surface reports the
same values with the agent's own labels and order.

The `/settings` picker (TUI config, in `@/nori-rs/tui/src/nori/config_picker.rs`)
and the `/config` picker (ACP session config, in
`@/nori-rs/tui/src/nori/session_config_picker.rs`) return to their parent panel
after a value is applied, landing the cursor on the just-edited row. This lets a
user change several settings in one visit instead of reopening the slash command
after every change. Each panel re-derives its `initial_selected_idx` from a row
identifier: a `SettingsItem` enum for `/settings`, or the ACP option id for
`/config`.

That return belongs to the panel only. A value picker opened for a single
setting, such as `/model`, closes once its value is applied: every value
picker carries a `ConfigPickerOrigin` (`Panel` or `Direct`) from where it was
opened through the custom-model input to the set request, and only `Panel`
reopens `/config`. Selecting another provider's model starts a candidate session and applies the selected model before replacing the current session.

The Next and Previous Effort Level hotkeys (default `ctrl+j` and `ctrl+k`) step the agent's
effort option (the one it declares as its thought level, or named for effort or reasoning)
through the same path, and the footer's model segment shows both (`Sonnet 5.5 · High`).
`ctrl+shift+n` and `ctrl+shift+p` remain aliases where the terminal reports Shift; some
terminals fold it into the letter and report plain ctrl+n, which is indistinguishable from
Next Model. Recognized models are ranked by the configured model ordering;
unrecognized models retain their advertised order (`agent_config_state::sort_models`).

The Next Model and Previous Model hotkeys (default `ctrl+n` and `ctrl+p`) step
through the models the agent offers without a picker (`chatwidget/model_cycle.rs`,
origin `Cycle`). A press updates the advertised configuration in place, so the
footer shows the new model at once (the `model` footer segment, on by default, beside
the mode above the prompt; it hides when the agent offers no model choice) and the
agent's echo of the change adds no history line. One request is out at a time: presses made while it is out only
move the target, which is sent when the request returns. A refusal restores the
model from before the run and says why. Up and Down keep prompt-history recall;
with a popup open, `ctrl+n` and `ctrl+p` move through its list.

`/settings` is searchable through the shared-picker adapter. Each setting
builds its search value from the displayed name and optional description before
the panel opens, so filtering can match either the setting identity or its
user-facing explanation without coupling the component state to configuration
types.

- For `/settings`, each `App::persist_*` success path in
  `@/nori-rs/tui/src/app/config_persistence.rs` calls
  `ChatWidget::reopen_settings_focused` (in
  `@/nori-rs/tui/src/chatwidget/pickers.rs`), which rebuilds the panel from the
  current config while preserving the ephemeral per-session loop-count override.
- For `/config`, a successful set in `@/nori-rs/tui/src/chatwidget/pickers.rs`
  re-emits `OpenAcpSessionConfigPicker` with the refreshed options and the edited
  `focus_config_id`.
- The Vim sub-picker is reachable from both `/settings` and the standalone
  `/vim` command, so `AppEvent::SetConfigVimMode` carries a `from_settings` flag;
  only settings-originated changes reopen the panel, while `/vim` just closes.
- Excluded from auto-reopen: the mode-cycle hotkey (Shift+Tab must not pop
  `/config` open), the multi-toggle Footer Segments sub-picker (which already
  stays open by replacing itself), and the bespoke Hotkeys view. Failed persists
  never reopen.
- Model-category value pickers (`acp_session_config_value_picker_params()`)
  split into two labeled sections when both have content. **Recommended** lists
  the values the agent advertises over ACP, exactly as reported. **Other** lists
  a curated, per-agent set of real models the adapter does *not* advertise but
  that generally run when forced via spawn-time injection; it comes from
  `nori_harness::AgentKind::other_models()`, resolved in
  `@/nori-rs/tui/src/chatwidget/pickers.rs` via `AgentKind::from_slug` on the
  active agent (custom/unknown agents resolve to an empty list → no Other
  section). The Other list is deduplicated at render time against the advertised
  values so a model an account already sees under Recommended never appears
  twice. When either side is empty — no advertised catalog, no curated
  complement, or a non-model option — the picker renders one flat list exactly as
  before and the labeled headers do not appear. The durable-complement design is
  intentional: the advertised set is agent/version/account-dependent, so the
  curated list plus runtime dedup avoids a second, drift-prone source of truth.
- An Other row whose id already equals the session's injected `currentValue` is
  marked `(current)` and carries no action; every other Other row emits
  `AppEvent::SetAcpSessionConfigOption { is_custom_model: true }` — the same event
  a free-text custom entry emits — so it follows the identical
  reject → persist-to-`[default_models]` → restart-with-injection recovery path
  (below). `initial_selected_idx` prefers the current row.
- Section labels remain non-selectable through `SelectionItem::is_header` in
  [`list_selection_view.rs`](src/bottom_pane/list_selection_view.rs). The shared
  adapter maps that flag to `PickerItem::section_heading`, so grouped model and
  config rows render as bold structure rather than faded disabled choices;
  keyboard navigation and default selection continue to skip them. Retained
  legacy lists project the same flag into `GenericDisplayRow` and keep their
  selectable-only numbering.
- `acp_session_config_value_picker_params()` also pins a "Use custom model..."
  entry at the bottom of every Model-category picker. Selecting it emits
  `AppEvent::OpenCustomModelInput`, which opens a `CustomModelInputView`
  (`@/nori-rs/tui/src/nori/custom_model_input.rs`) — a `BottomPaneView` text
  input for free-form model IDs. On submit it emits
  `AppEvent::SetAcpSessionConfigOption` and follows the same
  `session/set_config_option` path as selecting from the advertised list.
- The config-set events (`SetAcpSessionConfigOption` and
  `AcpSessionConfigSetResult` in `@/nori-rs/tui/src/app_event.rs`) carry an
  `is_custom_model` flag that distinguishes injectable Other or free-text model
  choices from an advertised picker choice. When the live RPC rejects one of
  these model choices and the active agent `supports_model_injection()`, the
  handler in `@/nori-rs/tui/src/app/event_handling.rs` treats the rejection as
  recoverable:
  `persist_custom_default_model` (in
  `@/nori-rs/tui/src/app/config_persistence.rs`) writes the value to
  `[default_models]` unconditionally (no config-options snapshot to categorize —
  it is a model by construction), an info message
  ("Saved '<model>' as the default model for <agent> — restarting the session to
  apply it.") is shown, and `AppEvent::NewSession` restarts the session so the
  model is injected at spawn. If the agent has no injection channel, the original
  error is surfaced instead.

#### The `/browser` profile picker

On Unix, `/browser` (in `@/nori-rs/tui/src/chatwidget/key_handling.rs`) reuses a
running browser if one is active; otherwise it loads the saved default
`browser_profile` from config and opens a three-tier picker
(`browser_profile_picker_params` in `@/nori-rs/tui/src/nori/config_picker.rs`)
pre-selected on it. The tiers are `Throwaway` (fresh temp profile, the secure
default), `Persistent nori profile` (nori-owned, logins persist), and `Real
Chrome profile` (the user's real Chrome, danger-noted). Selecting a tier emits
`AppEvent::SetBrowserProfile`, whose handler persists the choice as the new
default (`persist_browser_profile_setting`, writing the top-level
`browser_profile` key) and then launches Chrome with it via
`launch_browser_session`, which spawns `BrowserSession::launch_and_store(mode)`
in `nori-harness`.

#### Terminal requirements

Initialization requires stdout to be a terminal. It does not require the same of
stdin: crossterm reads keys from the controlling terminal whenever stdin is
redirected, so the guard only checks that a controlling terminal (`/dev/tty`, or
`CONIN$` on Windows) can actually be opened. The older stdin-must-be-a-tty rule
was nori's own policy rather than a crossterm limitation, and dropping it does
not change where input comes from.

This is what lets `echo "..." | nori` run as an ordinary interactive session. The
CLI composes the piped text into the normal `TuiCli` prompt field before launch,
so it seeds the first turn and nothing downstream distinguishes it from an
argument prompt (see `@/nori-rs/cli/docs.md`). A pipe never selects headless
behavior; that requires `nori exec` or `nori -p`, which never open a UI.

Having drained the pipe, the CLI re-points file descriptor 0 at the controlling
terminal before the UI starts. This matters to every child the TUI spawns with
inherited stdin -- the external editor and the file browser -- which would
otherwise be handed an EOF'd pipe and exit immediately.

#### Native terminal scrollback

Nori stays on the terminal's primary screen throughout startup, conversation
navigation, search, response focus, suspend/resume, and shutdown. It never
enters the alternate screen. The **stage** (`stage/`) owns every row from where
the program started to the bottom of the terminal: the tail of the conversation
(cells not yet in the scrollback, drawn into the frame and so redrawable) and the
bottom block (live tail, composer, status line). Rows leave the tail from the top
only when the region is full: the screen scrolls, the rows that pass the top edge
are in the scrollback exactly once, and the boundary moves past them. While a
prompt is focused or a tool card expanded the tail is clipped instead of
scrolling. Full-screen views (pickers, child conversations, the pager) draw over
the region without scrolling or committing anything; the next ordinary frame
draws the tail again. See `docs/tui-stage.md`.

Resize events are coalesced into the next draw. The event stream asks the terminal where the
cursor is after a resize; the difference from where the last frame parked it says how many rows
the terminal pushed into the scrollback or pulled back. Pushed rows of the tail are marked
committed, the region is erased row by row and the rest of the tail is drawn again at the new
width, so the conversation stays visible and no row is drawn twice. Without an answer the context
is kept and a row may repeat. Composer drafts, cursor positions, selections, and queued messages remain scoped to their
conversations. See `docs/tui-stage.md`.

Native wheel scrolling takes priority on the main conversation. The root chat
leaves mouse reporting off so the terminal owns wheel scrolling and native
selection. The picker and a focused child temporarily enable application mouse
capture for their own wheel and click gestures; closing either view restores the
primary terminal immediately.

#### Startup

Unversioned development builds (`0.0.0`) skip the release-update modal so the
composer opens directly. The startup card omits optional installer advertising;
system metadata remains available through `/status`.

Nothing before the first frame starts a process or waits on the network. Git
facts for the footer and for project trust come from `nori_config::GitFacts`,
which opens the repository in-process with `gix` using only the repository's
own configuration. Agent installation checks look binaries up in `PATH`
in-process. The Codex auth manager builds its HTTP client only once saved
credentials exist. A transcript chosen with `nori resume` loads on a task while
the agent starts (`PendingSessionActivation::ResumeLoading`), never before the
first frame, and is moved (never copied) to the session that replays it. The
first frame is drawn directly once the app is ready
rather than through the frame scheduler. `NORI_STARTUP_TRACE=<file>` appends
`<phase> <microseconds>` for each startup phase (`startup_trace.rs`) to find
anything that creeps back onto this path.

#### Streaming paint, local echo, and redraw

Agent deltas only append; once per frame (frames are at least 8 ms apart)
`ChatWidget::commit_stream_for_frame` renders what arrived, sends the lines
that became final to the scrollback in one cell, and shows the rest as the
live tail above the composer. Adjacent text deltas of one message are merged
before they reach the UI (`chatwidget/conflate.rs`), so a frame costs the
messages that changed, not the token rate. `MarkdownStreamCollector` freezes
each completed top-level block and re-renders only the open one; a top-level
fenced code block is streamed by `markdown_stream/fence.rs`, which highlights
each line once with the syntax state carried forward, so a long code block
costs its length in total, and at most 400 of its lines a frame, so a burst of
thousands is on screen from its first frame. A turn that ends with such a burst
still unrendered leaves the rest to the frames that follow (`stream_ending`)
rather than rendering it in one stall; anything shown after it waits for it.
The width-aware live renderer keeps only the newest rows of a very long
unfinished paragraph active. Completed cells are handed to the stage, which wraps them to the screen. Structured cells retain their
own width-aware layouts. On exit, an exceptionally large unfinished line is
abbreviated in terminal output to keep shutdown responsive; its recorded source
remains in the conversation journal.

Cells reach the screen through the stage (`stage/mod.rs`): a long history, such as a
resumed session, is typed from the top of the region in one frame and scrolls into the
scrollback as it goes, each row once and in order.

A prompt submitted while the session is idle is painted at once. Its
`PromptEchoId` travels in `_meta` on the `session/prompt` request and on every
`user_message_chunk` the harness echoes, and `chatwidget/local_echo.rs` drops
the chunks whose id it painted: a hash lookup, never a text comparison.

Each frame is written with one `write` through `FrameWriter`, inside one
synchronized update. The content flows from where the program started and the
bottom block follows it until the region is full; the Redraw hotkey (default
`ctrl+l`) pushes everything on screen into the scrollback, which stays whole
(`ESC[2J` and `ESC[3J` are never sent), and starts a clean region with the composer
at the top.

The terminal's colors are not asked for before the first frame: a new terminal tab
can take seconds to answer, and the question held the first paint back for its
whole two-second timeout. The background from the previous run
(`.terminal-colors-<TERM_PROGRAM>` under the Nori home) is used for the first frame; the
foreground, which only tints the shimmer, is derived from it. A terminal that has
answered before is asked again four seconds after launch from the event loop, and any
terminal when its window gains focus, and a changed background redraws.

The header card is written for the first conversation of a run only. A later one (an
agent switch, `/new`) writes the line announcing it and, once the agent lists them, its
options; the footer names the model and mode.

The composer holds a fast first character back for 8 ms in case it begins a
paste that arrives without bracketed-paste markers; the frame that releases it
draws it, rather than asking for another frame and waiting out the frame
interval too. An `@` file search runs at the first keystroke (`file_search.rs`);
keys typed while it runs only update the query, and the newest query is searched
the moment it finishes, while a query that no longer extends the running one
cancels and replaces it. A Markdown table is set apart from the blocks around it
by a blank line, as every block is.

#### Subagents

Child sessions use the same conversation canvas and composer as the root.
The harness routes child-scoped ACP updates separately from parent output and
preserves immediate parent IDs for nested children. Nori task tools and native
provider children share this presentation.

Running subagents are drawn in the chat as lanes of one tree under `◈ main`
(`chatwidget/subagents/lanes.rs`), never below the composer and never as a
separate view. Each child is one row: `● name  agent/model · 12s · latest
activity`, the state word shown only when it is not `running`. A subtree that
is not focused is one row (`⋮ 4 nested · ●2 ✓1 ✗1 · what is happening in it`),
however deep it goes; focusing a child opens the path to it, one row per child,
each child's own subtree compressed again. With several children running in
parallel at 100 columns or more the lanes sit side by side as cards of three
rows (who, what it is doing, what is under it). Idle and finished children
leave the lanes; a finished child becomes one transcript cell (`✓ name
agent/model · 1 tool · 34s`, the start of its conclusion, and one row counting
what finished under it). Tapping a lane opens its conversation.

Ctrl+Tab cycles recently visited conversations. Tab moves to the focused
conversation's parent. Escape stops work in the focused conversation. Each
conversation retains its own draft, attachments, selection, cursor, undo state,
and queue. Enter queues a follow-up, Shift+Enter steers, Alt+Enter inserts a
newline, and empty Enter promotes the next queued message. A closed provider
session is distinct from a canceled turn in an otherwise usable session.

Ctrl+Space opens fuzzy child search with a conversation preview. Selection is
bound to a conversation ID rather than its changing row position. Enter opens
the selected conversation in the shared canvas. `/reference` searches locally
recorded conversations for the current project; selecting one inserts a bounded
excerpt and its identifiers into the draft.

#### Transcript cells

Every cell is drawn from one design system (`history_cell/chrome.rs`) and takes
its colours from the live theme's tokens, never from names: a status glyph in
column 0 (`●` done, a spinner while it runs, `●` coral when it failed), text from
column 2, a one-line header `● Verb target · meta` that is truncated, not
wrapped, and a body that is collapsed until the cell is expanded. A cell exposes
`expansion()` / `set_expanded()`; the transcript view's own toggle works on call
ids and uses the same collapsed and expanded renderers.

- **Reads, searches, listings** (`client_tool_cell/explore.rs`): one call is a
  line (`● Read src/app.rs:120–180`); several in a row are one card, a header
  counting them (`● Explored · 5 files · 2 searches`) over a grid of two to
  four equal tiles by width (`history_cell/bento.rs`), each `◈ dir/file.rs:10–40`;
  expanded, each tile grows into a few rows of what it returned (a read's first
  lines numbered and coloured, a search's matching lines with the hit marked). One
  search is `⌕ “query” in src · 12 matches`. Paths inside the working directory
  are always relative.
- **Edits** (`client_tool_cell/edit.rs`): `● Edited path  +14 −4` over the diff,
  drawn the way a document editor shows suggestions (`diff_render/`): one column
  of the new text, line numbers faint, every edited line merged in place (the words
  it lost struck through where they stood, the words it gained marked), a line that
  only appeared marked whole, one that only went struck through whole, and a bar in
  the margin of each changed line (mint added, coral removed, amber edited). A line
  rewritten more than it kept, or spread over several lines, is shown as what it was
  and then what it became, removals first in each run of changes. Lines are paired
  with the line they are an edit of by what they share, a little more when they keep
  their first token and more again when they keep the name after it (`fn area`).
  Colour sits behind text only, never to the edge of the row. At most 14 rows
  collapsed and `⋯ +N more lines`. `/diff` shows the working tree's changes (since
  the merge-base with the default branch, plus untracked files) with the same
  renderer, `± Changed N files`, in the pager; binary files are named, not drawn.
- **Shell** (`history_cell/shell_block.rs`, shared with `!` commands): `$ command`
  highlighted by brush-parser (`render/bash_highlight.rs`, the only shell
  highlighter: exec cells, `!` cells, lane activity, picker previews and fenced
  `bash` blocks all use it), the duration only when it took a second, a non-zero
  exit as a coral chip, and the tail of the output: the last two rows, six when it
  failed, `⋮ N earlier lines` above.
- **Peer messages** (`history_cell/peer.rs`): `✉ reviewer → main · queued`
  (`delivered`, `acked`, read from `SubagentMessage.delivery`) and the first
  rows of the body, each name in the style of its provider. The harness
  re-announces a message as its delivery advances; the card is drawn once.
- **Who runs it** (`Chrome::identity`): a model is shown by its short name in
  the theme's `provider_*` style, and the agent only when the model does not
  say where it ran (`deepseek/v4 via opencode`). Lanes, finished subagents,
  peer cards and route changes all use it.
- **Route changes and retries, agent-owned turns**: `⇄ from → to · reason`,
  `↻ Retrying in 4s · 2/4 · overloaded`, `◈ The agent is working · unprompted`,
  each one row.
- **Nori's own words** (`history_cell/notice.rs`): `◇` information, `▲` warning,
  `✗` error, quiet text. Notices that arrive while a turn runs are one cell, and
  a notice never lands inside the model's text: the answer being streamed is
  finished first. An adapter's warning carried in the text stream
  (`nori_protocol::AGENT_WARNING_PREFIX`) is never conflated with the text around
  it and is split out wherever it appears in a chunk.
- **Nothing lands inside a message.** A finished subagent, a peer card, a notice or
  a route change that arrives while an answer streams waits for the message to be
  whole and is committed after it (`ChatWidget::add_side_cell`); the harness's
  message flow does not end a message for subagent or task-result events. A
  child's `✗` card is live: if the child runs again (its fallback route took over) the card
  on screen flips to `↻ … continued on another route` in place; a peer card reads its delivery state (shared with the
  harness's announcements) each frame it is on screen, so it moves from `queued` to
  `acked` or `failed · reason` in place (`HistoryCell::animated`). A tool call closed after
  its turn's response updates its cell instead of making a second.
- **Proposals wait.** A call that asks permission shows `Edit src/x.rs · awaiting
  approval` with a still glyph, not a spinner and not the past tense.
- A plan update that only moved steps along draws the new count and the steps that
  moved, not the whole plan again.
- A turn that took under ten seconds has no "worked for" rule; longer ones get
  one faint row with the duration.

Unknown structured results use labeled fields and lists rather than serialized
JSON. Expansion is scoped to the conversation, and pointer release must match
the original target so scrolling does not accidentally toggle a tool. The
presentation does not replace the canonical rich tool result passed through the
harness.

#### File-change diffs

An edit tool's change reaches `diff_render/` as `ui_types::FileChange::Update`:
the text it replaced, the text it wrote, and the line of the file they begin
on (`nori_harness::patch::first_line_in_file`, read when the tool's snapshot
arrives, so the numbers survive the file changing again). The two texts are
diffed with `disparate` when they are drawn (`diff_render/update.rs`). For a
file whose extension names one of the grammars disparate compiles in (Rust,
Python, JavaScript, TypeScript, Go, C, C++, Java, JSON, C#, Zig, HTML, CSS,
Bash, Ruby, TOML, YAML), and when both texts parse without errors, the diff is
structural (`disparate::syntax::Structural`): re-indented lines are unchanged
and fold away with the context, rewrapped lines are changed but have nothing
new in them, and code that moved is not new where it went. Otherwise the lines
are diffed, and the words of each edited line are diffed against its
counterpart. Either way, three unchanged lines are shown around each hunk and
`⋮` stands for the rest; a hunk's removed lines come before its added ones;
the header counts are the diff's lines removed and added.

Each line has right-aligned faint line numbers, `+`/`-` in the accent colours,
syntax colours from the theme's tokens (`render/syntax_theme.rs` builds the
syntect theme from them), and the theme's diff tints (`diff_add`, `diff_del`);
256-colour terminals tint from the xterm palette. A changed line that is an
edit of another also shows what changed in it, on a stronger tint over the same
syntax colours (`disparate::layout::segments` cuts the line where colours and
changes begin and end): in a structural diff, the novel tokens on it (a line
with some tokens kept is an edit); in a line diff, the words that differ from
the line it is paired with. Lines are paired by `View::rows_by` with
`EditPairing`, disparate's `Dice` score raised a little for lines that start
with the same token, because an edited line keeps its head: when an edit also
splits a line, the half that kept `let total =` is its partner, not the shorter
half. A paired line that was also rewrapped (split, or joined with its
neighbour) is compared with all the lines it became, and when those have
nothing new in them the words that only moved are not emphasised. A line new
throughout is only tinted, as added and removed lines are. On 16-colour terminals, which have no
tints, the changed words are bold instead. A diff costs what
changed, not the file: equal lines are compared as bytes, only the top-level
items around changes are parsed, and only the lines shown are highlighted.
Diffs are drawn on the UI thread and cached per width with the cell.

#### Lifecycle behavior

Connection preparation is separate from session activation, but ordinary local
startup records a pending New decision as soon as it begins preparing, so the
frontend issues `session/new` on its own once the connection is ready instead of
waiting for a first prompt. This keeps session-scoped UI (`/model`, `/config`,
`/approvals`, Ctrl-R prompt-history search, and Up/Down prompt recall) alive out
of the box, because in ACP those surfaces only exist after `session/new`. Every
subprocess agent starts through [`session_setup.rs`](src/app/session_setup.rs),
including ordinary local agents and a registered `nori-handroll acp --type
remote` adapter. `App` retains the harness's opaque `PreparedAgent` after
`initialize`, capability inspection, and optional `session/list`; the pending
New decision consumes it as soon as preparation completes. Advertised listing
runs in the same background preparation while the composer remains usable and
does not block activation. Unsupported listing is distinct from an empty
successful catalog, and preparation or advertised-list failure leaves the widget
sessionless. [`chatwidget/agent.rs`](src/chatwidget/agent.rs) is the boundary
that consumes the exact prepared connection into the harness runtime. An
ordinary-agent failure reopens the existing agent picker so the user can recover
by choosing another agent unless a live candidate already owns its picker; cloud
remains on its sessionless `/resume` or `/new` retry flow.

Cloud startup and agent-switch candidates keep deferred activation as an
invariant: cloud stays picker-first and sessionless until the user chooses a
session, and a candidate stays hidden until it publishes `SessionStarted`. Only
the plain-local and per-session-skillset startup paths auto-activate; the
prepared/candidate connection lifecycle and teardown are otherwise unchanged.

A prepared agent carrying the recognized Nori remote-control active-session
marker bypasses both primary and switch-candidate pickers. The TUI emits its
existing resume action for the advertised stable session ID, and the harness
uses `session/load` on that same connection without issuing `session/list` or
`session/new`. A failed automatic load leaves the primary session unattached or
the candidate uncommitted; it never creates a replacement session.

When ordinary agent startup fails, the handler records the complete failure in
history and opens the provider picker as the recovery action. Because that
full-height picker can place the history cell outside the viewport, the same
spawn error is also its subtitle; the `/providers` command instead offers connection and sign-in actions. This recovery path is assembled by
[`ChatWidget`](src/chatwidget/pickers.rs) and the shared picker parameters in
[`agent_picker.rs`](src/nori/agent_picker.rs), without changing the error value
or treating it as picker state.

Primary preparation is owned as a generation, task abort handle, current
intent, retained fork context, and optional pending activation. Ordinary local
startup records New up front so activation follows preparation automatically.
When per-session skillsets are enabled the agent spawn is deferred until a
skillset is chosen: applying a skillset writes its workspace state
(`.claude/CLAUDE.md`) and then records New, while dismissing the picker records
New without a skillset — both preserving the ordering that the skillset is
written before `session/new`. `/new` and a first genuine user prompt record New
as well; `/resume` changes the intent to open the prepared catalog and then
records the selected Resume. Esc-Esc backtrack and transcript fork also queue
deferred New while preserving their selected history summary as initial context. If preparation is in flight, these decisions wait
for it instead of cancelling it; if preparation is complete, they consume the
stored connection. Thus `initialize`, optional `session/list`, and the chosen
`session/new`, `session/load`, or `session/resume` use one child process. This
orchestration lives in [`event_handling.rs`](src/app/event_handling.rs).

Every primary activation calls
[`take_refreshed_prepared_agent`](src/app/session_setup.rs) before consuming the
connection. The refresh applies current mutable approval and sandbox policy and
retains fork context. If process-defining identity changed, the TUI reaps the
stale child and reprepares while keeping the pending New or Resume decision;
stale policy is never activated.

During the activation window — the gap between recording a New/Resume decision
and `SessionStarted`, which now exists even on plain-local startup and also for
the cloud, candidate, positional, and backtrack/fork flows — the sessionless
composer distinguishes activation input from local behavior. Any text or image
prompt entered before the session is live remains in frontend state, requests
New, and is transferred without rewriting to the activated widget, which submits
it exactly once when `SessionStarted` establishes the configured session.
Because activation rebuilds the composer widget, it transfers an owned
`ComposerDraft` rather than restoring a plain text copy. The draft retains the
cursor, undo/Vim state, atomic placeholders and their image/large-paste payloads,
and buffered characters with their original paste deadline. This also preserves
input that has not rendered yet. Ordinary `set_composer_text` replacement clears
old buffered input and puts the cursor at the end; it is not a handoff API.
The transfer runs [`helpers.rs`](src/chatwidget/helpers.rs) ->
[`bottom_pane/mod.rs`](src/bottom_pane/mod.rs) ->
[`chat_composer/draft.rs`](src/bottom_pane/chat_composer/draft.rs).
Initial positional prompts use the same path. Activation replaces the
sessionless widget, so `SessionStarted` first applies normal history metadata to
the new widget, then records its queued launch prompt into composer-local
history immediately before submission. Slash commands and local shell commands
are handled before this implicit-New decision; a shell command reports that no
harness is active until activation. Neither can claim an ACP session. These
ownership rules live in
[`user_input.rs`](src/chatwidget/user_input.rs) and
[`helpers.rs`](src/chatwidget/helpers.rs).

An `AgentPrepared` result is accepted only when its generation still matches
the owned preparation; a late successful result is explicitly shut down.
Primary and switch-candidate preparation both use the same 20-second
wall-clock bound in [`session_setup.rs`](src/app/session_setup.rs). Close,
cancellation, timeout, and exit invalidate preparation and reap its subprocess.
Together these rules keep a cancelled or hung initialize/list task from
repopulating the picker, leaking its child, or displacing a usable session.

Candidate state retains the candidate identity and opaque prepared connection;
it does not separately snapshot the full `NoriConfig`. When the user chooses
New or Resume, the TUI derives activation config from the latest `App` config
and asks the harness to refresh the prepared agent. Session-time settings
therefore include changes made while the picker was open. Agent identity,
working directory, ACP proxy/wire-recording settings, and the resolved default
model must still match preparation because they determine the existing process
or transport; a mismatch tears down the candidate and asks the user to retry
the switch. On `SessionStarted`, only the active-agent identity is committed
into `App`, so other current settings are never overwritten by candidate-era
state.

`/model` combines the active session's advertised models with models cached from
available providers. It shows clean model names, combining equivalent versioned
model IDs into a row while retaining their distinct routes. Matching unversioned
aliases share a floating model intent, while explicit versions remain distinct. When
available, the model family's native agent is tried first; an explicit authentication,
quota, or unavailable-model failure
tries another route for the same model. A route using another provider prepares a
candidate, checks the model against its advertised options, and applies it before
committing the switch. A failed candidate leaves the active conversation in place.
`/providers` offers the existing sign-in flows; ACP does not consistently report
authentication status. Highlighting a model whose preferred route uses another
agent can prepare that candidate in the background; dismissing discards it.

Candidate activation classifies hidden candidate events in
[`App`](src/app/event_handling.rs) before they reach the candidate widget. An
ACP error or pre-start `RequestFailed` is logged and captured rather than
forwarded into history. A safe ACP message is retained only when it has a
distinct, non-empty string `data.detail`; otherwise the request-failure message
becomes the retained fallback. On terminal `SessionEnded`, the TUI renders
exactly one failure through the still-active widget, preferring detailed ACP
context, then the request-failure message, then the terminal lifecycle message.
This preserves useful broker detail without duplicating hidden candidate
failures or exposing other structured fields.

Bare `/login` has a narrow candidate-target override. Selecting a candidate
sets it, and preparation or activation failure leaves it available so the user
can authenticate that attempted agent without recreating pending-switch state.
Explicit cancellation and successful authentication clear the override; a
successful switch replaces the widget and returns bare `/login` to the active
agent. The override affects login resolution only—prompts and lifecycle actions
continue to target the current session unless a real candidate state exists.

The Codex OAuth `/login` flow (`chatwidget/login.rs`) writes its credentials to
the Codex agent home (`$HOME/.codex`, resolved by `codex_oauth_credentials_home`
with a `nori_home` fallback), not `nori_home`. This matches where the codex-acp
subprocess actually reads `auth.json`, since that subprocess is spawned with
`CODEX_HOME` stripped (see `@/nori-rs/acp-host/docs.md`). The resolver
deliberately ignores `$CODEX_HOME` because the subprocess ignores it too.

Claude Code `/login` runs the installed official CLI's `claude auth login
--claudeai` command in Nori's in-app PTY. The CLI owns the subscription OAuth
flow and writes its credentials to its normal Claude home; Nori does not read
or copy those credentials. When authentication completes for the active Claude
agent before a session exists, Nori starts a fresh session so the agent uses
the saved login.

The app-owned `HarnessRemoteHost` follows the same commit boundary. `App`
attaches the stable host only after the active widget publishes
`SessionStarted`, seeding identity from that observed event. During a switch it
keeps following the current session until the candidate reaches that same
commit event, so a failed or cancelled candidate cannot displace the current
remote session. Listener enablement is independent of attachment: the host
continues following the active harness while remote control is off.

An orderly ACP close completes the typed close call, leaves the raw close
response observable on the stream, observes `SessionEnded(Closed)`, and then
handles stream closure. The TUI does not render a successful close-response
message. Explicit application shutdown uses `SessionEnded(Shutdown)`. Local
exit requests immediate owned ACP process-group cleanup; cloud exit allows a
short detach grace. The TUI exits when cleanup publishes `SessionEnded`, rather
than using an independent timer that can abandon reaping.

Events entering the application are tagged with their session generation.
Candidate events are routed to the candidate widget until activation commits;
after a session is replaced, events from older generations are discarded.
Replacement shutdown is based on the live harness handle rather than transcript
recorder or conversation-ID availability. Preparation generations apply the
same stale-result rule before a widget or active session exists.

Unexpected child or transport loss emits a request failure when work was in
flight followed by `SessionEnded(ConnectionLost)`. The TUI stays open so the
user can read the failure and choose the next action; connection loss is not
treated as a successful quit.

Cloud sessions use standard ACP `session/list`, `session/resume`,
`session/load`, and `session/close`. Capabilities describe what an initialized
ACP facade supports; they are not a sound test for whether its process
represents a remote VM. The top-level `nori cloud` launch supplies explicit
`cloud_mode` state through `TuiCli`, `App`, and `ChatWidget`. Before the TUI
starts, [`run_main`](src/lib.rs) bypasses the local auto-worktree policy — its
ask and blocked screens as well as automatic creation — and the local
first-launch/directory-trust flow for this mode. During application
construction, [`App`](src/app/mod.rs) also suppresses per-session skillset
selection and its deferred agent preparation. These settings remain available
to ordinary local sessions but cannot intercept, change the working directory
for, or delay Handroll Cloud startup.

Cloud entry is normally picker-first: `App::run` prepares one connection and
opens its session picker before any session directive can claim a VM, with
"Start a new session" as an explicit pick. The prepared child remains alive
behind that picker and the selection consumes it rather than spawning a second
agent. The clap-skipped `cloud_onboard` flag (`nori cloud --onboard`, for
customer onboarding) skips the picker but runs the same bounded connection
preparation. It remains a Handroll/ACP onboarding path and does not re-enable
the suppressed local startup flows. The broker projects the config-pinned
onboarding session as `_meta.nori.purpose = "onboarding"`;
when that tag is present, the TUI emits the existing resume action and the
harness uses `session/load`, including recorded-history replay. If no tagged
session is present, or listing is unsupported, onboarding explicitly starts a
new session on that same prepared `cloud-acp --onboard` connection, preserving
the broker's serialized acquire-or-resume behavior and compatibility with older
components. An initialization or advertised-list failure is shown as a
preparation failure rather than being treated as an empty catalog. A
sessionless `/new` consumes that valid prepared onboarding connection; after an
active session, `/new` prepares another connection with the same onboarding
registry entry. `/close` retains its ordinary picker-first lifecycle. Because
the `--onboard` argv is part of the process-wide agent registry entry, every
fallback acquisition remains onboarding-only. While a
picker-first launch waits for a choice, its initial positional prompt and image
attachments remain owned by the deferred widget. Choosing Start new transfers
that input into the replacement widget before the deferred widget shuts down,
so it auto-sends at the same `SessionStarted` boundary as every other entry
path.

Consuming a prepared Cloud connection for either New or Resume immediately
adds a durable “Connecting to Nori Cloud…” history entry and shows the live
connecting indicator. The constructors in
[`constructors.rs`](src/chatwidget/constructors.rs) enter this state through
the presentation helper in [`helpers.rs`](src/chatwidget/helpers.rs).
`SessionStarted` is the readiness boundary: it hides the indicator, applies the
connected session state, and renders the existing status card. The composer
remains editable while activation is pending, but
[`key_handling.rs`](src/chatwidget/key_handling.rs) restores the submitted text
on Enter instead of passing it to the harness; the user must submit again after
the status card appears. This prevents the harness's ordinary pre-activation
command queue from accepting a Cloud prompt before the remote session is ready
while preserving the user's draft. A pre-start `RequestFailed` or
`SessionEnded` also clears the live indicator, so any failure output is not
accompanied by stale connection state; the durable progress entry remains in
history. Snapshots in [`chatwidget/tests`](src/chatwidget/tests/) pin the live
indicator, durable connecting entry, stopped indicator with its preserved
draft, and terminal failure history as separate presentation boundaries.

The shared ACP resume picker treats session source as first-class presentation
instead of requiring users to inspect raw metadata. Cloud rows with a typed
`_meta.nori.sessionType` are labeled `Slack`, `CLI`, or `Web`; typed internal
sources are excluded from this user-facing picker, while untyped legacy cloud
rows remain available as `Unknown`. When every listed session is cloud-backed,
the table drops local-only working-directory and turn-status columns and gives
the session title most of the row width. The reusable picker renderer follows
the same priority at wide terminal sizes: once its detail pane becomes visible,
the list and detail panes receive a responsive 2:1 share of the available
width rather than fixed list sizing.

That launch-origin state retains the cloud ACP session id for footer and
welcome-card identity, rejects local-only commands, and selects cloud
detach/reattach wording. Reattach copy does not promise whether history is
replayed because that is selected independently from ACP capabilities.
Quitting an attached cloud session detaches through connection teardown;
`/close` is available only in cloud mode and remains gated by the facade's
`session/close` support.

#### Remote ACP transport activation

`--remote <ADDR>` (in [`cli.rs`](src/cli.rs)) serves the running interactive
session as a remote ACP agent over WebSocket, per
`@/docs/remote-control.md`. [`App`](src/app/) owns one
[`RemoteControlManager`](src/remote_control.rs) for the complete run. Startup
`--remote` and runtime commands both enter this manager, which retains one
stable `HarnessRemoteHost`, owns every `RemoteAcpServer` listener, and shuts the
listeners down on exit. All remote types reach the TUI through
`nori_harness::remote_agent` re-exports, preserving the dependency boundary.

The client-owned `/remote-control` command is handled before active-session
validation, so it never becomes an agent prompt and works while no agent is
active. Its forms are:

| Form | Runtime behavior |
| --- | --- |
| bare or `on` | Bind exact loopback on an allocated port. |
| `on tailnet` | Require `tailscale status --json` to report a running node and exact IPv4, then bind loopback and that address on one shared port. |
| `on IP:PORT` | Bind loopback and the exact address; a non-loopback address first opens a red, one-shot confirmation that is not persisted. |
| `off` | Disconnect the controller and stop all listeners while preserving the host and harness. |
| `status` | Report scope, reachable endpoints, and controller state. |

Wildcard addresses are rejected. Successful enable and status results become
durable history cells containing every reachable `ws://.../acp` URL, always
including loopback while enabled. Local-only mode may suggest `on tailnet` when
Tailscale is available, but does not present a tailnet URL until it is bound.
An explicit loopback target, including IPv6 loopback supplied at startup or at
runtime, is still classified as local-only and receives the same hint.

A bare startup port remains loopback-only. An exact non-loopback startup
address requires `--remote-allow-nonloopback`; after that gate it is normalized
to loopback plus the exact address on the requested port. Runtime replacement
normally binds the complete new surface before stopping the old one. When the
new target reuses an exact nonzero address already owned by the old surface,
the manager must shut down and await the old listeners first so the port can be
rebound. It first snapshots the old target and exact addresses; if the new bind
fails, it restores that surface and returns the original error, with the old
controller disconnected and able to reconnect. Only a restoration bind failure
leaves remote control off, and the user-visible error includes both failures.
Repeating the active target is idempotent.

On every active session's `SessionStarted`, [`App`](src/app/event_handling.rs)
attaches its stable host using the observed identity, even if listeners are
disabled. During a candidate switch, the old host attachment remains until
the replacement reaches that commit event. Committing the replacement closes
any current controller; after reconnecting, it discovers the new conversation
through `_meta.nori.remoteControl.activeSessionId` plus `session/load`; ordinary
ACP clients can continue to discover it through `session/list` on the same
configured listener endpoints. All remote types reach the TUI through
`nori_harness::remote_agent` re-exports, preserving the rule that the TUI never
imports the ACP host crate directly.

While a remote controller drives the session, the TUI stays attached to the
same handle and ordered event stream and renders remote-driven activity as an
observer. Canonical user prompt chunks reach both consumers, including the
frontend that submitted the prompt. The remote host rewrites only the outward
session id on forwarded updates and sends delegated permission requests to its
controller only when that controller owns the turn; the TUI continues to
observe the same request on its own stream. The transport retains one remote
controller, while the fan-out can feed future bounded observers. Policy for
simultaneous local and remote input is deliberately deferred by the spec.

#### Subagent navigation

The in-chat lanes show running actors only. Once the last actor becomes idle or
finishes, the lanes and their hit regions disappear. Completed actors remain in
the transcript and conversation picker.

Ctrl+Space opens the conversation picker. Plain text filters the roster using
Helix's `nucleo` fuzzy matcher; `%agent`, `%model`, `%status`, `%parent`, and
`%detail` restrict terms to those fields. For example, `%model deepseek` finds
children with a reported DeepSeek model, and `%status completed` matches the
visible `done` status. An unknown model stays unknown rather than inheriting
the parent's model. Metadata arriving after completion can still update the
retained entry without making the actor run again.

Press `/` to edit a search, then Enter to keep its matches and use `n`/`N` to
iterate. Ctrl+N/P and arrow keys move through matches; Ctrl+U clears the query.
Enter on a selected result focuses its conversation, and Ctrl+Space reopens the
picker with its search and selection retained. Wide panes show a transcript
preview beside the list; narrow panes keep the list usable without the preview.

Nested actors are represented as a compact tree. A grandchild is attached to
its immediate parent and is counted in the one row under that parent;
grandchild prose is available by opening the child conversation or its preview,
not by expanding the lanes into a second transcript. Parent completion is
deferred until known descendants have reported their terminal state, so the
root row is emitted once with stable sibling ordering. A completed conversation
can be reactivated only when it reports `running`; late chunks or metadata do
not resurrect a terminal entry. Reactivation keeps the old archived detail and
appends the new live detail under the same conversation identity.

#### Footer configuration

The shipped default is a quiet shell. An idle local session shows the agent
model and mode on the textarea's top-right corner, and branch, worktree, and
context on its top-left corner. The rows below the prompt remain available for
subagent navigation:

```
⎇ branch · 44% / 272k                                          [ Plan ]
› Ask Nori to do anything
```

`footer_left` holds only self-hiding state — the cloud session id when attached
through cloud mode, and the vim mode indicator when vim mode is on — so an
ordinary local shell leaves that group empty. Approvals, skillset, skillset
version, and session title are all off by default: they are static, restate the
transcript, or belong to a workflow the user has not opted into. Every one of
them still appears in `/status`, and each is one
`[tui.footer_segments]` line away from returning to the footer. Those segments
keep a default `footer_left` placement precisely so enabling them needs nothing
more than that one line.

On a terminal too narrow to hold both groups, the right group sheds trailing
segments until it fits and then disappears, rather than overwriting the left
group. Textarea corner segments are clamped to the composer and are skipped
entirely when the composer is squeezed below its three-row minimum, so they
never land on the prompt line.

`[tui.footer_layout]` accepts the same layout entries in `footer_left`,
`footer_right`, and all four `textarea_*` corners. An entry is either a built-in
segment name or an inline custom chunk:

```toml
[tui.footer_layout]
footer_left = [
  "git_branch",
  { format = "{context_used_percent} / {context_window_tokens}" },
  "approval_mode",
]
```

Built-in names are `prompt_summary`, `session_title`, `vim_mode`, `git_branch`,
`worktree_name`, `git_stats`, `context`, `context_used_percent`,
`context_remaining_percent`, `context_used_tokens`,
`context_remaining_tokens`, `context_window_tokens`, `approval_mode`,
`skillset`, `nori_version`, `token_usage`, `mode_indicator`, and
`cloud_session`. The default `context` segment renders used percentage and
maximum window size, such as `44% / 272k`. The five atomic context segments are
off as standalone entries by default so custom chunks can compose only the
values they need.

Naming a segment in any `[tui.footer_layout]` group moves it out of whatever
default group it started in, so a partial override never duplicates a segment
and never disturbs the groups it did not name. `[tui.footer_segments]` toggles
are applied on top and are never rewritten.

`session_title` shows the title the agent reports over ACP session-info updates
(`Title: Fix login flakes`). It self-hides for agents that never send one, and
the same value reaches the `title:` row of the `/status` card.

Custom formats are deliberately limited to literal text and built-in
placeholders. `{{` and `}}` emit literal braces. Unknown placeholders,
unbalanced braces, expressions, conditions, and format specifiers are rejected
when configuration loads. Referenced segments keep their styles and ignore
their standalone `[tui.footer_segments]` toggle; if any referenced runtime value
is unavailable, the complete custom chunk is hidden.

`FooterLayoutItem` and the load-time parser live in
`@/nori-rs/nori-config/src/types/mod.rs`. Composition and context formatting
live in `@/nori-rs/tui/src/bottom_pane/footer.rs`; resolved ACP or transcript
usage reaches it through
`@/nori-rs/tui/src/bottom_pane/chat_composer/rendering.rs`.

The values behind those segments come from `SystemInfo`
(`@/nori-rs/tui/src/system_info.rs`), collected on a single worker thread that
refreshes only when asked: at startup, on message submit, turn completion,
tool-call cwd change, and skillset apply. A full collection spawns Node for the
skillset version, runs `nori-skillsets list-active` for the active skillset
names, and walks the filesystem for transcripts and worktrees, which
is too slow to hold the first frame, so `SystemInfo::seed` runs a cheap subset —
the git branch and worktree identity, three short `git` invocations — and
`App::run` applies it before the loop starts. `configure_new_chat_widget`
re-seeds after a widget swap, because the replacement carries its own empty
footer. The background refresh then overwrites the seed wholesale. Debug builds
under `NORI_SYNC_SYSTEM_INFO=1` seed with a complete synchronous collection
instead, so E2E snapshots stay deterministic.

`SystemInfo.active_skillsets` is the single source for "which skillsets are
active" in both the `skillset` footer segment and `/status`.
`nori-skillsets list-active` reports every skillset installed for any agent in
the session directory or its parents, one name per line. When an older
`nori-skillsets` rejects the subcommand (non-zero exit with stderr output),
`get_active_skillsets` falls back to `read_active_skillset`, which walks parent
directories for a `.nori-config.json` `activeSkillset` key; current
`nori-skillsets` no longer writes that key per folder, which is why nothing
else reads it. The seed leaves the list empty, so it stays empty until the
first background refresh lands.

#### Session-info verbosity

ACP session-info updates carry an agent-defined metadata blob. Rendering all of
it documents what a harness supports and how agents differ, which is worth the
transcript noise while developing against an agent and not otherwise, so the
metadata history cell is limited to unstable builds — debug builds and
`X.Y.Z-next*` prereleases, per `is_unstable_build` in
`@/nori-rs/tui/src/version.rs`.

`SessionInfoDetail` in `@/nori-rs/tui/src/nori/session_info.rs` carries the
decision, and `ChatWidget` holds it as a field so tests can exercise the stable
path. `ErrorsOnly` is not silence: `error.*` assignments still render on every
build, because this cell is their only surface and dropping them would turn an
agent-side failure into a silent stall. Every build also merges every update
into `SessionInfoState`, so the session title keeps reaching the footer and the
`/status` card; only the dump is suppressed.

#### Status card

On a fresh Nori home, startup opens the shared composer immediately. The ordinary
header provides the branding; the first-launch welcome screen does not gate
input or rewrite the user's configuration. The separate trust flow remains
available when the resolved policy requires it.

The startup status block is an unbordered two-row summary beneath a green
prompt marker: system context on one row and agent identity on the next. The
system row is the location (directory, or the cloud session id and title) and
the approval mode. It deliberately carries no skillset: the welcome card is
rendered to lines once and written into terminal scrollback before the
background `list-active` refresh finishes, so a skillset there could never be
kept current. The
agent row reads provider, then the agent's model, then its thought level, then
its remaining options in exactly the order the agent advertised them; boolean
toggles read by presence (the label appears only when the toggle is on). Before
the agent advertises any configuration the row is the provider name alone —
nothing is guessed.

The card is written at startup, not at `SessionStarted`. Startup now
auto-activates the session, but the welcome card is still emitted immediately so
it shows while the agent resolves its configuration rather than blanking until
activation commits. `App::run` calls `ChatWidget::emit_welcome_card` once
everything local is configured; the agent row is the provider name alone at that
point, and when the session does start `on_session_started` updates the live
footer with the model and options without adding a history notice or second card.
Two entries keep writing the card at session start instead: a cloud session,
whose card names a broker session it only learns about once attached, and the
agent-switch candidate, which is hidden until it publishes `SessionStarted`.
When activation swaps in a fresh widget, `App::configure_new_chat_widget`
suppresses that widget's card so the conversation keeps the one card it already
has; a later `/new` is a different conversation and writes its own.

`/status` uses the same compact, unshaded definition-list grammar with plain
labels and a two-cell label/value gutter, expanding it into a superset of the
footer's information categories independent of the user's footer configuration:
directory, session id (the conversation id, shown for every agent — cloud
sessions append the broker title), title, summary, skillsets (with detected
skillsets version), approvals, a git row (branch / worktree / +added −removed /
untracked), and a single consolidated context row (`% left (used / window)`)
when the agent reports usage. Everything the agent decides then follows in its own
block: the provider on the `Agent` row and one indented row per advertised
option, labelled and valued exactly as the agent reports them. Local full status
also outlines every active instruction file with its token count; cloud status
omits this local discovery because it does not describe the remote agent's
context. Only the provider name receives its identity color; model, thought
level, separators, and agent-specific values remain in the terminal foreground.

Both renderings are pure views over a single `StatusViewModel`
(`@/nori-rs/tui/src/nori/session_header/status_view.rs`) that `ChatWidget`
assembles in `@/nori-rs/tui/src/chatwidget/agent_status.rs` from the config, the
footer values (`ChatComposer::status_footer_values()`), and the agent's
configuration state. The skillset row reads the footer's `active_skillsets`
list, so `/status` and the footer cannot disagree: the label is `Skillset` for
zero or one name and `Skillsets` for more, names are comma-joined with one
shared version suffix, and an empty list reads `(none)` — including a `/status`
issued before the first background refresh. The only filesystem input the
assembly reads directly is the instruction-file discovery
(`local_instruction_files`). The row helpers and the git/context formatting live in
`@/nori-rs/tui/src/nori/session_header/status_card.rs`; the storybook specimen
(`cargo run -p nori-tui --features storybook --example status_card_storybook`)
renders through those same views. The welcome card holds a live
`AgentStatusHandle`, so it fills in as soon as the agent advertises its
configuration; `/status` takes a detached snapshot so printed output does not
change afterwards. After a
branch-at-head fork the block also shows a `Forked from` row (the parent
conversation id): the harness emits `NoriEvent::SessionForked` when it forks the
transcript, and `on_session_forked` (`@/nori-rs/tui/src/chatwidget/event_handlers/nori.rs`)
updates `conversation_id`, records `forked_from`, and says so in a notice that
carries the copy-pasteable `nori resume <previous>` hint, so the previous (now
frozen) conversation stays resumable.

#### Transcripts and view-only mode

Between `ReplayStarted` and `ReplayFinished`, replayed updates go through the
same handlers as live ones (`chatwidget/replay.rs`), so a resumed session
renders as it did live: text that continues across a tool call stays one
message, and a duration rule shows how long its turn ran when that was ten
seconds or more, which the replayed chunk carries (`nori_protocol::turn_elapsed_in_meta`). A replay
has no turn lifecycle; a user message starts a turn and ends the one before,
doing to the history what the end of a live turn does, and the end of the
replay ends the last. The cells of a replay go into the transcript only, and
when it ends the newest of them are painted at once with the reflow's bounds
(`app/history_replay.rs`): a long session costs its replay's last 1,000 cells
and 10,000 rows in output rather than every row it ever wrote.
View-only rendering recovers initialization identity and replay source from the
stored lifecycle events, then runs raw session-information notifications
through the same private normalizer and renderer as the live TUI.

Transcript schema v3 contains lifecycle events even before
a prompt, so session pickers determine whether a transcript is empty by its
exact user-turn count rather than total record count. The loader retains private
v2 compatibility.

Goodbye-card statistics are accumulated from the TUI's private projection of
raw ACP updates and from Nori lifecycle state. They are presentation data, not
part of the public harness protocol.

### Things to Know

- Never add a public normalized ACP enum to make rendering convenient. Keep
  display inference private in `presentation/`.
- Query results arrive as typed method returns; do not add correlated Nori
  response events.
- The PTY suite in `@/nori-rs/tui-pty-e2e/` is the regression boundary for real
  terminal behavior, event ordering, approvals, lifecycle, and transcript
  selection.
- `nori-config` is the source of approval and sandbox policy; ACP session config
  options remain ACP schema values.
- `-p` belongs to the top-level CLI as `--print`. The TUI's own flag set must not
  claim it, and the legacy Codex `--profile` / `-p` selector stays rejected
  outright.

Created and maintained by Nori.


### Primary-screen rendering, `Ctrl+L` scrollback push, in-place rewind, and Helix pickers

- **Zero alternate screen**: Nori never enters the terminal alternate screen
  (`EnterAlternateScreen`). All overlays, pickers, subagent previews, and
  transcript views project inline on the primary screen so native terminal
  selection and scrollback remain intact at all times.
- **`Ctrl+L` redraw & scrollback push**: `HotkeyAction::Redraw` (`ctrl+l`) pushes
  everything on screen cleanly into native terminal scrollback (the stage
  scrolls it off, see `docs/tui-stage.md`) and starts a clean region with the
  composer at the top.
- **Direct in-place focus-backwards rewind & edit**: `Esc` on an empty composer
  while idle, or `[` in normal mode, focuses the last sent prompt in place inside
  the chat stream (amber `▌` rail, `↶ Turn K of N` header, `⇣ rolls back below on
  submit` divider, everything after it dimmed) without touching the composer
  draft. `[`/`]` or the arrows move between prompts; `Enter` or `e` edits the
  focused prompt inline and a second `Enter` restores the recorded files, forks
  before it and sends the edit; `r` rewinds to it (composer prefilled), `R` keeps
  the files, `f` forks a background subagent from its response; `Esc` leaves.
- **In-chat subagents & Helix two-pane pickers**: Live and completed subagents
  render directly inside the chat stream (`InChatSubagents`, clickable via
  primary-screen SGR mouse) rather than in a strip below the composer. `Ctrl+Space`
  opens the two-pane Helix subagent search + streaming preview overlay, and
  `/resume` opens the two-pane Helix full-text transcript search + match-highlighted
  conversation preview overlay.
- **In-process `!` `brush` shell**: Prefixing input with `!` executes the command
  in a persistent in-process `nori-bash` (`brush`) shell (`cd`, `export`,
  functions, and aliases persist across `!` calls) and renders an AST-highlighted
  `UserShellCell` directly in chat without claiming an ACP session.

### Selection-first prompt editing

The prompt uses a selection-first editor by default. Its Kakoune-based engine
supports the Helix-compatible keys below; `editor_mode = "helix"` selects the
same engine. It starts in INSERT mode so
text can be typed immediately. Press Escape for NORMAL mode. Use `h`, `j`, `k`,
`l` to move, `v` followed by motion keys to extend the selection, `x` to select
a line, `d` to delete, `c` to change the selection, and `i` or `a` to insert.
The prompt bar shows the current mode. Enter queues the prompt; Shift+Enter
steers the active turn, and Alt+Enter inserts a newline. Press Enter with an
empty composer to promote the next queued prompt.

To use a different editor, set `editor_mode = "vim"` or
`editor_mode = "standard"` under `[tui]` in `config.toml`. Existing explicit
`vim_mode` settings continue to select Vim or the standard editor.
