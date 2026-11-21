# Fake worlds

A **world** is a declarative script, in Pkl or JSON, for one or more fake ACP agents. It says what
each agent does for each prompt: the text it streams, the tools it calls, the permissions it asks,
the usage it reports, the children it delegates to, the errors it fails with, and the faults real
agents commit. Nothing runs against a network or a provider, and the same world plays the same way
every time, so one script serves an integration test, a live demo, and a terminal capture.

| Piece | Where | Role |
| --- | --- | --- |
| `nori-fake-agent` | `nori-rs/fake-agent` | The fake agent, the world model, the Pkl schema `nori:World`, the virtual clock, the wire tap |
| `nori-world` | `nori-rs/world` | The driver: boots the real harness against a world, acts as the user, records and queries what happened |
| Showcase worlds | `nori-rs/fake-agent/worlds/` | Worlds for UI design, demos and captures |
| `scripts/rex-scenario` | `scripts/` | Opens a world in a Rex pane or tab (see [tui-captures.md](tui-captures.md)) |

The fake agents are built on `nori-acp-kit` ([native-agents.md](native-agents.md)), so they are
ordinary in-process agents: the host starts them on an in-memory pipe, with no subprocess, and gives
them Nori's host tools.

## A world

```pkl
amends "nori:World"

name = "hello"
default_agent = "main"          // required when there are several agents

agents {
  ["main"] {                    // the key is the agent's slug
    models { new Model { id = "fake-1"; context_window = 200000 } }
    scenes {
      new Scene {                // the first scene whose `on` and `model` match answers a prompt
        on = new Matching { pattern = "(?i)trim" }
        steps {
          new Say { text = "On it. You asked: {{prompt}}" }
          new Tool { title = "Read src/lib.rs"; tool_kind = "read"; duration = 200.ms }
        }
      }
      new Scene { steps { new Say { text = "Anything else." } } }
    }
  }
}
```

The schema is `nori-rs/fake-agent/schema/World.pkl` and the serde types in
`nori-rs/fake-agent/src/world/` are the same shape: Pkl evaluates a script to JSON and serde reads
it, so a hand-written Pkl world and a generated JSON fixture are the same thing. A world is validated
before anything runs, and every problem names the agent, scene and step it is in. Unknown fields are
errors.

Pkl gives a world loops (`for (i in IntSeq(1, 36)) { ... }`), conditions (`when (i % 6 == 0) { ... }`)
and arithmetic on durations, so a thirty-minute script with climbing usage is a dozen lines. Steps are
typed (`Listing<AnyStep>`, where `AnyStep` is `Step|Tool|Fail`), so a misplaced value is an error
where it is written. A world can import modules of its own: a library of agents, scenes and step
functions is an ordinary Pkl module that imports `nori:World`:

```pkl
// lib.pkl
module lib
import "nori:World" as W
function readFile(path: String): W.Tool =
  new W.Tool { title = "Read \(path)"; tool_kind = "read"; locations { new W.Location { path = path } } }
reviewer: W.Agent = new W.Agent { models { new W.Model { id = "fake-sonnet" } }; turn_policy = "replace" }

// world.pkl
amends "nori:World"
import "lib.pkl"
agents { ["review"] = (lib.reviewer) { scenes { new Scene { steps { lib.readFile("a.rs") } } } } }
```

### `nori:Kit`

`import "nori:Kit"` gives agents shaped like the real ones on the wire, to amend with a script, and
short helpers for the steps every world writes (`nori-rs/fake-agent/schema/Kit.pkl`):

| Kit | Is |
| --- | --- |
| `claudeLike`, `codexLike`, `opencodeLike` | Agents: Claude's `replace` turns, late model list, permission modes and commands; Codex's `steerable` turns, efforts and service tiers; OpenCode's gateway models |
| `say`, `stream`, `think` | Text in one chunk, or streamed some words at a time |
| `readFile`, `edit`, `create`, `shell`, `search`, `askToEdit` | Tool calls with real diffs, output and exit codes; an edit behind a permission request |
| `fail` | A failure Nori's recovery knows |
| `claudeTask`, `codexChild` | Provider subagents with their launcher call and the provider's own spelling |
| `onWake`, `onCommand`, `matching` | Scenes for Nori's wakes, a slash command, a pattern |

