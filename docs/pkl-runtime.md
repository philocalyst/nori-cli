# The Pkl runtime

Pkl is Nori's programmable core. Everything a person or an agent can *declare* about how Nori
behaves (scripted agents, recovery, background work, what to watch and what to do about it,
multi-agent procedures, plugins) is a Pkl module of one of a few **kinds**. One evaluator reads
them all into typed Rust values; the harness performs the effects; the journal records them.
Evaluating a module never has an effect, and replaying a journal never evaluates a module.

Terms follow [glossary.md](glossary.md).

## One core

| Piece | Where | Role |
| --- | --- | --- |
| Evaluator | `vendor/pkl-native`, `nori-pkl::native` | Native Rust Pkl: imports, amends, classes, typed listings and unions, functions, generators. No JVM, no subprocess. Errors carry `file:line:col` and the source line. |
| Schemas | `nori:<Kind>` virtual modules | One schema per kind, embedded in the binary. A module says what it is with `amends "nori:<Kind>"`. |
| Catalog | `nori-pkl::catalog` | Finds modules, evaluates them off the event loop (`spawn_blocking`), decodes each into its Rust type, keeps the last good value per file, publishes the set through a `watch` channel, and re-evaluates a file when it changes. |
| Bindings | one Rust type per kind | serde types with `deny_unknown_fields`; a module that does not fit its type is an error that names the field. |

A broken module never blocks a session: it is reported once (the TUI shows `pkl: jobs/dev.pkl:12
unknown field "comand"`), skipped, and its last good value, if any, stays in force. Nothing on
the UI path waits for evaluation.

## Kinds

