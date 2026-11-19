# ACP turn lifecycle

This document describes the current Nori harness behavior for ACP requests,
session updates, and recorded conversation history. The reducer is implemented
in `nori-rs/harness/src/backend/session_reducer.rs` with its rules in
`nori-rs/harness/src/backend/turn/`; its per-session state is in
`nori-rs/harness/src/normalized/session_runtime.rs` and `nori-rs/harness/src/normalized/turn.rs`. The canonical event journal
is described in [Transcript journal and JSONL compatibility](transcripts.md).

A fresh session emits `SessionStarted` followed by `SessionReady` with the same
ACP session ID, without replay events. A resumed session emits `SessionReady`
after its deferred replay batches finish. Consumers can use the same readiness
boundary for either startup path.

Cloud sessions keep their broker-owned model route when they become ready.
Automatic preference for an equivalent local native agent applies only to local
sessions, so readiness cannot replace a freshly attached cloud session.

## The turn ledger

Each ACP session has one `SessionRuntime` and at most one **turn** in it: a span of agent
work with exactly one owner. Everything the agent does while the turn is open (message
text, tool calls, permission requests) is accounted to that turn. There is no separate
phase, active-request, or observer flag; the ledger is the only record of what owns the
session, and the TUI receives a flattened view of it.

```rust
Turn { seq, owner: TurnOwner, work: TurnWork }

enum TurnOwner {
    Local  { request_id, prompt, cancelling },       // our session/prompt
    Replay { request_id },                           // our session/load
    Agent  { cause: AgentTurnCause, cancel_sent },   // the agent works without a request of ours
}
enum AgentTurnCause { Detached, Observed, Unprompted }
```

| Owner | Opens when | Ends when | Published phase |
| --- | --- | --- | --- |
| `Local` | A prompt starts | Its prompt response or failure arrives | `Prompting` / `Cancelling` |
| `Replay` | `session/load` starts | Its response arrives | `Loading` |
| `Agent` / `Unprompted` | Agent activity arrives with no turn open | The agent reports `idle`, or goes quiet | `Agent` |
| `Agent` / `Observed` | A Nori `working` status arrives with no turn open | `idle`, or a long silence | `Agent` |
| `Agent` / `Detached` | Activity continues within ten seconds after a local request ended | `idle`, or the agent goes quiet | `Agent` |

An `Agent` turn is how Nori says what is true when the agent works without a request of
ours: a prompt response that arrived while the agent kept working, work another frontend
started, a request Nori had to finalize itself, background work. It is reported as a phase
change (`SessionPhase::Agent { cause, cancelling }`) and rendered as running, never
warned about and never dropped. A `Detached` turn after a forced cancel inherits that the
user had cancelled.

`TurnWork` is what the turn accounts for: the message being assembled, every tool call it
saw with the last status the agent reported, the permission and question requests the
agent still waits on, a count of agent activity, and whether the next queued prompt starts
the moment the turn ends (steering). Session state that outlives turns, such as the
advertised commands, lives on the runtime.

Nori serializes inbound ACP updates, request outcomes, timers and user actions through the
session reducer. A result keyed by a wire request ID can only end the local turn that
request owns. A late response, a duplicated response, or the response of a superseded
prompt owns nothing and is inert; so is a timer for a turn that has already ended.

### Permissions and questions in every turn

A permission or question request is valid in every kind of turn, including a `session/load`
that has not finished: a request made during a load is held, owned by the replay turn in the
ledger, and put to the user as soon as the session has somewhere to ask. The ledger tracks
each request in the turn that owns it, and approval policy or the user answers it exactly as
in a local turn. Nori answers a request as cancelled in only these cases:

- the connection to the agent dies or the user stops the agent;
- the user cancels the turn that owns it, and the agent has not yet shown it ignores the
  cancel (a request already pending is answered at the moment of cancelling; one that arrives
  while the cancel is fresh is declined).