```pkl
amends "nori:World"
import "nori:Kit"
name = "kit"
agents {
  ["claude"] = (Kit.claudeLike) {
    scenes { new Scene { steps { Kit.readFile("src/parser.rs"); Kit.shell("cargo test", "ok\n", 0) } } }
  }
}
```

### Agents

An agent declares its slug, title, models (the first is the default), modes, reasoning efforts
(listed in the order given, however unordered), model modes (`model_modes`, offered as the
`service_tier` option, the first being the default), slash commands, capabilities (`load`, `resume`, `fork`, `list`, `close`, `host_tools`, ...), turn policy
(`exclusive`, `steerable`, `replace`), account, and `startup`: delays for `initialize` and
`session/new`, errors for either, and `requires_auth`. The kit refuses requests for any capability the
agent did not declare, so a world cannot claim more than it implements.

`startup` also says how an agent fails to start or to be reopened: `hang_initialize` and
`hang_new_session` (the request is never answered), `load_error`, `resume_error` and `list_error`
(every `session/load`, `session/resume` or `session/list` fails), and `models_announced_after` (the
agent lists no models at `session/new` and announces its `models` that much later in a
`config_option_update`, as Claude does once its CLI has reported them).

`limits` is the provider behind the agent: `max_text_block_bytes` refuses a prompt in which any one
text block is longer, `max_prompt_bytes` one whose text blocks add up to more (about four bytes to a
token). The prompt is refused before any scene runs, with a JSON-RPC internal error whose
`data.detail` is the provider's own body for `provider = "openai"` (`string_above_max_length`,
`context_length_exceeded`) or `"anthropic"` (`request_too_large`, `prompt is too long`), after an
optional `prefix` (Codex words a failed compaction `Error running remote compact task: `). The flight
recorder keeps what every prompt weighed (`PromptReceived.bytes`, `largest_block_bytes`) and each
refusal (`PromptRefused`), so a test can assert that Nori never sent a block over the limit.

`config_faults` are settings that misbehave on `session/set_config_option`. The first fault whose
`option` (and `value`) matches applies: `reject` fails with `error`, `ignore` says yes and keeps the old
value (the agent keeps running the old model), `substitute` says yes and sets `actual`; `delay` is how
long the agent takes. Each is recorded as `ConfigFaulted`.

A world may carry a `policy`: Pkl source for `$NORI_HOME/init.pkl`, the recovery policy
([workflows.md](workflows.md)) a world is about, such as the fallback chain of `quota-fallback.pkl`.
Drivers install it in a home they own; a home that already has an `init.pkl` keeps it.

### Scenes

A scene matches a prompt by `Always`, `Nth { index, per_agent }` (the index counts a session's
prompts, or all of the agent's), `Matching { pattern }` (a regular expression), `Wake { of }` (the
prompts Nori sends by itself: `task_results`, `monitor_events`, or `any`), `SlashCommand { name }`
(`/name` with or without arguments), or a combination (`AllOf`, `AnyOf`, `Not`); and optionally by
the session's current `model` and by how the turn began (`turn`: `started`, `steered` into a
running turn of a `steerable` agent, or `replaced` one of a `replace` agent). A scene with
`times = n` answers at most `n` of its agent's prompts and then leaves them to the next scene that
matches, so "fails the first time, works the second" is two scenes. Matching reads what the person said: Nori's own context
(`<nori_...>` blocks and the `<conversation-checkpoint>` handed to a fallback route) is stripped
first. A prompt no scene matches is answered according to `unscripted` (`echo`, `fail`, `hang`) and
recorded as a divergence.

A scene's `cancel` policy says what the agent does when the user cancels: `honor` (stop and answer
`cancelled`), `ignore` (never notice), or `hang`; how long it takes to notice (`after`); and `then`
steps to run once it has noticed, optionally after answering (`detach_then`).

### Variables

