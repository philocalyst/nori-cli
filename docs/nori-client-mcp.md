# nori-client MCP Behavior Spec

This document specifies the desired behavior for the backend-owned
`nori-client` MCP server. It replaces the old append-only
`CURRENT-PROGRESS.md` log, which mixed completed implementation history with
obsolete investigation notes.

## Current State

Nori owns `/goal` state in the ACP backend. Capable ACP agents also receive a
backend-owned MCP server named `nori-client`, served as streamable HTTP on
`127.0.0.1`, so they can call `get_goal`, `create_goal`, and `update_goal`
against the same state the TUI uses.

Agents that advertise Nori host-tool support get these tools directly instead
of through the server; see [host-tools.md](host-tools.md). The in-process
OpenCode adapter uses the `nori-client` MCP server because OpenCode's engine
runs as a separate local process. On its shared service, each session registers
its own namespaced MCP servers with session permissions that exclude other
sessions' registrations. The adapter refreshes these connections before a turn
and removes its registrations when the session closes.
Everything below about goals, capabilities and the connected gate applies to
them too, with "initialized `nori-client`" read as "has the session".

Nori's built-in Codex launch disables Codex-native goals through the ACP
adapter configuration. For that launch, only `nori-client` controls Nori thread
goals; similarly named native or unqualified tools must not be used for this
state.

The server is intentionally general-purpose: goal tools are the first tenant,
not the whole API. It should remain the narrow harness-side channel for
capabilities that ACP does not yet provide directly.

The `conversation` tool runs a Pkl program (a module that amends `nori:Program`)
through the same runtime as `HarnessHandle`. The program's source and outcome are
journaled in the conversation, and each run keeps its own journal and snapshot
under `.nori/runs/`. See [programs.md](programs.md) for the schema, recovery and
replay guarantees. The read-only `conversation_context` tool returns bounded pages
of canonical context with event IDs, visible bounds and the current head so an
agent can select history for Pkl context, forks and rewinds.

OpenCode calls carry the invoking provider session in transport metadata
`_meta["ai.opencode/sessionID"]`. Nori validates it against this backend's root
binding and observed native child ancestry; unknown or foreign IDs fail rather
than inheriting root authority. It briefly waits for a racing child announcement.
Native callers share the root's user control plane, but keep their own task
ownership, delegation depth, idempotency keys, journal and completion destination.
A native child's background result queues a prompt to that child even while the
root is busy. Direct host calls carrying a session ID use the same validation.
Native child `conversation_context` reads that child's journal. Root goal tools
and mutating Pkl `conversation` programs currently return an explicit unsupported
caller-scope error for native children; they never operate on the root implicitly.

## Delegation Tools: Actors, Tasks and Messages

The task tools (`agent_capabilities`, `delegate_task`, `task_send`, `task_status`,
`task_wait`, `task_cancel`, `task_list`) operate on the tree-wide **actor directory**
(see the Delegation section of [glossary.md](glossary.md)); their agent-facing text is
`nori_host_tools::DELEGATION_INSTRUCTIONS` and the tool descriptions in
`harness/src/backend/orchestrator/tools.rs`.

- One resolver accepts `task-N` (a task the caller started), an alias, `actor://alias`,
  `actor://<session-id>` and a raw session ID, for every tool, for conversation-program `send`,
  and for the TUI's subagent controls. An address that names nothing returns an error listing the
  addressable actors (alias, agent, state), the caller's tasks and the accepted forms. Task IDs
  are private to the actor that started them; a provider-owned caller (an OpenCode child) sees
  only its own tasks but can message any actor.
- `task_send` queues a message for an actor and returns the same actor and task, never a new
  child. Mail queued for the root session is delivered in a hidden turn only when no turn of any
  owner is open, and a rewind waits until it is delivered. An idle actor takes it now, a working one at its next turn boundary, `steer: true`
  interrupts the current turn first, and an ended actor is revived with its history (`outcome:
  "revived"`). A provider's own child that ended is revived as a managed actor on the same
  agent with the same session ID, seeded with its recorded history; when that history or its
  configuration was never recorded, the error says so. Of several senders that find an ended
  actor at once, exactly one restarts it. A message to the actor's parent-delegated child is a task whose answer returns to
  the sender; a message to any other actor is mail that the recipient answers with its own
  `task_send`.