Interrupting the root's turn (Esc, or submitting while a turn runs, which steers by interrupting)
ends that turn only. Delegated actors are independent: their tasks keep running, and their results
and mail are delivered at the next turn boundary as usual. They stop only on an explicit stop
(`task_cancel`, a lane's stop, the second Esc with no turn open, stopping the agent), on session
end or quit, and on a rewind that discards them.

A decline is never silent and never a stream. The first decline of a tool publishes
`RequestDeclined { tool, count: 1 }`; further declines of the same tool within three seconds
are counted, and one more `RequestDeclined` carries the total when the window closes.

### An agent that keeps working

When the agent keeps working after its request ended, or after a cancel it ignored (the
cancel watchdog finalized the request, or an agent turn was abandoned), the next activity opens
an `Agent` turn that inherits the cancel (`cancel_ignored`). Its permission requests are not
declined: declining is what made the agent retry the same edit for hours. The user decides.

Every permission request that arrives in a `Detached` turn, or in a turn whose cancel was
ignored, is preceded by `AgentContinued { agent, request_id, tool, cause, after_cancel }`,
which tells the user the agent continued after its turn ended (and, with `after_cancel`, after
being cancelled). The request itself follows as the ordinary approval prompt. When
`after_cancel` is set clients offer a third choice besides allow and deny: stop the agent.

Stopping the agent (`Harness::stop_agent`) tears down its connection. The open turn ends with
`ConnectionLost` semantics: its requests are answered as cancelled, its unfinished tool
calls are closed, the local request fails, delegated tasks stop, and the session ends as
`ConnectionLost`, so it can be resumed from its journal.

### How turns end, and what ending closes

When any turn ends, whatever the owner and the reason:

- The open message is closed and the turn's last complete answer is recorded.
- Every tool call the turn started and the agent never finished is closed with a failed
  `tool_call_update` published to every client and the journal, marked
  `_meta.nori.interrupted` with the reason (`cancelled`, `failed`, `turn_ended`, or
  `connection_lost`). An interrupted operation never leaves a spinner running. The same
  closing happens the moment the user cancels.
- The idle phase is published, and queued prompts start according to the rules below.

### Message boundaries

`nori_protocol::message_flow` is the single definition of a message boundary. A message is a
run of consecutive text chunks of one stream with nothing visible between them. A chunk
continues the open message only if it is the same stream and does not name a different message
ID. A tool call or its update, a plan, a permission request, a notice or other Nori event
shown in the transcript, a warning carried in the text stream (the text before it is still the
agent's, the warning never becomes part of the message), or the end of the turn closes it.
Session facts such as usage, mode, and configuration changes are transparent: they never
split a message, and neither does the activity of other actors (a delegated child's updates,
delivered task results), which arrives while the parent's answer is still streaming and is shown
after it: a boundary is something in this actor's own timeline. The turn's assembly and the journal's response-boundary markers both use
this function, so two messages, or a message and a notice, are never concatenated, and the
two can never disagree about where a message ends.

A tool-call update marked `_meta.nori.interrupted` is the session's bookkeeping for a call it
closed. It is transparent to messages and is never agent activity: it cannot open a turn, count
toward quiescence, or start a presentation turn (`nori_protocol::is_interruption_close`).

### Agent turns end by status or quiescence

An `Agent` turn has no response to end it. It ends when:

1. the agent reports a Nori `idle` status, or
2. it is quiescent: every tool call it started is terminal, no request of the agent is
   unanswered, and there has been no activity for the quiet window (three seconds; thirty
   for a turn the agent declared with `working`). The reducer asks the driver for a
   timer wake-up. New activity makes the wake stale and the reducer re-arms it. Requests
   withdrawn without an answer stop counting, and a user's answer counts as activity.

When an agent turn ends and it was not cancelled, the next queued prompt starts. Session
idle hooks (delegated results, CI) run at that boundary exactly as after a prompt, and an
uninterrupted agent turn continues an active goal whose agent can complete it.

## Prompts, queues, and cancellation

A prompt submitted while no turn is open starts immediately. A prompt submitted while any
turn is open, including an `Agent` turn, is added to the in-memory FIFO queue and waits
for that turn to end. When a prompt ends with `end_turn`, or an agent turn ends without
having been cancelled, the reducer starts the next queued prompt. Other stop reasons end a
prompt without draining the queue. A transport failure ends the active prompt, reports a
cancelled stop reason with its failure details, and does not drain the queue.

In the composer, Enter submits to this queue. Shift+Enter steers the active turn: Nori
cancels that turn and starts the steering message as soon as it ends, ahead of other
queued messages. If no turn is open, the message starts immediately. Enter with an empty
composer promotes the oldest queued message by cancelling the active turn and starting
that message next. Alt+Enter inserts a newline. Queued and steering messages retain their
attached images and resources.

Cancelling marks the turn as cancelling, answers its outstanding requests as cancelled,
closes its unfinished tool calls, and sends `session/cancel`. It does not make the session
idle. Cancelling again sends `session/cancel` again and nothing else; this is how the user
presses through an agent that ignored the first one.

- A local request waits for its prompt response or failure. If the agent has not answered
  within ten seconds, Nori aborts the wait and ends the request with an explicit failure
  (`RequestFailed`, not a cancellation the agent never confirmed). If the agent keeps
  working, its next activity opens `Agent { cause: Detached, cancelling: true }`.
- An agent turn that ignores the cancel for ten seconds is abandoned: Nori stops tracking
  it, says so, and if the agent is still working its next activity opens a new
  cancelling `Agent` turn.

### Who may start a prompt

The ledger is the only arbiter. Every prompt that starts does so in the reducer, which starts
one only when no turn of any owner is open (and, except for the queue draining itself, nothing
is queued) and no session swap is in flight.

| Source | Rule |
| --- | --- |
| Typed by the user | Starts if the ledger is idle, otherwise waits in the queue (Enter) or steers (Shift+Enter) |
| Queued prompts | Start when a turn ends cleanly: a local request with `end_turn`, an uncancelled agent turn, a steered cancel |
| Retries and same-agent model swaps | Stay inside the local turn that owns the failed request; the failed attempt's unfinished tool calls are closed first, and nothing else can start during the backoff |
| Goal continuations, workflow and CI hooks, delegated results, peer mail | Offered with `offer_prompt`: they start only if the ledger is idle at that instant and their conversation generation is current. A refused offer keeps what it owed (the lease, the inbox, the verdict) for the next turn boundary; it is never queued behind another turn |
| Session swaps (`/fork`, rewind) | Fence the ledger: refused if anything is open or queued, and no prompt starts until the swap ends; prompts that arrive wait in the queue |

A prompt submitted by a remote controller has a stricter path: it is rejected as busy
instead of being queued when any turn is open. A request-scoped cancel from a remote
controller only reaches the local turn that request owns.

Provider-owned child sessions maintain independent FIFO queues, including nested
children. Steering interrupts the focused child and places the steering message
ahead of its queue; promotion interrupts that child and starts its oldest queued
message next. Cancellation is sent to the focused child session. Native child
work and its queued messages prevent conversation rewind until they finish.
Each provider-owned child has a separate conversation journal. Its history and
response boundaries are scoped to that child, including typed content and tool
updates. A child rewind stages a separate agent connection and tool context from
the selected visible prefix, then keeps the same public child identity after
commit. The parent and sibling sessions keep their own histories and connections.
OpenCode child sessions are controllable as soon as they are announced: follow-up
prompts, cancellation, and configuration use the child's own session and do not
require a preceding `session/load`. Its model options are published on the child
session; loading remains a separate replay operation. OpenCode announces each
child on its immediate parent's session and sends its text, tool, and config
updates on the child's own session. A tool completing does not finish the child;
only the child's execution lifecycle changes its state.

For the OpenCode v2 engine, Nori enables agent delegation and sets its
nesting depth to 9,007,199,254,740,991 when those settings have no explicit
configuration. This is the engine's finite JavaScript integer ceiling; Nori
adds no separate nesting budget. Explicit depth and delegation rules are kept.
The adapter reads the engine's resolved configuration and, when defaults are
needed, restarts the background engine with an in-memory configuration overlay.

When a Codex or OpenCode agent asks a question, Nori presents it on the originating
session, including child sessions, and returns the selected or typed answers to
the agent. Dismissing the question cancels its pending interaction. Automatic
permission approval does not answer interactive questions.

`session/load` opens a `Replay` turn that owns replay updates until its response arrives. A
load request received while a turn is open is rejected with a warning. Loading history is not
reported as a completed prompt turn.

## Compaction, rewind, fork, and resume

Every operation that replaces the agent's session (summarize-and-swap `/compact`, `/fork`,
rewind, replacing the agent after a failure) runs with the turn ledger fenced, and
`swap_active_session` refuses to run without the fence. While the fence is up no turn is open
and no prompt of any kind starts: user prompts and steering wait in the queue, and offers
(goal, hooks, delegated results, mail) are refused and kept by their senders. Releasing the
fence starts the front of the queue on the session that results, and runs the turn-boundary
work (delegated results, mail, CI) that was refused meanwhile.

- **Summarize-and-swap.** The summary turn ends holding the fence, so nothing queued starts
  before the compaction has decided. A compaction either swaps completely or leaves the old
  session exactly as it was and says why: the summary turn failed or was cancelled, the agent
  did not write the template (it is reminded once, ahead of the queue), the agent started working
  again so a swap would orphan its turn and the requests it waits on, or no new session could be
  started. The checkpoint is applied when a prompt is sent, not when it is submitted, so the
  first prompt to reach the new session carries it whether it was typed before or after the
  swap. A prompt that fails before the agent takes it gives the checkpoint back. Native compact
  is an ordinary turn; its divider appears only if the agent finished.
- **Rewind and fork.** Asking for one while a turn is open (a prompt, an agent turn, a retry
  waiting out its backoff) cancels that turn cleanly and waits for the ledger to see it end;
  it does not swap in the middle of a turn, and it does not begin if the agent will not stop
  within 25 seconds. Queued prompts survive and start on the rewound or forked session.
- **Resume.** The ledger of a resumed session starts idle. A load's permission requests are
  held for the user. A journal that stops mid-turn replays with its unfinished tool calls
  closed as interrupted (`connection_lost`), unanswered permission requests are not replayed
  as live prompts, and a final user prompt the agent never answered is offered back as
  `PromptUnanswered` rather than lost.
- **Journal.** The journal records every public event: turn ends (idle phase, prompt
  response), interruption closes, declines, route changes, `AgentContinued`, stopping the
  agent. A replay reproduces what the user saw: interruption closes, declines, route changes
  (as notices, so replay never changes the present route), and a stopped agent's notice. It
  uses the same message boundaries as the live turn.

### Typed-ahead prompts survive the session

Every change to the queue of prompts the user typed ahead is one small `queue` command in the
conversation journal, recorded before its effect runs: `enqueue` (at the back, or at the front
when steering), `start`, and `remove`. Hidden continuations are not journaled; the resumed session
decides those afresh. When a session is resumed, folding the journal in order gives what was
queued when it ended (after a crash, or after a quit with prompts still waiting). Those prompts
come back **held**, not queued: nothing starts them. The session publishes `QueueRestored`
listing them by id; the user sends the oldest (the ordinary "send next queued" action promotes
it into the queue and starts it) or drops one (`Harness::drop_queued`). Journals without `queue`
commands fold to nothing, so older transcripts load unchanged.

### Events are scoped to their session

A swap retires the session it replaces, on a connection the agent still holds. The relay, the
boundary where inbound events enter the ledger, scopes each event by its ACP session ID. A
notification of a retired session is dropped together with the reducer input that follows it,
and logged once per session; a permission request of a retired session is declined explicitly
and the user is told once. Permission requests the retired session still held at the swap are
declined with one notice. Only retired sessions are refused: the first updates of the new
session may arrive before the swap has installed its ID.

## Recording and replay

The append-only Pkl journal is the canonical conversation history. The harness
records a command before attempting its effect; a journal write failure stops
that command from running. Conversation programs record their source before they run
and their outcome afterwards; every step is journaled in the run's own journal. Received ACP
and Nori session events are journaled before context observation and live
reduction. Replay reconstructs state from recorded data and does not evaluate
conversation programs or repeat external effects.

Journal record IDs remain stable and increasing within a session. A rewind
changes the visible projection but retains the earlier records and their IDs.
Fork-source records identify an inclusive source range by session ID and record
IDs, so that range remains addressable even when a later rewind hides it. A
forked conversation records its lineage in its own session journal.

Every delegated child session has its own journal and session identity. The
parent journal also records child events relayed to the parent. When Nori
replaces a provider during a handoff, the replacement uses the same recorder
and Nori conversation identity; the journal records the handoff and the new
provider binding. The companion JSONL transcript remains a compatibility and
display view and does not represent rewind projection or the latest provider
binding by itself.

## Terminal program status

The interactive Nori ACP client reports its active conversation through the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
(OSC 7501). It reports the root turn and each announced child session as separate
records, retaining the parent-child hierarchy. Terminals that do not implement the
protocol ignore the reports. Permission and sign-in requests are reported as
blocked; cancellation returns to idle. Permission requests use `kind=permission`,
and sign-in flows use `kind=auth`. Interactive questions transported by
`_nori/request_user_input` use `kind=question` on the originating root or child
record. Answering or withdrawing the exact request clears its blocked state;
tool names and text alone never imply a question.
Nori clears a child record and its descendants when the agent reports that the
child session closed; cancellation of a turn instead leaves the child idle. It
clears all records when the root session closes or the client exits. Reports do
not contain percentage estimates.

Prompt results and cancellation watchdogs retain the ACP wire request ID that owns them. A
late response or timeout from an earlier turn cannot complete the current turn or abort its
task. Child promotion and steering keep their queued content on the same child while applying
the same turn ownership rules.
