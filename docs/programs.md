# Programs

A **program** is a declarative multi-actor workflow written in Pkl. It declares the
**actors** it uses (agent sessions it starts and addresses by alias), optional
**worlds** (isolated scopes), tools, and a tree of typed **steps**. Nori evaluates
the module with the native Rust Pkl runtime, checks it, compiles it to an **op
graph** and runs the graph through one host, journaling every step. The `conversation`
host tool runs a program; `nori-conduit` owns the runtime and the harness supplies
the host.

Pkl is the basis and `nori-conduit` is the rules substrate: the same data can be
written back to a file (the run journal and a Pkl snapshot) or carried as versioned
messages to another process, such as a future conduit daemon or a node on the BEAM.

```pkl
amends "nori:Program"

name = "review-loop"
actors {
  ["dev"] { agent = "claude-code"; model = "opus" }
  ["reviewer"] {
    agent = "codex"
    recovery {
      retry { max_attempts = 2; initial_backoff = 500.ms }
      fallback { chain = new Listing { new { agent = "claude-code"; model = "sonnet" } } }
    }
  }
}
steps {
  new Send { to = "dev"; message = "Implement the change"; bind = "patch" }
  new Loop {
    max_iterations = 4
    until = new Check { value = "verdict"; contains = "APPROVED" }
    steps {
      new Send { to = "reviewer"; message = "Review:\n{{patch}}"; bind = "verdict" }
      new When {
        check = new Check { value = "verdict"; contains = "APPROVED"; negate = true }
        steps { new Send { to = "dev"; message = "Fix:\n{{verdict}}"; bind = "patch" } }
      }
    }
  }
}
result = "patch"
```

Complete examples live in `nori-rs/pkl/examples/`: `review-loop.pkl`,
`research-fanout.pkl` and `two-worlds.pkl`.

## Checking a program

```sh
cargo run -p nori-conduit --example program_check -- pkl/examples/review-loop.pkl
cargo run -p nori-conduit --example program_check -- pkl/examples/review-loop.pkl \
  --journal /tmp/run --fail reviewer:1:rate_limited --approve reviewer:2
```

The first form loads and validates. `--run` (implied by `--journal`) executes against
an in-memory host that starts no agents; `--fail alias:n:class` makes an actor's first
`n` turns fail with a failure kind and `--approve alias:turn` makes it approve from
that turn on, so recovery paths can be exercised (the in-memory host applies the overrides the way
the harness's engine does, without waiting), and `--signal topic=payload` sends the run a
signal through its `RunHandle`. `--json` prints the compiled op graph.

## The schema

The schemas are embedded as `nori:*` modules (`nori-rs/conduit/schemas/`): `Program`,
`Agent`, `Recovery`, `Tool`, `Skill`, `Workspace` and `Run`. A program imports the module a
class comes from (`import "nori:Tool" as Tool`), as any Pkl module does.

| Program field | Meaning |
| --- | --- |
| `name`, `description` | Identify the program in the journal. |
| `actors` | `Mapping<String, Actor>`; the key is the alias. |
| `worlds` | `Mapping<String, World>`; scopes steps can run `Within`. |
| `world` | The world the whole program runs in. |
| `skills` | `SkillSpec`s actors use by name: instructions and the tools they allow. |
| `tools` | Tools the program calls by name, host or `wasm`. |
| `recovery` | Program-wide recovery override. |
| `budget_tokens` | Token budget of the whole run. |
| `steps` | The typed steps, in order. |
| `result` | The binding returned as the program's value; default is the last step's. |

An **actor** declares `agent`, `model`, `role` (`implementation`, `research`, `review`,
`design`, `test`, `general`), `instructions` (delivered ahead of the first message it
receives), `options` (agent settings by id), `tools`, `skills`, `sandbox`, `recovery`,
`isolation` (`shared`, `worktree`, `sandbox`) and `world`. A `Send` to a declared actor starts it on first use;
`actor://alias` addresses an actor the program did not start. Aliases resolve through
the tree-wide actor directory, so a program, the task tools and the TUI address the same
actor.

### Sandboxes, tools and skills

An actor's **sandbox** is the program's outer sandbox, narrowed by the world it lives in
(`allowed_tools`, `allowed_paths`) and then by the actor's own `sandbox`; a nested scope
never widens access. On the harness it is enforced where the actor's Nori tools are served:

- `tools` (the actor's own, plus those its skills allow, intersected with the tool names the
  sandbox allows) selects which Nori host tools exist for the actor, through the same
  per-session tool selection workflow policy uses. A tool outside it is not advertised and
  cannot be called.
- The actor's Nori `bash` runs every command through the AST sandbox: `bash:git` allows only
  `git`, redirection targets and path literals must lie in `allowed_paths`, dynamic command
  names are refused. The restriction is part of the actor's identity; it survives a revival
  and is inherited, as a sandbox, by anything the actor delegates to.
- Skill `instructions` are appended to the actor's tool instructions.

This is what the removed session-diff sandbox did (it restricted the model's `bash` by tool,
command and path), now declared per actor or world and also narrowing which tools exist. It
does not restrict an agent's own built-in tools (an agent's native shell or editor); a
sandbox is as strong as the tools it governs.