- `delegate_task` with an `alias` that already names an actor sends it the task (`outcome:
  "reused"`) instead of starting a duplicate. It reports success only once the child's session
  is open; a child that cannot start is an error with the cause. A turn that ends with no text and
  no tool calls fails with an explanation instead of reporting success.
- The addressed actor is always `target` (`task_send`, `task_status`, `task_cancel`, and
  `targets` for `task_wait`); `to`, `actor`, `recipient`, `task_id` and `id` are accepted
  silently and never shown in the schema.
- A parent can read the mail between its descendants without being woken: `task_list` shows
  `mail` (the recent messages with delivery states) and `mail_open` (how many are not
  acknowledged yet; zero means the exchanges are over), and `task_status` on an actor shows the
  mail involving it. `task_send` with `cc_parent: true` also tells the sender's parent, by one
  mail from the actor whose turn ended last, when the exchange between the two settles (no mail
  waiting, neither has a turn open); replies in the exchange keep the setting.
- Every message has a delivery state (`queued`, `delivered`, `acknowledged`) shown in
  `task_status`, `task_list` and in the UI's peer-message cards; `queued` is never described as
  received.
- Results and replies arrive as new turns. `task_wait` returns at once for a task that nothing
  returns from (a peer message) or when a message for the caller is waiting, and messages queued
  for the caller are delivered inline in any task tool result (`inbox`) so a reply never sits
  behind a blocking call. Repeated unchanged `task_status`/`task_wait` answers tell the caller to
  stop polling and end its turn.
- Interrupting the root's turn never stops its children: tasks run on and deliver at the next
  boundary. They stop on `task_cancel`, an explicit stop, session end, or a discarding rewind.
- `bash` reports a non-zero exit code as a tool error (`isError`), with the full output in the
  result.

### Compaction and resume

The directory is tree-wide and outlives any one session. When the root is compacted or replaced
it is the same root actor under a new session ID, and the checkpoint handed to the new session
carries a roster in `<subagent-continuity>`: each actor's address, session, agent and model,
state (with the failure cause when it failed), the headline of its last result, its outstanding
tasks and pinned facts, plus every message still waiting in a mailbox. A child compacting
changes what the child remembers, never its identity, alias or state as its parent sees them.

The directory is journaled event-sourced in the root conversation's journal: each change (an
actor registered, its session started or ended, one task enqueued or changed, a digest pinned)
is one small `actor-delta` command, and every 200 deltas, at root compaction and at session end
the whole tree is written as one `actor-snapshot`. A restore folds the latest snapshot and the
deltas after it, in journal order; rewound records are ignored. A delta is the size of one task,
so the journal grows linearly with activity. Only the stored text of a result is capped (64 KiB);
the full text stays in the actor's own journal, which its record names.

On resume the actors come back **ended and dormant**: nothing is spawned, none shows as running,
and a turn that was in flight when the old run ended is reported as failed with that cause. The
first message to an actor revives it with its history, after delivering the mail still queued for
it in order; messages already delivered, and results already read, are never delivered again, and
mail queued for the root session is delivered once it is idle. Restored grandchildren are owned
by the root's orchestrator until their parent is revived; the parent's new orchestrator then
takes over its dormant children. Transcripts without these records load as before.

The root session's own presence in `task_list` follows its turn ledger: working while any turn
is open, whoever owns it. A child's request that was declined after a cancel, or a child that
kept working after one, is announced to the user under that child's alias and agent.

## Monitor Tools

`monitor_start`, `monitor_list`, `monitor_stop` and `monitor_rearm` arm and manage background
watchers whose events wake the agent instead of being polled for. They are served like every other
`nori-client` tool, with the same sandbox and tool selection, and their guidance
(`MONITOR_INSTRUCTIONS`) travels with the delegation instructions. A provider's own child session may
not call them. See [monitors.md](monitors.md).

## Target Shape

`nori-client` should be the single structured way an MCP-capable ACP agent
learns about Nori-owned client context. The prompt stream should carry user
work, goal context when goal automation is valid, and a one-time source envelope
that identifies Nori CLI consistently across client surfaces. Only agents
without HTTP MCP support should receive fallback operating guidance in that
envelope. MCP-capable agents should not receive product explanation they can
discover through `nori-client`.