| Kind | Schema | Lives in | Is |
| --- | --- | --- | --- |
| World | `nori:World` | anywhere (`nori --world <file>`) | Deterministic fake agents for tests, demos and captures. [fake-worlds.md](fake-worlds.md) |
| Policy | (data module) | `$NORI_HOME/init.pkl`, `.nori/nori.pkl` | Recovery, routes, hooks, commit and PR rules. [workflows.md](workflows.md) |
| Job | `nori:Job` | `jobs/*.pkl` | A named background process: a dev server, a watcher, a long test run, on this machine or another. |
| Monitor | `nori:Monitor` | `monitors/*.pkl` | Triggers, a condition, and actions: what to watch and what to do when it happens. |
| Skill | `nori:Skill` | `skills/*.pkl` | A multi-step, multi-agent procedure (a [program](programs.md) with parameters), run by an agent or by `/name`. |
| Plugin | `nori:Plugin` | `plugins/*.pkl` | Commands built from typed operations on Nori itself, callable from the TUI, from Rex, or from another process. |
| Gate | `nori:Gate` | `gates/*.pkl` | Ordered rules consulted synchronously before something happens: a permission request, a Nori tool call, a turn ending. [Gates](#gates) |

`Lives in` is relative to two roots, read in this order: `$NORI_HOME/` (the person's own), then
`<repo>/.nori/` (the project's, only in a trusted directory). A project module shadows a personal
one of the same name. The directory names the kind and the module must amend that kind's schema.

## Jobs

A **job** is a process Nori supervises in the background. It is started by name from a job module
or ad hoc by an agent, runs on this machine or a remote one, and outlives the turn that started it,
the session, and Nori itself.

```pkl
amends "nori:Job"

name = "web"
description = "The dev server on :5173"
command = "pnpm dev"
cwd = "web"
machine = "local"                  // or a machine from the roster: "h16003mac"
ready = new Ready { pattern = "Local:.*5173"; timeout = 60.s }
restart = new Restart { on = "failure"; max = 3; backoff = 2.s }
wake = new Wake { on_exit = true; on_ready = true; on_output = "(?i)error|panic" }
check = "curl -fsS localhost:5173 >/dev/null"   // what "it worked" means, run after ready and exit
```

| State | Meaning |
| --- | --- |
| `starting` | Spawned; not `ready` yet (a job with no `ready` is running at once). |
| `running` | Ready, with its pid. |
| `exited` | Ended by itself, with its code. |
| `killed` | Stopped by a signal (`job_stop`, a restart, a session move that did not take it). |
| `unreachable` | Its machine cannot be reached right now; Nori keeps the record and adopts it again with backoff. |
| `lost` | Gone for good: the pid vanished without a code, or the machine restarted under it. Final. |

Output is kept on the machine that runs the job (`~/.nori/jobs/<id>/out.log`, `err.log`, rotated at
`max_log_bytes`), and every line carries a **cursor**, its byte offset in each stream. `job_read`
reads from a cursor, a tail or a head, so an agent reads exactly what is new, across reconnects,
restarts and moves, and never re-reads what it saw.

Execution belongs to the **supervisor** (`nori-machines`): it starts the process detached on its
machine (`setsid`, its own process group, a pid and an exit file next to the logs), streams its
lines, reports its state, stops it (TERM to the group, KILL after a grace), and **adopts** a job
recorded earlier: after Nori restarts, after a resume, after a session moves to another machine. A
record names its host absolutely (machine name and boot id), so a job that was "local" before a move
is a job of the origin machine after it, and a reboot that reused its pid is seen as `lost`. The
harness owns everything above it: the registry of an actor's jobs, the tools, the journal, and the
wakes. Environment values whose names look secret are never journaled, and credentials never travel
with a move: a moved session adopts its jobs, it never re-creates them. Worlds supply a scripted
supervisor, so a job's output and exit play on the virtual clock.

**What the agent never does:** poll. Other harnesses hand the model a shell id and make it call a
read tool in a loop to learn whether anything happened; each poll is a turn spent and a wait guessed.
A Nori job wakes its agent when something it said matters happens (it is ready, it printed a line that
matches, it exited, it was lost), through the turn ledger, without ever interrupting a running turn.

### Tools

Jobs are Nori host tools ([nori-client-mcp.md](nori-client-mcp.md)); their guidance is
`nori_host_tools::JOB_INSTRUCTIONS`. The registry is `backend/jobs/`, one per actor.

| Tool | What it does |
| --- | --- |
| `job_start` | Start `command` (or the module named by `job`) in the background; returns at once with its id. `label`, `machine`, `cwd`, `env`, `wake_on_exit` (default true), `wake_on_output` (regex), `ready` (regex). |
| `job_list` | Every job with its state, machine, command, wake and age. Not a way to wait. |
| `job_read` | Output from the agent's **read cursor** (`from: new`, the default): exactly what it has not read, once, then the cursor moves on. `tail` returns the last `max_bytes`, `start` the beginning. Returns `stdout` and `stderr` apart, the state, and `more` when output is still waiting. |
| `job_stop` | TERM to the process group, then KILL after `grace_seconds` (default 5); or one `signal`. The end the agent asked for does not wake it. |

Labels are unique among running jobs, at most 16 run at once per actor. A module job applies its
`command`, `machine`, `cwd`, `env`, `ready` and `wake`; the request's fields override them. Not yet
applied (marked `TODO` in `jobs/request.rs`): `restart`, `check`, `timeout`, `ready.timeout`,
`max_log_bytes`, and `job_restart`.

The **wake** is a monitor of the `job` source ([monitors.md](monitors.md)), armed by `job_start`
(or by whoever hands the supervisor's handle to `Jobs::adopt_handle`, as the host `bash` tool's
`background: true` does) and labelled like the job, so the pipeline, delivery and journal are the
monitors'. A wake that asks for nothing arms nothing; the job is still listed and readable.

**Journal and resume.** One `job` command per change: `Started { record, origin, wake }`,
`Ended { id, state }` and `Read { id, cursor }`. On resume every job started and not ended is listed at
once (`unreachable`, being found again) and the supervisor **adopts** them side by side in the
background, so a machine that does not answer never holds the session up; the agent's read cursor
comes back with each, and its wake monitor (restored dormant with the other monitors) is started
again. A session that moved here (`moved-in` in its journal) reads each `local` job record by the
machine that wrote it, the `from` of the first move after it: such a job is adopted as a job of that
machine when this roster names it (the corrected record is journaled), and is `lost` ("left
running on <from>; not reachable from here") when it does not (`backend/jobs/moves.rs`). A job that cannot be adopted is `lost`: the
person sees a notice, and the agent's wake says the job is gone. Closing the session stops
following; the jobs keep running. The status line count is `MonitorsState.jobs`.

## Monitors

A **monitor** is triggers, an optional condition, and actions. An agent arms one at runtime with
`monitor_start` ([monitors.md](monitors.md)); a monitor module arms one for every session it applies
to. Both are the same value.

```pkl
amends "nori:Monitor"

name = "nightly-deps"
on { new Cron { expression = "0 3 * * 1-5" } }
actions {
  new RunJob { job = "deps-audit" }
  new Wake { message = "The dependency audit ran:\n{{event}}\nFix anything critical." }
}
```

| Trigger | Fires on |
| --- | --- |
| `Every { interval }`, `At { time }`, `Cron { expression }` | Time: a period, an instant, a schedule (5 fields, or 6 with seconds first). |
| `Idle { after }` | The session has been idle that long (once per idle stretch). |
| `Session { events }` | `turn_ended`, `turn_failed`, `route_changed`, `subagent_ended`, `permission_requested`, `quota_low`. |
| `Job { job, on }` | A job is `ready`, `exited`, `failed`, `lost`, or prints a line (`output`). |
| `Output { command }` | Lines of a command run for the monitor alone (today's `command` source). |
| `File { path }`, `Watch { paths, kinds }` | Lines appended to a file; files created, modified, deleted. |
| `Http { url, interval }`, `Ci { branch }`, `Actor { actor }` | A value that changed; checks that decided; a peer's state, results, replies. |

`when` filters what fires (`filter`, `fail_on`, `until` regexes over the event's text), `debounce`
gathers bursts, `max_fires` and `expires` bound it, and `quiet` hours hold informational events.

| Action | Does |
| --- | --- |
| `Wake { message, mode }` | Gives the agent a hidden prompt (`{{event}}` is what fired). `mode` is `immediate`, `digest` or `boundary`. The default action. |
| `Prompt { text }` | Queues a visible prompt, as if the person had typed it. |
| `Notify { title, body }` | An OS notification and one transcript line; the agent is not woken. |
| `RunJob { job }`, `StopJob { job }` | Starts or stops a job. |
| `RunSkill { skill, arguments }` | Runs a skill; its result is delivered like a wake. |

Delivery keeps the monitor invariants: an event is journaled before it is queued, delivered once,
never interrupts a turn, and a resume restores what was not delivered. A monitor says how it ended.
Time triggers read the session's clock, so in a world they are virtual and exact.

### Gates

Monitors never block: they observe and act afterwards. A **gate** is the synchronous counterpart:
the same power as Claude Code's hooks (allow, deny, ask, change a tool's input, make the agent
carry on after it stopped), as typed Pkl data instead of a shell script run on every event. A gate
module lives in `gates/*.pkl` (`$NORI_HOME/gates/`, and `.nori/gates/` in a trusted project), is
evaluated with the other modules when the session starts, and is **compiled once** into a Rust
matcher (regular expressions and globs built, rules in order). Consulting one never evaluates Pkl,
spawns nothing and holds no lock: a check of 41 rules costs a few microseconds (measured by
`checking_a_gate_costs_microseconds`), and the first request of a session waits for the first
load (at most 3 s; it normally takes well under one) rather than pass gates it has not seen. The compiled gates live on the delegation tree, so a
delegated child's requests and tool calls meet the same gates as its parent's.

A gate has a `name`, a `description`, an event `on`, and `rules`; each rule is a matcher and a
decision. The gates are consulted in the order of their names, each one's rules in the order
written; **the first rule that matches decides**, and no match is today's behaviour.
`nori-rs/pkl/examples/gates/` holds three: `no-force-push`, `tests-before-done` and `force-machine`.

```pkl
amends "nori:Gate"

name = "no-force-push"
on = "permission"
rules {
  new PermissionRule {
    kinds { "execute" }
    command = "\\bgit\\s+push\\b.*(--force|--force-with-lease|\\s-f(\\s|$))"
    decision = "deny"
    reason = "Force-pushing is not allowed here. Push without --force, or tell the person why."
  }
  new PermissionRule { kinds { "execute" }; command = "\\bgit\\s+push\\b"; decision = "ask" }
}
```

| `on` | Checked | Matches on | Decides |
| --- | --- | --- | --- |
| `permission` | `handle_permission_request` (`harness/src/backend/turn/approvals.rs`): every `session/request_permission`, from any agent of the tree. | `kinds` (ACP tool kind), `title`, `command` (the call's `command` input with `args`), `paths` (any named file matches a glob), `only_paths` (every named file does). | `allow` and `deny` answer the agent without asking the person; `ask` always asks, even under approval policy `never` and over a saved allow rule. A refusal the person saved holds against `allow` and `ask`. |
| `host_tool` | `gates::gate_host_call`, from `host_tools::dispatch` (built-in agents) and the `nori-client` MCP server's `call_tool` (every other agent): `bash`, `job_start`, ... | `tools` (names), `arguments` (a regex per argument; non-strings match as their JSON), `absent` (arguments not passed). | `allow` runs it and stops looking; `deny` returns `reason` as the tool's error; `rewrite` applies `set` (new values) and `append` (text after a string argument) and runs the result, and the agent's result ends with a note saying the call was rewritten and how. |
| `stop` | `handle_completed_turn` (`turn/completion.rs`): a local turn of the person's session ended with `end_turn`. | `last_message` (the agent's last message), `changed` (globs; the edit journal says the turn changed a matching file). | `continue` hands the agent `message` as a hidden prompt (`QueuedPromptKind::GateContinue`, offered to the turn ledger like a workflow hook) at most `max_continues` times (default 1) for each prompt of the person, a rule that has spent them being skipped; `pass` lets the turn end and stops looking. |

Patterns are Rust regular expressions matching anywhere in the text; `new Not { regex = ... }`
matches text the expression does not. A matcher left out matches anything; those given must all
hold. Globs are matched against paths relative to the project (absolute outside it): `*` stays in
a directory, `**` crosses them. Everything a module can get wrong (a pattern that does not
compile, a `deny` with no `reason`, a `continue` with no `message`, a `rewrite` that changes
nothing, a rule of another event) is a module problem, reported with its `file:line` like any
other; a gate that cannot compile is left out whole and says so.

**Every decision is journaled** as one `gate` command (`{gate, rule, event, decision, subject,
detail, at}`), and the person sees one quiet line for each `deny`, `rewrite` and `continue`
(`gate no-force-push: denied git push --force origin main`), and for an `allow` that spared them a
question they would otherwise have been asked. Nothing is silent.

**What the agent reads.** A host tool refusal and a rewrite note are in the tool's result. ACP's
permission response has no field for a reason, so a refused request is answered with the
agent's reject option (`cancelled` when it offered none) and `_meta.nori.gate`
(`{name, rule, reason}`) for an agent that wants to show the model why; an agent that ignores it
tells the model only that permission was denied.

**Not built.** Claude Code's other two hook events are designed here and left for later:
`pre_compact` (block a compaction, or add instructions to its summary) and `subagent_stop` (the
`stop` event for a delegated child, which today is not gated). Pre-tool gating of an agent's
*own* tools that never ask permission is out of reach: an agent that does not ask is not
consulted, so gate what it asks (`permission`) and what Nori gives it (`host_tool`).

## Skills

A **skill** is a [program](programs.md) with a name, parameters and a description: agents, steps,
rounds, a result. An agent runs it with the `skill` tool (called with no `name`, it lists the skills
there are, each with its summary and parameters); a person runs it with `/name args`
(`HarnessHandle::skills` and `run_skill`). Arguments take the place of `{{parameter}}` before the
program is checked, so a skill run with them is an ordinary program, and a missing or unknown
argument is an error that names it. Its progress is the program's journal. Nori's own `council` is
`nori-rs/harness/builtin/skills/council.pkl` (dry-run it with `program_check ... --run --arg
request=...`).

**Arguments also shape the program.** Before the module is evaluated, each argument is visible to it
as the external property `skill.<name>`, so a module reads `read?("prop:skill.count")` (null when the
argument was not given) and builds actors and steps from it with `for` generators: the council's
members and their number are parameters this way, not only text in a prompt. A module that must
refuse an argument raises it from a regular property such as `name` (`name = if (bad) throw("...")
else "council"`): an error inside a `local` reaches the person as "Cannot find property".

```pkl
amends "nori:Skill"

name = "council"
summary = "Several models review a question in rounds and converge on one answer."
parameters { ["question"] { description = "What the council decides" } }
actors {
  ["opus"]  { agent = "claude-code"; model = "opus" }
  ["gpt"]   { agent = "codex"; model = "gpt-6.2" }
  ["chair"] { agent = "claude-code"; model = "sonnet" }
}
steps {
  new FanOut { over = List("opus", "gpt"); each = "member"
    steps { new Send { to = "{{member}}"; message = "{{question}}"; bind = "views" } } }
  new Send { to = "chair"; message = "Reconcile these views into one answer:\n{{views}}"; bind = "answer" }
}
result = "answer"
```

The program's capability bundles (instructions and allowed tools for an actor) are **toolsets**
(`nori:Toolset`), so `skill` means one thing.

## Built-in modules

Nori ships modules of its own, today two skills: `impact` (what a change affects, with nudox as the
evidence: [nudox.md](nudox.md)) and `council` (several independent models answer the same request in
parallel and a chair reconciles them: the shape of Theo's `/council`). They are embedded in the binary
(`nori-rs/harness/builtin/`) and written to `$NORI_HOME/builtin/<kind>/` each time modules load, only
where the content differs; the catalog reads that directory as its lowest root (`Scope::Builtin`).
A module of the same `name` in `$NORI_HOME/<kind>/` or a trusted project's `.nori/<kind>/` shadows
the built-in one, so overriding is copying the file out and editing it. `$NORI_HOME/builtin/` is
generated: its files are overwritten and a module Nori stopped shipping is removed. If the directory
cannot be written, the person sees one `pkl:` line and the built-in skills are missing.

`council` takes `request` (with all the context: the members start with none), `members` (`agent` or
`agent:model`, comma separated; default `claude-code:opus,codex,gemini:pro`, one strong model per
provider), `count` (how many sit; asking for more than are named fails, and a member that cannot start
fails the run: nothing is substituted, recovery fallbacks are off), `effort` (default `high`), `chair`
(default `claude-code:sonnet`), `synthesis` (the chair's brief), and `round` and `previous` for a pass
after fixes. Members run in parallel as read-only actors; the chair reports agreements,
disagreements, findings, next steps and, for a code review, each member's score and whether it is
satisfied (`no score` when none). Applying the recommendations and asking again is the calling
agent's, as in Theo's.

## Plugins

A **plugin** declares commands, each a typed list of **operations** on Nori: the API is the set of
operations, and a plugin is data, so what it may do is visible before it runs.

| Operation | Does |
| --- | --- |
| `session.describe`, `session.export` | What the session is (agent, model, machine, cwd, jobs, monitors, permissions), and a bundle of it. |
| `session.move { machine }` | Moves the whole session to another machine: see below. |
| `job.*`, `monitor.*`, `skill.run` | The same primitives the tools use. |
| `notify`, `prompt`, `config.set` | Tell the person; queue a prompt; change the model, effort or mode (accepted at once, applied at the next safe point). |

A command is invoked from the TUI (`/name`), from Rex or any other client over the remote-control
channel (`_nori/plugin.run`), or with `nori plugin run <plugin> <command>` against a running Nori.

### How a command runs

`HarnessHandle::plugins()` lists the session's plugins (name, description, each command with its
parameters) and `HarnessHandle::run_plugin(plugin, command, arguments)` runs one (`nori_harness::
backend::plugins`; the same two calls are the remote methods `_nori/plugins` and `_nori/plugin.run`,
see [remote-control.md](remote-control.md#10-plugin-methods)). A run is a task of its own: nothing
waits on the UI path, and no lock is held across an operation.

1. **Arguments are settled first.** Each parameter takes the given value, else its `default`; an
   argument the command does not declare, or a required parameter nobody gave, is an error naming
   it. Every `{{parameter}}` is then filled into every text field of every operation. All of this
   happens before the first operation, so a typo never leaves half a command done.
2. **Operations run in order**, each yielding a typed result (`StepResult`, tagged by the
   operation's name). The first failure stops the command; the error names the step, as
   `commands.to.operations[0] (session.move): ...`, and carries the results of the steps that ran
   before it.
3. **Each operation is something Nori already does**, called and never rebuilt:

   | Operation | Runs |
   | --- | --- |
   | `session.describe` | Reads the session: agent, conversation, cwd, this computer's name, the agent's settings (model, effort, mode), the jobs it follows, its monitors, the approval policy and the sandbox. |
   | `session.export { path }` | `portable::export` after flushing the transcript, written off the event loop; with no `path`, `$NORI_HOME/exports/<conversation>.nori-session`. |
   | `session.move { machine }` | The move belongs to the frontend that owns the terminal. The harness checks the roster, then emits `NoriEvent::SessionMoveRequested`; the TUI answers it with its own move flow (`/move`) and the person sees that flow's lines. The step result says only that the move was requested. |
   | `job.start`, `job.stop` | The job supervisor: `JobStartRequest::module(name)`, and `stop` (TERM, then KILL after five seconds). |
   | `monitor.start`, `monitor.stop` | Arms a monitor module exactly as standing monitors are armed at load; stops every monitor the module armed. |
   | `skill.run` | `execute_skill`. |
   | `notify` | The same code as a monitor's `notify` action: one transcript line and an OS notification. |
   | `prompt` | `submit_prompt` with queue admission: the agent answers it in its next turn. |
   | `config.set { key, value }` | `set_config_option` on the session. A key the agent does not offer names the keys it does; an agent that refuses fails the step with its words. |

4. **A run is journaled.** The harness records one `plugin` command (plugin, command, arguments) in
   the conversation journal, and says how it ended in one transcript line (`plugin rex park: 2
   steps done`, or where it stopped), so it is visible in the transcript and on resume.

### Plugin commands as actions

A command is also an **action** of the action registry (`nori-actions`), so the TUI lists it in the
palette and runs it as `/name value` like a built-in. The `nori:Plugin` command declares how it is
listed; everything is optional:

| Field | Becomes | Default |
| --- | --- | --- |
| `title` | the palette title | the description |
| `names` | what is typed after `/` (the first is canonical) | `<plugin>-<command>` |
| `keywords` | more words it is found by | none |
| `category` | the palette group (`conversation`, `session`, `model`, `agent`, `machine`, `context`, `view`, `settings`, `account`, `tools`, `plugin`) | `plugin` |
| `effect` | `now`, or `on_send` (a chip in the draft, applied with the prompt) | `now` |
| `requires` | `idle`, `main`, `local`, `cloud`: when it can run | none |

A parameter may carry `choices` (a fixed menu) or `from` (`machines`, `models`, `efforts`, `modes`,
`agents`, `skills`, `files`, `sessions`: the frontend lists them), not both; a `default` must be one
of the `choices`. The action's id is `<plugin>.<command>` and its source is the plugin. What it
takes follows the parameters: none takes nothing; one parameter with `choices` or `from` is a
choice, any other single parameter is text; several parameters are text of `name=value` pairs.

`HarnessHandle::plugin_actions()` is a `watch::Receiver<Arc<[Action]>>` holding every plugin's
actions, replaced whenever the modules load again (empty until the first load). A chosen action
comes back as `HarnessHandle::run_plugin_action(invocation)`, which reads the argument as the
command's arguments and then runs it exactly as `run_plugin` does (one `plugin` journal record, the
same results and errors). One parameter takes the whole argument, spaces included. Several take
`name=value` pairs, split as a shell splits words (`note="two words"`), and a bare value fills the
first required parameter the pairs left (`/rex-send gpu`). An id that names no plugin command, an
argument for a command with no parameters, an undeclared name or a value left over is a
`PluginError` that says which.

### The Rex plugin

`nori-rs/pkl/examples/plugins/rex.pkl` is the whole Rex plugin, one short file of data:

```pkl
amends "nori:Plugin"
name = "rex"
commands {
  ["move"] {
    parameters { ["machine"] { description = "A machine from the roster" } }
    operations { new SessionMove { machine = "{{machine}}" } }
  }
  ["where"] { operations { new SessionDescribe {} } }
  ["park"] {
    operations {
      new JobStop { job = "web" }
      new NotifyOp { title = "Parked"; body = "The dev server is stopped" }
    }
  }
}
```

Dropped into `$NORI_HOME/plugins/`, Rex's menu is `_nori/plugins`, and a menu item is
`_nori/plugin.run {plugin: "rex", command: "move", arguments: {machine: "h16003mac"}}`.

**Moving a session** (with the Machines engineer): at the next turn boundary the session is fenced;
its bundle (conversation journal, transcript, permission rules and approvals, monitors, job records,
the working tree's git state and the agent's route) goes to the target machine; Nori there resumes
it, **adopts** its jobs (the supervisor reattaches or restarts them by their restart policy), re-arms
its monitors and re-applies its permissions; the source detaches. The person sees one line saying
where the session now runs. A Rex plugin is one command: `session.move { machine = arg }`.

## What agents and people see

| | Agents | People |
| --- | --- | --- |
| Jobs | `job_*` tools; wakes | `⚙ 2 jobs` in the status line; `/jobs` lists them with state, machine and the last lines |
| Monitors | `monitor_*` tools; wakes | `◉ 3 monitors`; `/monitors`; one transcript row per delivered batch |
| Skills | the `skill` tool | `/name` commands in the command palette; a card per run |
| Plugins | none (a plugin is the person's) | `/name` commands; Rex menus |
| Gates | a refusal or rewrite note in the result; `_meta.nori.gate` on a refused permission | one quiet line per `deny`, `rewrite` and `continue`; a `gate` record in the journal |
| Problems | tool errors that say what to fix | one line per broken module, with `file:line` |

Tool descriptions are written to be read once and acted on: what the tool does, what comes back,
and what not to do (never poll).

## Status

Built: the evaluator, worlds (with `nori:Kit`, and scripted `processes` for jobs), policies
(recovery, routes, `compaction`, `loops`, `goals`), programs, the catalog (`nori-pkl::catalog`,
with built-in modules under `$NORI_HOME` and the project, loaded per session in
`harness/src/backend/pkl_modules.rs`), jobs (`job_*` tools on the Machines supervisor, journaled,
adopted on resume and after a session move; the host `bash` hands background commands to them
through `Jobs::adopt_handle`), monitors with time, session (including `quota_low`) and `job`
triggers and `wake`, `prompt`, `notify`, `run_job`, `stop_job` and `run_skill` actions, standing
monitors and job watches from modules, skills (`skill` tool, `run_skill`, the built-in `impact` and
`council`), gates (`permission`, `host_tool`, `stop`), and the plugin runtime
(`nori_harness::backend::plugins`: remote methods, the Rex plugin, plugin commands as palette
actions through `HarnessHandle::plugin_actions` / `run_plugin_action`).

Not yet: a job module's `restart`, `check` and `timeout`; `pre_compact` and `subagent_stop` gates;
`nori plugin run` (it needs a remote-control client, which exists only in tests).