A **world** has `isolation`, `allowed_tools`, `allowed_paths`, `budget_tokens` and `home`.
Steps inside `Within` run under the world's sandbox and budget. Actors that start in a
`worktree` or `sandbox` world work in their own git worktree. With `home = true` the world
gets its own `NORI_HOME` under the run directory: `config.toml` (with every key whose name
contains `token`, `secret`, `password`, `credential`, `api_key` or `auth` removed), `init.pkl`
and `commands/` are copied from the parent's home; history, sessions, transcripts, logs and
`.env` are not, so the world neither sees nor writes the parent's records. Agents keep their
own authentication in their own homes, which the world does not touch.

Every **step** may set `id`, `bind`, `timeout` and `recovery`. `bind` names the step's
value for later `{{name}}` references (messages, prompts, tool arguments, fan-out items)
and for checks; on a `Send` with `await_reply = false` it names the receipt for a later
`Await`. `\{{` writes a literal `{{`. Fan-out steps also bind their `each` name, and a
`Guard` binds `feedback` and, in its post check, `output`.

| Step | Effect |
| --- | --- |
| `Send` | Message an actor; waits for its reply unless `await_reply = false`. `delivery = "steer"` interrupts first. |
| `Await` | Wait for the reply to an earlier non-awaiting `Send`. |
| `Spawn` | Start an actor now. |
| `Prompt`, `Config` | Queue a prompt in, or set an ACP option of, the session running the program. |
| `Call` | Call a host tool or a declared tool with JSON `args`. |
| `Unsafe` | Run an external process. Never implied by anything else. |
| `SelectContext`, `Fork`, `Rewind` | Read, fork or rewind the conversation journal. |
| `Sequence`, `Par`, `Race`, `FanOut` | Compose steps: in order, all at once, first success wins, once per item. |
| `Loop`, `When`, `Guard` | Repeat until a check holds, branch, or check before or after steps. |
| `Budget`, `Within` | Bound tokens; run in a world. |
| `Retry`, `Recover` | Run steps again as a whole; run a fallback when they fail. |
| `Emit`, `AwaitSignal`, `Interrupt` | Signals between flows or worlds; suspend for a person. |
| `Delay`, `Pin`, `Checkpoint` | Wait; record session facts. |

A **check** is `value` (a binding) with exactly one of `contains`, `equals`,
`starts_with`, optionally `negate`.

## Validation and errors

Loading reports every problem at once. Syntax errors give `file:line:col` and the source
line. Type, constraint and missing-property errors from the evaluator are located in the
module's own source (`--> file:line` and the line, or the candidate lines when several fit).
Checks that need the whole program are reported with the line of the literal that caused
them and the step path: unknown actors, worlds and receipts, a `{{name}}` that is not bound
where the step runs, two parallel branches binding one name, empty bodies, malformed
recovery blocks, and wasm tools with a bad module path or schema. Every message lists
what is valid (the declared actors, the names bound at that point).

## Recovery