The boundary is:

- MCP tools mutate or read Nori-owned live state.
- MCP resources expose durable read-only facts.
- MCP prompts package reusable workflows for the agent.
- Every agent receives first-prompt source attribution; prompt fallback is
  reserved for agents that cannot receive the MCP server.

## MCP-Capable Agent Behavior

When the active ACP agent advertises HTTP MCP support, Nori should advertise one
loopback streamable-HTTP MCP server named `nori-client`.

The server should expose:

- Goal tools:
  - `get_goal`
  - `create_goal`
  - `update_goal`
- Context resources:
  - `nori://context/cli` - three concise operating facts: Nori CLI is the
    current harness, ACP is the JSON-RPC wire protocol backed by the Nori CLI
    source repo, and `nori-client` is an internal-only backend MCP server.
  - `nori://context/repo` - a compact source map for answering Nori CLI
    implementation questions from the repo source.
- Help resources and prompts:
  - `nori://help/custom-acp-agent` and `register_custom_acp_agent`
  - `nori://help/acp-wire-logs` and `debug_acp_wire_protocol`
  - `answer_nori_cli_question`, which points at `nori://context/repo`

The agent should be able to discover this context through MCP list/read/get
requests. Nori may include short MCP server instructions that point agents
toward the resources and prompts. Its first ordinary user prompt still receives
the shared source envelope. When active goal context is present, that envelope
also states that Nori CLI owns the goal and routes reads and updates to Nori's
goal tools:

```text
<context>
Source: this message is from Nori CLI.

When <goal_context> is present, Nori CLI owns the goal. Nori delivers changed goal context automatically when supported; use `get_goal` only if you need to inspect the current state, and use `update_goal` to change its status.
</context>
```

This source attribution and conditional goal-routing guidance are distinct from
the static Nori operating guidance that MCP-capable agents can discover through
`nori-client`, and the envelope is consumed once rather than repeated on later
prompts.

For a Nori-managed goal, Nori includes a goal revision and the latest state when the state has
changed since the conversation's previous delivered goal context. A cleared
goal is reported once if the agent previously received that goal. Usage-only
updates do not create a new goal revision. A reopened or forked conversation
receives its current state again; an initially empty goal adds no context.
For an in-process Codex root session, a pending change can also accompany the
next Nori host-tool result. Provider-owned tool results do not yet carry these
updates; they remain pending until a supported delivery point.

Resources and prompts should be curated guidance, not an arbitrary filesystem
read API. Tools should remain reserved for Nori-owned state changes.

## Non-MCP Agent Behavior

When the active ACP agent does not advertise HTTP MCP support, Nori should not
advertise `nori-client`.

Non-MCP agents should receive a concise first-prompt-only `<context>` block that:

- identifies the source as Nori CLI, matching the MCP-capable envelope,
- says the agent is operating over ACP,
- includes `https://github.com/tilework-tech/nori-cli` as the stable source
  reference for implementation questions,
- says MCP-backed Nori affordances are unavailable in this session, and
- names `/goal` completion tools as unavailable rather than implying the agent
  can close the loop through `update_goal`.

That fallback block must be consumed once and must not be repeated on later user
prompts.

## Goal Behavior

`/goal` requires a close-the-loop path: the agent must be able to call the
backend-owned goal tools to mark work complete or blocked. When `nori-client` is
not available, goal automation should be unavailable as behavior, not merely
dimmed as UI — unless the agent advertises the `_session/goal` extension
described below.

When the requested work is verified complete, the agent must call Nori's
`update_goal` with the exact status `complete` and verify the returned status.
A genuine impasse uses the exact status `blocked`. The instruction text names
the tools without a server, because a built-in agent reaches them as host
tools.

## Goal Extension Bridge (`_session/goal`)

Agents may advertise a goal capability in the top-level `_meta` of the
initialize response (`goal: {version: 1, controlMethod: "_session/goal",
actions: [...]}`; `set` and `clear` are the required floor). When advertised,
the harness prefers driving the goal through that extension: `/goal` sends
`_session/goal` requests, the agent's native goal loop owns continuation, and
the harness mirrors the goal snapshots the agent publishes on
`session_info_update` `_meta.goal` into its own goal store and `GoalChanged`
events. While the extension drives a goal:

- the harness goal-continuation loop is suppressed,
- the `<goal_context>`/`<goal_control>` prompt injection is skipped (the agent
  runtime owns goal context), and
- `/goal pause`/`resume` are honored only when the capability advertises those
  actions; otherwise they fail with an explicit error rather than silently
  diverging from the native loop.

If an extension request fails and `nori-client` is available, the harness falls
back to the MCP goal loop for that goal. Agents advertising the extension but
not HTTP MCP get `/goal` through the extension alone. The end-to-end contract
is exercised in `nori-rs/harness/tests/goal_ext_bridge.rs` against the mock
agent (`MOCK_AGENT_GOAL_EXT`, `MOCK_AGENT_GOAL_EXT_AUTOCOMPLETE`).

Required behavior:

- The TUI keeps `/goal` visible but disabled with a clear reason.
- Direct typed `/goal ...` submissions are rejected by the TUI.
- Backend `ThreadGoal*` operations from user-facing paths are also rejected or
  made inert when HTTP MCP is unavailable.
- Replayed active goals should not inject `<goal_context>` or submit hidden goal
  continuations into a non-MCP session. The stored goal may remain visible as
  historical state, but it must not drive autonomous work the agent cannot
  complete.
- A resumed non-MCP session with an existing active goal should emit a clear
  user-visible notice explaining that goal automation is unavailable for the
  active agent.

MCP-capable agents may still delay hidden continuation chaining until the agent
actually initializes `nori-client`; advertising the server and connecting to it
are different states.

## Capability Projection

`SessionCapabilitiesChanged` should remain a snapshot of the current client
state, not a collection of feature-specific one-off events.

The projection should eventually distinguish:

- raw ACP capabilities such as HTTP MCP and `session/load`,
- whether `nori-client` was advertised,
- whether the active agent has initialized `nori-client`, and
- derived builtin command availability such as `/goal`.

Nori should re-emit the capability snapshot whenever a new ACP session is
created or loaded, including compact-created replacement sessions. The TUI
should render command availability from the derived builtin command map rather
than inferring it from raw ACP details.

## Server Ownership And Safety

`nori-client` is a reserved MCP server name. User-configured MCP servers should
not be allowed to shadow or duplicate that name.

Before the server surface grows beyond goal tools, it should use a per-session
loopback authentication mechanism. The expected shape is a generated bearer
token advertised in the ACP MCP server config headers and verified before
requests reach the streamable HTTP MCP service.

The server should stay loopback-only and abort when the owning backend session
drops.

## Verification

The behavior is correct when tests prove:

- HTTP-MCP-capable agents receive a `nori-client` server and can initialize it.
- MCP clients can list and read the Nori context resources.
- MCP clients can list and get the Nori workflow prompts.
- non-MCP agents do not receive `nori-client`.
- every agent's first prompt receives the Nori CLI source envelope exactly once.
- non-MCP first prompts additionally receive fallback guidance inside that
  envelope.
- MCP-capable first prompts do not receive fallback guidance once the MCP
  context surface exists.
- active goal prompts direct completion to `update_goal` from the `nori-client`
  MCP server with exact status `complete`.
- `/goal` is disabled in the TUI and inert in the backend when `nori-client` is
  unavailable.
- replayed active goals do not inject goal context or hidden continuations into
  non-MCP sessions.
- capability state is refreshed after spawn, resume, and compact-created session
  replacement.

## Non-Goals

This surface should not become:

- a general local file reader,
- a second store for goal state,
- a replacement for ACP capabilities that ACP already provides,
- a way for agents to mutate user configuration without explicit user action, or
- a broad tool bucket for anything that happens to be convenient to expose over
  MCP.

## Superseded Notes

The old `nori-goal` over `acp:<uuid>` failure is no longer the current
architecture. Nori now advertises a real loopback HTTP MCP endpoint named
`nori-client`, served by `rmcp`'s streamable HTTP server. References to
`nori-goal`, ACP pseudo-URLs, or hand-rolled MCP framing should be treated as
historical notes, not active work.