Text may contain `{{prompt}}`, `{{index}}`, `{{session}}`, `{{agent}}`, `{{model}}`, `{{now}}`, the
`{{i}}` of a `Repeat`, and anything a step bound with `bind` (`{{task.task_id}}`). A name that is not
bound stays as written and is recorded as a divergence.

## Steps

| Group | Steps |
| --- | --- |
| Output | `Say`, `Think` (text with `chunk { size, unit, every }` for cadence; `Generated` text for huge outputs), `Wait` |
| Tools | `Tool` (announce, stream `progress`, end, with `Diff` content, `stdout`, `exit_code`, `outcome`), `ToolStart`, `ToolUpdate` |
| Permissions | `Permission`: announce, ask, branch on the answer (`on_allow`, `on_deny`, `on_cancelled`, `on_timeout`), run the tool when allowed; `expect` records a divergence when the answer differs |
| Control | `When` (branch on a value as the step runs: `value` with `matches`, `equals`, or `bound`; `then`, `otherwise`), `Set` (bind a variable for the session's later steps and turns) |
| Session facts | `ConfigUpdate` (the agent announces the models it can run, and switches its own options with `set`, as Claude falling back to another model does), `Commands` (the slash commands it offers from now on), `Plan`, `Usage` (the `_meta.nori` shape of [usage.md](usage.md), with rate limits), `SessionInfo`, `Status` (`working`, `idle`), `Mode` |
| Delegation | `Delegate` (`delegate_task`), `Send` (`task_send`), `HostTool` (any host tool); results bind to variables |
| Ending a turn | `Fail` (structured errors), `Stop` (any stop reason) |
| Composition | `Par` (lanes at once), `Lane` (steps on another session), `Subagent` (a child session the agent reports itself: `wire = "claude"` or `"codex"` spells the announcements as that provider does, `name` is its nickname, a `Launcher` shows the parent's `Task`/`spawn_agent` call, `background` for an async launch), `PeerMessage`, `Repeat` |
| Faults | see below, and the [fault catalog](fault-catalog.md) |

A diff carries the real old and new text, so diffs, edit journals and undo behave as they do for a
real agent. An edit's `raw_input` is derived from its diff when a script does not give one.

`Fail` takes `failure` as one of `quota`, `model_unavailable`, `model_placeholder`, `capacity`,
`rate_limited`, `server_error`, `network`, `auth`, `prompt_too_long`, `session_not_found`,
`not_resumable`, `executable_not_found`, `not_running`, `internal`, or `raw`. `raw` is exactly a
`code`, `message` and `data`, which is how a provider's JSON body arrives inside an internal error
(`data = new Dynamic { detail = "{\"type\":\"error\",...}" }`). Each is written in the fields `classify_failure` reads (`code`, `status`,
`retry_after`, `resetsAt`, `detail`), so the recovery policy never parses prose.

### Faults

Faults are steps like any other, so a world says exactly when they happen. Faults that are about an
agent's behavior run in the interpreter; faults below the ACP library are applied by the **wire tap**,
which sits between the host's pipe and the kit, forwards JSON-RPC lines both ways, and applies what a
script armed.

| Fault | Step | Where |
| --- | --- | --- |
| The prompt response never arrives | `DropResponse` | tap |
| The prompt is answered twice | `RespondTwice` | tap |
| A response for a request id that was answered (`previous`) or never asked (`unknown`) | `StaleResponse` | tap |
| A line that is not JSON-RPC, or an update nobody expects | `WireNoise`, `RawUpdate` | tap, kit connection |
| The connection dies mid-turn; a served process exits with a code | `Crash` | tap |
| `initialize` or `session/new` is slow, or fails | `startup` | tap, agent |
| The agent never answers and ignores `session/cancel` | `Hang` | agent |
| The agent ignores `session/cancel` and finishes | scene `cancel.mode = "ignore"` | agent |
| The agent keeps working after the turn ended, including permission requests | `Detach` | agent |
| The agent keeps asking for permission after being cancelled (the "Carbon" bug) | scene `cancel.detach_then` | agent |
| Updates for two sessions interleave, or for a session that does not exist | `Par` with `Lane` | agent |
| Tool updates out of order: end before start, duplicate start, update after end, contradicting end, never ends | `Tool.disorder`, `ToolUpdate` | agent |
| Huge outputs | `Generated` text | agent |
| A prompt over the provider's text-block or context limit | agent `limits` | agent |
| `session/set_config_option` that is acknowledged and ignored, substituted, or refused | agent `config_faults` | agent |
| `initialize` or `session/new` that is never answered; `session/load`, `resume` or `list` that fail | agent `startup` | tap, agent |
| Models listed only after `session/new` | `startup.models_announced_after`, `ConfigUpdate` | agent |

## Time

Every task that scripts time (a turn, a detached tail, a parallel lane, a delayed response) is a
participant of the world's **clock**.

- **Virtual** (the default when Nori runs the agent in its own process): no timer runs. When every
  participant is asleep the clock jumps to the earliest deadline and wakes that one task alone, so a
  thirty-minute script plays in milliseconds, in the same order every time. Ties break by the order
  the sleeps began.
- **Real** (the default when the agent is served to an external client, and for `nori --world`), or
  scaled (`x30` is thirty times faster): ordinary timers. A world may say how a person should watch it
  with `clock = "x30"`; `NORI_WORLD_CLOCK` overrides it.

Waiting on the outside world (a permission answer, a host tool) does not hold time back after a short
real-time grace, so a question nobody answers still times out. Within the grace, the answer arrives at
the same instant of the world's time, so a prompt answered from a test or a TUI is not raced by the
script's own timeouts. The turn's share of the clock outlasts its handler until the prompt response
has left the wire, so a detached tail cannot overtake the response it follows.

The world's `start` is virtual time zero (a fixed instant by default), so reset times in rate limits
are the same on every run. `resets_in` is relative to the clock when the step runs; `resets_at` is
absolute.

## Processes

A world's `processes` say what its machine runs when a job starts (`job_start`, a background `bash`).
Without any, jobs are real: they spawn on this computer (`~/.nori/jobs/<id>`), as they do outside a
world. With any, **every** job of the session runs on them, and nothing real is spawned; the
supervisor, the registry, the journal, the wakes (`job` monitors) and the tools are Nori's own.

```pkl
processes {
  new Process {
    command = "^boot-service"             // a regular expression over the job's command; first match runs
    output {
      new Line { text = "booting" }                        // printed at once
      new Line { after = 1.s; text = "READY" }             // a second after the previous line
      new Line { text = "warning: slow disk"; stream = "stderr" }
    }
    exit = 3                              // the exit code, `linger` after the last line
    linger = 1.s                          // default 0.s
  }
  new Process { command = "^serve-forever"; exit = null }  // runs until it is stopped
}
```

`exit` defaults to 0; `null` never exits. A command no process matches does not start: `job_start`
fails with `this world runs no process for "<command>": add one to its processes`, and nothing is
listed. A process keeps what it printed with byte cursors like a real job's log files, so `job_read`
returns each line once and `more` when a `max_bytes` left output behind. Any signal, including the
`job_stop` an agent asks for, ends it as `killed by <SIGNAL>`; a job adopted after a resume is `lost`,
since a scripted process does not outlive its world.

Time: the monitors that follow a job batch its lines for a few hundred milliseconds on Nori's own
timers, which run in real time whatever the world's clock is. A virtual clock would jump over every gap
the moment the agents are idle and play a whole process inside one batch, so processes wait in real
time: at their script's speed in a virtual world, scaled by the speed of a `real` or `xN` clock. A line
with no `after` is printed without waiting, so a flood of thousands of lines takes milliseconds.
`jobs.pkl` is the showcase; its faults are in the [fault catalog](fault-catalog.md#background-jobs).

## Registration

`nori_fake_agent::registration()` registers the agent `fake`. A world's agents are registry entries of
`native = { agent = "fake" }` with options `world=<file or mem:N>`, `agent=<slug>` (the world's default
when absent) and `clock=virtual|real|xN`. Unknown options are errors.

```sh
# the TUI against a world; nothing real is offered, probed or warmed
NORI_HOME=$(mktemp -d) nori --world nori-rs/fake-agent/worlds/nested-subagents.pkl
NORI_WORLD_CLOCK=virtual nori --world nori-rs/fake-agent/worlds/long-running.pkl   # skip to the end

# one fake agent over stdio, for any ACP client or a PTY test
nori agent serve fake --world nori-rs/fake-agent/worlds/carbon-replay.pkl
nori agent serve fake world=worlds/quota-fallback.pkl agent=fake-codex clock=real

# a Rex pane or tab, for captures
scripts/rex-scenario nori-rs/fake-agent/worlds/comprehensive-chat.pkl --session "$REX_SESSION" --tab
```

`nori --world` lists only the world's agents and starts on its default agent. Its policy is installed as
`$NORI_HOME/init.pkl` when `NORI_HOME` is set and the file does not exist; a person's own home is never
written. A served agent is outside Nori's harness, so it has no host tools: a `Delegate`, `Send` or
`HostTool` step there records a divergence and shows a failed card.

## The driver

`nori-world` is a crate of its own, not a feature of `nori-harness`: the harness stays free of test
support, the fake agent stays free of the harness, and a test in any crate can depend on it.

```rust
let world = World::from_path("nori-rs/fake-agent/worlds/nested-subagents.pkl".as_ref())?;
let session = WorldSession::boot(world, WorldOptions::default()).await?;
let events = session.ask("audit the auth and config code").await?;
assert!(events.root().text().contains("reviewer"));
assert_eq!(events.subagents().len(), 4);
session.finish().await?.is_clean()?;
```

`WorldSession::boot` runs the real `AcpBackend`, orchestrator, approvals and transcripts, in a temporary
`NORI_HOME`, on the world's default agent. The agents of every live world form the agent registry;
worlds that share a slug run one at a time, others run side by side.

- **Acting as the user**: `submit`, `ask`, `steer`, `cancel`, `stop_agent`, `approve`, `deny`, `answer`,
  `rewind`, `resume`, `finish`. `Approvals` chooses who answers permission requests: `Manual` (the test),
  `AllowAll` (Nori's `approval_policy = never`), `DenyAll`, or a `Script` per request.
- **Waiting**: `settle` waits until the turn is over, no child runs, and nothing happened for the quiet
  moment (`WorldOptions::quiet`, one second by default: longer than the harness's completion-wake batching). It fails with `Stuck::AwaitingUser` when
  the session waits on a permission nobody answers, and with `Stuck::Timeout` naming the phase. `wait_for`
  waits for any condition over the events.
- **Reading**: `events()` is the normalized event stream with a query API, `events.root()`,
  `events.subagent("reviewer")`, `events.child(&id)`, each a `Lane` with `text()`, `thoughts()`,
  `tool_calls()` (folded: statuses in arrival order, diffs, output), `plan()`, `usage_reports()`; and
  `permission_requests()`, `route_changes()`, `failures()`, `phases()`.
- **The flight recorder**: `recorder()` is what the agents saw and did (prompts received with the scene
  that matched, permission requests and their answers, host tool calls and results, the fate of every
  response on the wire, crashes, divergences) in one order with what the user did.
- **Invariants**, checked by `violations()` and `finish()`: no permission is answered as cancelled unless the
  user's last action since their last prompt was a cancel (or a shutdown, or the connection died); no message is delivered twice, and once settled every
  message was delivered; each response reaches the client once; and once settled no tool call is still
  running and every turn ended.

A script and the world disagreeing (an expected answer that did not come, an unbound variable, a missing
scene, a host tool that failed) is a divergence in the recorder and in `Report`, never a silent pass.

## Showcase worlds

| World | Shows |
| --- | --- |
| `smoke.pkl` | The smallest world |
| `comprehensive-chat.pkl` | Thought, a plan that advances, five reads at once, a search, two edits that ask permission with inline diffs, a failing test run and a passing one with streamed output, usage with rate limits, a long markdown answer |
| `nested-subagents.pkl` | Three levels through the real host tools: a lead, two reviewers in parallel, two scouts under one of them, a peer message between siblings, and completion wakes |
| `long-running.pkl` | Thirty-seven minutes of world time in one turn: usage and rate limits climbing, the context filling, a permission request near the middle |
| `carbon-replay.pkl` | An agent that asks for permission after its turn ended, and after being cancelled |
| `quota-fallback.pkl` | A placeholder model (404), then a spent quota, then a route that answers, through the recovery policy |
| `multi-account.pkl` | Two Claude accounts, Codex and a gateway, each reporting its own quota: `/usage` per account, `/model` with room meters, a fallback to the sibling account |
| `adversarial-faults.pkl` | One prompt keyword per fault, and agents that start slowly, need authentication, or fail `session/new` |
| `claude-coordination.pkl` | A Claude-shaped agent: unknown stream kinds, a replaced turn, a permission racing a cancel, a dying CLI, provider prose and nested JSON errors, a child that fails, a model switch that does nothing, models that arrive late; and agents that never answer `initialize` or `session/new`, are not installed, or cannot reopen a session |
| `kit-demo.pkl` | Agents built from `nori:Kit`: a read, an edit behind a permission, a test run, a Claude `Task` subagent, a Codex native child, a `/review` command, and a scene that answers only once |
| `monitors.pkl` | Monitors: a failing build, a noisy job, a clock, a `notify` action that never wakes the agent, a `session` monitor woken by a failed request |
| `jobs.pkl` | Background jobs on scripted processes: ready then exit 3, a job that never ends and is stopped, a flood of output with `wake_on_output`, a job that dies with a line on stderr, a command nothing runs, and the running-jobs count |
| `goal-judge.pkl` | The goal judge ([workflows.md](workflows.md#goals)): an agent claims its goal done and a second scripted agent, with no Nori tools, answers not yet, then done; refuses until the goal is paused for the person; fails; never answers; or gives an answer that is no verdict |
| `oversize-context.pkl` | A twelve-megabyte history and a provider that refuses a text block over 10 MiB or a context over its budget: the fork, the fallback checkpoint, and the pasted prompt |

`cargo run -p nori-world --example run_world -- <world> "<prompt>"...` plays a world through the real
harness and prints the summary a test would assert on; `cargo run -p nori-fake-agent --example check_world
-- <world> [--json]` validates one.

## The fault suite

`nori-rs/world/tests/` is the integration suite for the faults in [fault-catalog.md](fault-catalog.md): each
test boots one of the worlds above through `WorldSession` and asserts what Nori must do. `cd nori-rs && just
faults` runs it. The clock is virtual, so every wait that is a script's time is instant; the only real time
is the quiet moment `settle` waits for (under a second). Tests about agents that never answer run on a paused
tokio clock, so the harness's own timeouts pass at once. `worlds_load` checks that every showcase world still
evaluates and validates.

## The TUI as text

`cargo run -p nori-world --example world_ui -- <world> --out DIR --size 100x34 [--ask] "<prompt>" /waitfor TEXT
/key y /frame NAME` runs `nori --world` on a pty with a `vt100` terminal (`tui-pty-e2e`), types prompts, presses
keys, waits for text or time, and writes each frame as `NN-NAME.screen.txt` and `NN-NAME.scrollback.txt`. It
needs a built `nori` (`cargo build --bin nori`). A debug `nori` approves every request; `--ask`
(`NORI_TEST_REQUIRE_APPROVAL=1` with `scripts/rex-scenario`) makes it ask. The frames are the visual
before and after of a UI change: the same world, size and script, diffed. Set `NORI_WORLD_CLOCK=real` for
streaming and for worlds with permission timeouts; the default `virtual` skips cadence.

## Authoring

- A property named like a Pkl keyword (`class`, `delete`, `when`) needs backticks; the schema avoids them.
  Inside a scene, `steps` is the scene's own property, so a `local` of that name is shadowed.
- Describe what the world means, not what the harness should do: a scene that retries after a refusal is
  an agent, not an assertion. Assertions belong to the test.
- A `Permission` that times out marks its tool call failed; to leave a call open on purpose use `ToolStart`.
- Time that must pass in the harness as well (a connect timeout) needs the real clock.

`mock-acp-agent` keeps its own environment-variable scenarios and recorded wire replay for the existing
terminal fixtures; new fixtures are worlds.