A program's retry and fallback use the same `recovery` shape as workflow policy
([workflows.md](workflows.md#recovery)) and the same engine. An actor is a managed child with a
session of its own, and that session's recovery engine retries its requests and moves it between
routes (other models of its agent, other agents, the ranked catalog) while it keeps its identity
and lane. With every message the run hands that engine the overrides in force for the turn,
laid over the policy for the actor's role and agent, least specific first: the program's
`recovery`, the actor's, and the `recovery` of every enclosing step.

The run recovers nothing a second time. A turn the engine gave up on fails the step with the
final failure kind and its cause (what was tried is in the journal), and the program's own
`Retry` (re-run a block under the resolved `retry` policy; failures that carry no class, such as
tool exits and guard rejections, are `unknown`) and `Recover` steps decide what happens next.
What the engine did is journaled as it happens: `recovery.retry` for each scheduled retry and
`route.changed` for each move, and the run's snapshot lists the routes each actor ended on.

## Runs, the journal and the snapshot

Each run writes `.nori/runs/<run-id>/` in the working directory:

- `journal.jsonl`: one versioned envelope per line, appended before the run goes on. A
  failure to journal fails the run.
- `state.pkl`: a Pkl module that amends `nori:Run`, rewritten after every step with the
  run's status, sequence, tokens, pins, checkpoints, open interrupts, actors (agent, model,
  session, turns, fallbacks), worlds and the latest value of every binding. It is plain data: load it with the same
  evaluator to inspect a run. It is a projection; the journal is authoritative.

The conversation journal records the program's source, its run id and its outcome, so a
rewound or resumed conversation knows what ran. Reopening a journal only reduces recorded
data: it never evaluates a program, repeats a tool call or starts a process. Append-before-
effect makes intent auditable but does not make an external process exactly-once across a
crash. A rewind is rejected while a program effect runs; a program that began before another
rewind cannot dispatch further effects into the new session, and may continue after its own
successful rewind.

Program steps do not wait on peers inside a turn: a `Send` waits in the program, not in an
agent turn, and an agent that calls the `conversation` tool receives the run's result when
the program ends.

## The wire envelope and transports

Every fact is one envelope (`nori-conduit` `wire.rs`):

```json
{"v":1,"run":"run-1a1f127-000","actor":"reviewer","turn":2,"seq":14,"op":"message.replied","payload":{"receipt":"task-9","text":"APPROVED"}}
```

`v` is the envelope version (readers reject unknown versions), `run` the run id, `actor`
the alias or `program`, `turn` the actor's turn counter, `seq` the strictly increasing
sequence of the run, `op` what happened and `payload` its JSON object. Ops: `run.started`
(carries the compiled op graph), `run.finished`, `run.failed`, `run.cancel`,
`step.started`, `step.finished`, `step.failed`, `actor.spawned`, `message.sent`,
`message.replied`, `recovery.retry`, `route.changed`, `tool.called`, `signal`,
`interrupt.requested`, `interrupt.resumed`, `world.opened`, `world.closed` and `note`.
`message.replied` carries the turn's `tokens` and `tokens_estimated`.
Inbound ops (`signal`, `interrupt.resumed`, `run.cancel`) wake `AwaitSignal`, answer an
`Interrupt` and cancel the run.

A BEAM node reads the same data as the tuple `{conduit, V, Run, Actor, Turn, Seq, Op,
Payload}`: `Run` and `Actor` are binaries, `Op` is an atom named by the op string, objects
are maps with binary keys, strings binaries, `true`/`false` atoms and `null` the atom
`nil`. `Envelope::to_erlang_term` renders that term.

A `Transport` carries envelopes both ways. `FileTransport` is the journal plus an
`inbox.jsonl` that is tailed for inbound envelopes; `ChannelTransport` is an in-process
pair; `FanoutTransport` mirrors a run to several transports (a failing mirror never stops
the run). A daemon implements the same trait over a socket or port and is a drop-in
second world.

## Tools and custom WebAssembly

`Call` runs a host tool (`bash`, git, delegation, ...) or a tool the program declares.
Tools are checked against the active sandbox before they run. A declared `WasmTool` names
its module file (relative to the program), exported function, a JSON Schema for the
arguments, capability grants (`fs_read`, `fs_write`, `net`, `env`, `clock`, `random`,
`actors`, each with a scope) and time and memory limits. `nori-conduit` defines the
`ToolRuntime` trait; this build has no WebAssembly engine, so a program that calls a wasm
tool fails before it starts, naming the tool and what is missing.

## Watching and steering a run

A `RunHandle` is the live capability over a running program, built from the run's transport:
`subscribe()` streams its envelopes, `snapshot()` reads the latest `state.pkl`, `signal()`
and `resume_interrupt()` feed it, `cancel()` stops it and `wait()` returns its outcome. A
daemon or supervisor holds the same handle (or a transport that speaks the same envelopes).
The older session-diff seam (`Session.pkl`, `submit_diff`) is gone: a program's state is its
snapshot and its journal.

## Conversation context and journals

The read-only `conversation_context` host tool returns the conversation ID, visible bounds
and head, and canonical context carrying event IDs (default last 50 records, `limit`
1-200, `start` and `end` inclusive, `next_start` to continue). Rewound records stay in the
journal but are excluded from this view. `SelectContext`, `Fork` and `Rewind` take event IDs
from it.

The conversation journal is append-only Pkl properties; replay reduces data and never
evaluates it. `ConversationStore` separates append and replay from storage (file and memory
stores, versioned writes, command deduplication). `nori_pkl::native::parse_document` retains
each property's declaration and value spans, and `native::edit::replace_values` applies
transactions of value edits to source while preserving every other byte.

Programs read the captured [machine properties](workflows.md#machine-properties) as
`read("prop:...")`. Normal file and environment reads remain available during fresh
evaluation, so imports and resources are not a reproducible snapshot; replay uses recorded
data.

## Budgets and usage

`Budget` steps, `budget_tokens` on a world and on the program bound what actors spend. On the
harness a turn is charged the input and output tokens its actor's agent reported while it ran
(the usage samples of [usage.md](usage.md), tagged with the actor). When the agent reported
nothing, the turn is charged an estimate of one token per four characters of the reply and
the journal says `tokens_estimated`. Tool calls charge their own cost.

## The Pkl evaluator

Evaluation is native Rust (`vendor/pkl-native`); there is no Java or external-Pkl
fallback. Durations (`2.s`, `500.ms`) and data sizes reach the host as `{value, unit}`.
Syntax errors show `file:line:col`, the source line and a caret; type, constraint and
missing-property errors show the offending property in the module's own source, the declaring
property when the engine knows it, and `amends`, `extends` and `import` failures name the chain
of modules they passed through. Constraints on values a module assigns to a property it amends
are enforced. Known conformance gaps are in `vendor/pkl-native/NORI-SPIKE.md`; unsupported
constructs fail rather than being approximated.
