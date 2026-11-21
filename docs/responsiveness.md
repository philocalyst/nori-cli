# Responsiveness

No interaction may take more than 100 ms to show its response. The interface's event loop
(`nori-rs/tui/src/app/mod.rs`) handles one event at a time, so anything an event handler waits
for (a file, a disk flush, an agent's answer, a lock) is a stall of the whole interface: no key
is read and no frame is drawn until it returns. Handlers therefore never wait on the disk or on
an agent. Work that takes as long as its input is big, or as long as someone else takes to
answer, runs on a task, and its result comes back as an `AppEvent`.

## Measuring

`nori-rs/tui/src/latency.rs` times, always:

- every event the loop handles, by kind (`terminal: key`, `terminal: draw …`, each `AppEvent`
  variant): a slow one is the handler with the blocking call in it;
- interactions, from the input that asks for something to the first frame drawn after the
  response is on screen: `key → frame`, `/command → frame`, `picker open` (the resume picker
  with its rows), `model/effort/mode switch` (to the footer showing it), `agent switch` (to the
  new agent taking over), `session start`, `resume` (to the conversation on screen).

Anything over 100 ms is logged at once (`warn`, target `nori_latency`); every sample is logged
at `debug`. With `NORI_LATENCY_LOG=<path>`, the table of every path (count, median, 95th
percentile, worst, how many went over 100 ms) is appended to that file when Nori exits.
`tui-pty-e2e/tests/latency_budget.rs` drives a session through these paths and prints the table.
`harness/examples/index_latency.rs` times the index, listing and config writes on a large
synthetic session. Bringing the index of a session that grew by 50 records up to date took
1.3 s at 11 MB and 11.6 s at 90 MB when an update read only the appended bytes but wrote the
whole digest again; it takes about 10 ms at both sizes now, that of the appended bytes alone.

## What runs off the loop

- **Config writes.** Every `config.toml` edit goes to one config writer thread
  (`nori_config::NoriConfigEdits::apply`, `apply_in_background`), in the order made, so two
  edits never lose each other's change and no caller's thread touches the file. A setting is in
  effect when it is chosen; a write that fails says so in one line and the setting lasts for the
  session.
- **Session configuration.** A model, effort or mode change shows in the footer on the keypress.
  The harness hands it to a configuration lane (`harness/src/runtime/config_lane.rs`) that tells
  the agent one change at a time, in order, folding a change that a later one to the same option
  replaces; the harness's command loop never waits for the agent's answer, so a Cancel never
  queues behind one.
- **Switching agents.** The new agent's model, effort and conversation binding are put in place on
  a task (`tui/src/app/candidate_commit.rs`); the interface takes the new agent when they are.
- **Pickers.** The resume picker lists each session from its first record and the index, brings
  the newest up to date for at most 40 ms (an update reads and writes only what a session grew
  by: it appends the turns that became final, patches the search filter and replaces a small
  cursor and header; see [pickers.md](pickers.md)), and does not wait for the agent's own listing, which joins the
  open picker when it arrives.
- **Journals.** Each event is journaled once, written at once and flushed to disk in the
  background (see [transcripts.md](transcripts.md)).

## Changes during a response

Changing how a session runs never waits for a turn and is never refused for being mid-turn: the
model, effort and mode (applied from the agent's next model call), the approval policy, settings,
MCP servers, and the commands that open them (`/model`, `/config`, `/approvals`, `/settings`,
`/vim`, `/mcp`, `/providers`). A model that only another agent runs is accepted at once and
switched to when the response ends, with one line saying so. Commands that replace or rewrite
the conversation itself (`/new`, `/resume`, `/fork`, `/undo`, `/compact`, …) still wait for the
turn.
