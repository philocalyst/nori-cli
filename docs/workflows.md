# Workflows

A workflow configures follow-up prompts, commit and pull request guidance, and
how a session retries and falls back when its model fails. Workflow policy files are Pkl modules; they
return data for the harness to apply. Evaluating policy does not dispatch agent
messages or repeat conversation effects. Terms follow [glossary.md](glossary.md).

## Policy sources

| Source | Path | When it loads |
| --- | --- | --- |
| Built-in defaults | `nori-rs/pkl/default.pkl` | First |
| User policy | `$NORI_HOME/init.pkl` | Every session, when present |
| Project policy | `<repo>/.nori/nori.pkl` | After user policy, only in a trusted directory |

Policies apply in that order. Later modules override fields they set. Nori evaluates
these policy modules with its native Rust Pkl runtime, including imports, amendments,
typed values, collections, lambdas, interpolation, property reads, and the standard
library needed by policy files. The implementation is still experimental: known
language and renderer gaps remain documented in `vendor/pkl-native/NORI-SPIKE.md`.
An unsupported construct is a policy error and is never delegated to Java or an
external Pkl executable. A session with no custom policy uses compiled defaults
without evaluating policy source; a test compares those defaults with `default.pkl`.
A missing file is skipped. User policy modules have normal Pkl file and environment
resource access, so they can use Pkl imports and reads directly.

The `nori-script` Steel interface remains only for compatibility with legacy
code; ACP policy loading uses Pkl and shared `nori-workflow` data types.

A policy module returns top-level values. Unset values keep those from the
previous source. For example, a project policy can replace `instructions`
without replacing the global policy's hooks. The nested `commit`, `pr`, `hooks`
and `instructions` values are replaced as whole values when set. The `recovery`, `compaction` and `loops`
blocks are the exception: each merges field by field (see below). Unknown policy
fields are errors.

## Policy fields

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | `String` | Workflow name. |
| `max_turns` | `Int` | Maximum hidden hook turns after a user turn. Omit it for no policy cap; `0` disables hook turns. Negative values are invalid. |
| `instructions` | `List<String>` | Standing instructions included with workflow prompts. |
| `hooks` | `List<Dynamic>` | Ordered `{ event, prompt }` rules. Events: `on-change`, `on-turn`, `on-commit`, `on-pr`, `on-stop`, `on-ci-fail`, `on-ci-pass`. |
| `recovery` | `Dynamic` | Retry, fallback and return behaviour when a request to the model fails. See [Recovery](#recovery). |
| `compaction` | `Dynamic` | When a session compacts itself, and how a too-long checkpoint is cut down. See [Compaction](#compaction). |
| `loops` | `Dynamic` | What happens when an agent repeats the same tool call. See [Loop detection](#loop-detection). |
| `goals` | `Dynamic` | Whether an independent reviewer checks a goal the agent calls complete. See [Goals](#goals). |
| `fallback_models`, `fallback_ranked` | `List<String>`, `Boolean` | Legacy spellings of `recovery.fallback.chain` and `recovery.fallback.ranked`; still read, and a `recovery` block in the same module wins. |
| `commit` | `Dynamic` | Commit style and batching options. |
| `commit_reviewer` | `Dynamic` or `null` | Agent and optional model for background commit review. |
| `pr` | `Dynamic` | Pull request content and upstream policy. |
| `tools` | `Dynamic` | Session selection, descriptions, and instructions for Nori-owned host tools. |

The built-in policy leaves `max_turns` unset. There is no default hook-turn
budget. A `max_turns` value is an explicit policy cap; it does not cap a normal
model turn or its final answer.

## Recovery

Programs ([programs.md](programs.md)) reuse this block: a program, an actor and any
step may carry a `recovery` of the same shape, layered over this policy.

Retries and fallbacks are declared, not coded: one engine in the harness reads
the `recovery` block, and nothing in the UI decides policy. Every failed request
is classified into a **failure kind**, from the structured ACP error first
(JSON-RPC code, `data` identifiers, HTTP status, `retry-after`) and from message
text only when nothing structured identifies it:

| Kind | Meaning |
| --- | --- |
| `rate_limited` | Throttled; waiting helps. The provider's `retry-after` is honoured. |
| `quota_exhausted` | The subscription or billing window is used up. Only a reset or another route helps. |
| `model_unavailable` | The model is gone on this route: HTTP 404, a stale id, an unfilled `MODEL_PLACEHOLDER`. |
| `capacity` (alias `overloaded`) | The provider has no capacity for the model right now (HTTP 529, overloaded). |
| `server_error` | The provider failed internally (HTTP 5xx). |
| `network` | The agent's backing service could not be reached. |
| `auth` | Credentials are missing, expired or rejected. |
| `prompt_too_long` | The prompt does not fit the context window, or a provider refuses one of its strings as too long (`string_above_max_length`). Nori first retries with a much shorter checkpoint of earlier conversation. |

Then, for the failing route:

1. If the kind is in `retry.on` and retries remain, the same route is tried again
   after an exponential, jittered backoff. Each retry is shown in the transcript
   ("Server error on codex · gpt-6.2; retrying in 8s (attempt 2 of 4). Esc
   cancels.") and Esc stops the wait.
2. Otherwise, if the kind is in `fallback.on`, the session moves to the next
   route of the plan. A route on the same agent is selected in the running
   session (its history is kept). A route on another agent is started by the
   harness itself, with the session fenced so nothing else starts meanwhile: the
   agent is replaced, receives a checkpoint of the visible conversation, and the
   failed prompt is sent again there. This works the same for the terminal UI,
   headless runs and delegated children (a child follows its role's chain across
   agents and stays in its place in the delegation tree). A route that cannot start
   is skipped; when none can, the old agent stays and the failure is shown. The
checkpoint is bounded to the new agent's context window (see harness `docs.md`:
a quarter of the window, at most 1 MiB). Esc
   during the swap abandons it at once: the old agent stays, whatever was starting
   is stopped, and the transcript says so. Prompts queued behind the failed one
   move to the new agent's queue and run there.

   Whatever a failed attempt had streamed stays in the transcript and is marked
   ("retried" / "continued on another route") before the new attempt starts. The
   conversation shows one route-change marker for a swap; the new agent's session
   start only updates the header and status in place.
   The transcript always says which route now runs
   ("Quota exhausted on opencode · openai/gpt-6.2-sol; continuing on codex ·
   gpt-6.2-sol"), and the status line shows it.
3. Otherwise the failure is shown with its cause.

The plan is the `chain` in order, then, when `ranked`, every other runnable route:
first routes for the same model (native providers before gateways), then other
models. A route that failed in this conversation is never offered again.

With several accounts of one provider (`claude-code` and `claude2`, two Claude
profiles) the same model is reachable through each. `prefer_headroom` (default
`true`) decides between them by the room each account has left in its rate-limit
windows, as the usage log last saw them (see [usage.md](usage.md#accounts)): the
account with more room is tried first, so a spent `claude-code` moves to
`claude2` before anything cross-provider, and an account known to be spent is
tried after every route that is not. The `chain` is never reordered. Accounts
trade places only among the places their provider already holds, so low room never
lets a gateway pass a native agent or one provider pass another, and the route a
session runs on is left for its sibling only when it is spent. An account no
window was ever reported for has unknown room and counts as having plenty. Set
`prefer_headroom = false` to keep the order below untouched. The route change
names the accounts: `account primary → secondary`.

```pkl
recovery = new Dynamic {
  retry = new Dynamic {
    max_attempts = 4                 // retries after the first failure; 0 turns retrying off
    initial_backoff = 2.s            // doubles each retry
    max_backoff = 60.s
    jitter = 0.2                     // +/- 20% so parallel sessions spread out
    on = List("server_error", "capacity", "network", "rate_limited")
  }
  fallback = new Dynamic {
    on = List("quota_exhausted", "model_unavailable", "capacity")
    chain = List(                    // tried in order, before anything ranked
      new Dynamic { agent = "claude-code"; model = "haiku" },
      "codex/gpt-6.2-astra"          // shorthand for agent/model
    )
    ranked = true                    // then every other runnable route
    prefer_headroom = true           // between accounts of one provider, the one with room first
    return_to_primary = "next_session"   // never | after_reset | next_session
  }
  roles = new Mapping {              // delegated subagents, by role
    ["review"] = new Dynamic {
      fallback = new Dynamic { chain = List(new Dynamic { agent = "claude-code"; model = "haiku" }) }
    }
  }
  agents = new Mapping {             // by agent; a role override beats an agent override
    ["codex"] = new Dynamic { retry = new Dynamic { max_attempts = 2 } }
  }
}
```

Every field is optional and layers deep-merge in the order built-in, user,
project: a project policy that sets only `recovery.retry.max_attempts` keeps
everything else. `chain` and `on` lists are replaced as whole values. A route is
`agent`, `model` and optional settings; an omitted `agent` means the running one. A route
states how the model is served as `model_mode` (`"standard"`, `"fast"`, `"flex"` or
`"priority"`) and its reasoning as `effort` (as the agent offers it); each is mapped onto
whichever option that agent has (Claude Code's `fast` toggle, Codex's `service_tier`, a thought
level), and one the agent lacks is left alone. Any other select setting is named in `options` by
its id or name (`["service_tier"] = "flex"`); `model_mode` and `effort` are also accepted there.
The settings are applied after the model, against the options that model offers. Durations are Pkl durations
(`2.s`, `500.ms`, `1.min`); a bare number is read as seconds.

`return_to_primary` says what happens to the route the user chose once a fallback
has left it:

- `next_session` (default): the conversation stays on the fallback until a new
  session of it starts (after `/compact` or a rewind), then goes back to the
  chosen route if nothing rules it out. Usage that is exhausted rules it out for
  the rest of the conversation, so a quota fallback stays until a new conversation.
- `after_reset`: at the first turn boundary after the route may be used again, the
  session goes back to the chosen route before the next prompt runs. Quota
  exhaustion uses the agent's stated reset time (one hour when it states none), a
  rate limit its `retry-after`. A model that failed as unavailable is re-checked
  after 15 minutes and again an hour later by asking the agent what it offers (a
  stale id has vanished from that list); the route is used again only if the model
  is still offered. Capacity has no cheap check, so it behaves as `next_session`.
- `never`: the fallback becomes the saved choice.

Going back is announced like any other route change ("back on your chosen
route"). Quota exhaustion on a gateway such as OpenCode excludes only the provider
its model id names (`openai/...`), not every model the gateway serves.

An unavailable model (a 404, a placeholder id) is remembered with its reason:
`/model` and `agent_capabilities` list it as unavailable instead of offering it,
and the mark lapses after six hours. Debug fixture agents never appear in any
model list or fallback plan unless the session itself runs on one.

Children use their role's policy. Capacity that an external provider cannot
supply is not fixed by retrying: Nori retries visibly within the limits above,
then falls back or tells you.

### Route order

One ordering decides every model list and every ranked fallback. A route sorts by:
whether it is the route running now (your own choice); its place in the explicit
preferences (the `chain`, then your `default_models`, agent by agent); whether its
agent is native for the model's family (`claude-code` for Claude, `codex` for GPT,
`gemini` and `antigravity` for Gemini) rather than a gateway to it; then agent
order; between accounts of one provider reaching one model, the room left in each
account's windows decides (`prefer_headroom`). There is no per-model score table. A gateway such as OpenCode is first only
when you chose it, named it, or it is the only agent that offers the model.
Choosing a model in `/model` is local until you commit: moving the cursor never
switches anything.

A model chosen on the running agent (`/model`, `/config`, or the cycle keys `Ctrl+N` /
`Ctrl+P`, effort `Ctrl+J` / `Ctrl+K`, mode `Alt+.` / `Alt+,`) shows in the footer when it is
chosen, not when the agent answers, during a response as well as between them: the advertised
configuration is updated in place, the agent is told in the background, its answer confirms
it, and a refusal puts the previous value back with the reason in a toast. The harness hands
each change to a configuration lane of its own: changes reach the agent one at a time in the
order they were made, a change that a later one to the same option replaces before it was sent
is not sent, and no other command (a Cancel included) waits for the agent's answer. Every
`config.toml` write goes to one config writer thread, in order, so neither the event loop nor
a concurrent edit waits or is lost; a write that fails says so in one line, and the choice
stays in effect for the session.

A model that only another agent runs starts that agent with the conversation so far; the
conversation is read off the event loop. Chosen during a response, the switch is accepted at
once and made when the response ends, with one line saying so; it is never refused. The new
agent's model, effort and binding are put in place on a task, and the interface takes the new
agent when they are; an unchanged journal head, not a second read of the conversation, proves
the conversation did not change while the agent started.

### Nori host tools

The optional `tools` object selects Nori-owned host tools before the ACP agent
receives its catalog. It does not alter the agent's built-in tools or other MCP
servers. By default, git (`git_status`, `git_diff`, `git_log`, `git_commit`) and
forge (`ci_status`, `pull_request`) wrapper tools are excluded in favor of the
persistent in-process `bash` shell; list them in `enabled` when a workflow
explicitly wants them exposed. `enabled`, when present, is an allowlist (an
empty list selects none); `disabled` removes names from that list or from the
default catalog. `descriptions` overrides model-facing descriptions by name, and
`instructions` appends model-facing guidance to the generated list of selected
tools. Later policy sources replace each supplied field independently.

```pkl
tools = new Dynamic {
  enabled = List("get_goal", "git_status")
  disabled = List("git_status")
  descriptions = new Mapping { ["get_goal"] = "Read the current goal" }
  instructions = List("Check the goal before reporting completion.")
}
```

Only selected tools are advertised and callable. If `get_goal` or `update_goal`
is disabled, Nori disables its `/goal` command and goal continuations unless
the agent provides its own goal extension. A customized catalog cannot
reuse a Claude process prewarmed with a different catalog. Nori loads policy
once before creating the agent session when it can receive Nori tools, and uses that same policy and captured
machine snapshot for its prompts and tools. Agents without Nori tools can start
while the policy loads; prompt admission waits for it.

### Commit options

`commit` may set `style` (`plain` or `conventional`), `max_files`,
`max_subject`, `atomic`, `batch` (`per-change`, `per-turn`, or `per-task`),
`require_body`, and `agent` (`{ agent, model? }`). The `agent` field delegates
commit writing to that configured ACP agent.

### Pull request options

`pr` may set `title`, `sections`, `max_lines`, `max_files`, `base`, `draft`, and
`upstream`. The upstream value is `never`, `ask`, `tests-pass`, or `done`.

## Compaction

A session compacts itself before its context runs out. The `compaction` block merges field by
field over the layers (built-in, user, project), as `recovery` does: a project that sets only
`auto = false` keeps every other value from the layers below. `shrink_ladder` is a list and is
replaced whole.

| Field | Default | Meaning |
| --- | --- | --- |
| `auto` | `true` | Compact without being asked. `false` leaves `/compact` to the person. |
| `trigger_free_fraction` | `0.15` | Compact when the free context is below this fraction of the window... |
| `trigger_free_tokens` | `16000` | ...or below this many tokens (at most a quarter of the window), whichever asks for more room. A 200000-token window compacts below 30000 free. |
| `background_margin_tokens` | `12000` | Write the summary this many tokens of free context before the trigger, while the provider's prompt cache is warm, so the switch is instant. `0` prepares nothing early. Keep it within the checkpoint's 15,000 tokens of verbatim recent context: a summary further behind than that is written again. |
| `shrink_ladder` | `List(0.85, 0.7, 0.5, 0.35)` | When a provider refuses a prompt as too long, retry with the checkpoint of earlier conversation (never the person's words) cut to each fraction of its size in turn. Empty: no retry with a shorter one. |
| `prefer_native` | `true` | Use the agent's own `/compact` when it advertises one, instead of a summary prompt and a new session. |

Nori measures the context the agent last reported of the live session (the `usage_update` of
[usage.md](usage.md), as `used` of `size`), never in the middle of a turn: before a prompt starts,
and when the session comes to rest after a turn. Usage reported while a resumed session replays its
history describes the context as it was then, on whatever model ran it, and is not measured: an old
chat resumed does not compact on what it once held. An agent that reports no context size is never
compacted automatically. A window too small for the token floor or the margin takes a quarter of
the window for each instead, so it is not always due. Compaction waits for the turn that is running, so nothing the agent is doing is cut
short, and it never holds up the interface: the summary is an ordinary turn of the agent.

- **Due** (free context below the trigger). The person sees one quiet line, `Context 87% full;
  compacting before the next prompt.` Then the agent is sent its own `/compact` (`prefer_native`),
  or is asked for the summary, and the conversation moves to a new session that opens with the
  checkpoint. A prompt typed meanwhile waits and starts on the compacted session. Compaction at
  rest happens while the person is still reading the last answer.
- **Approaching** (within `background_margin_tokens` of the trigger), at rest, for an agent that
  compacts by summary: `Context 71% full; preparing a summary while the cache is warm, for when it
  fills.` The summary is written and kept; the session does not change. When the context passes the
  trigger, the swap uses it (`Context 89% full; switching to the summary prepared earlier.`) with no
  second request. What happened since the summary was written goes into the checkpoint verbatim
  (up to 15,000 tokens); a summary further behind than that is written again.
- **Settings survive.** The replacement session gets the model first, then the mode, effort and the
  other settings the old one had.
- **Back-off.** A compaction the harness started that fails (the agent's summary is not in the
  template after one reminder, the turn stops early, no new session can start) is said once, with
  `Automatic compaction is off for this session after that failure; /compact when you want it.`
  No further automatic compaction is tried in that session until the person runs `/compact`.

Old tool results leave the checkpoint first (`nori-harness` `context_budget`). See
[turn-lifecycle.md](turn-lifecycle.md) for how a compaction holds the queue, and the
`context_pressure` tests ([fault-catalog.md](fault-catalog.md), `context-overflow`).

## Loop detection

An agent that makes the same tool call again and again is not getting anywhere, and every round
costs tokens. The `loops` block merges field by field like `compaction`.

| Field | Default | Meaning |
| --- | --- | --- |
| `window` | `10` | How many of a turn's latest tool calls are looked at. |
| `repeats` | `5` | A call is a loop when its signature occurs more than this many times in the window. Must be less than `window`. |
| `action` | `"nudge_then_stop"` | `nudge_then_stop`: steer the agent once, and cancel the turn if it loops again in the same task. `nudge`: steer once per task and otherwise let it go on. `stop`: cancel the turn at once, as Esc does. `off`: do not look for loops. |

A call's signature is its kind, its title and a hash of its raw input (object key order does not
matter). A call whose input has not arrived yet is not counted, so parallel calls that are still
streaming their arguments do not look like repeats of each other. The window belongs to one turn; a
task (the work from one user prompt to the next) remembers that the agent was steered.

What the person sees: `Loop: "cargo test" called 6 times with the same input; steering the
agent.` and the agent is sent `You have called "cargo test" with the same input 6 times; it is not
making progress. Stop and say what is blocking you, or try a different approach.` as a steer that
interrupts the turn. If it loops again in the task: `Loop: "cargo test" called 6 times with the
same input; stopped the turn.` and the turn is cancelled.

## Goals

An agent that is pursuing a goal marks it done with `update_goal` and the status `complete`.
Its own say-so is not evidence, so Nori has an independent reviewer, the **goal judge**, check the
claim first. The `goals` block merges field by field like `compaction`; `judge_agent` and
`judge_model` can be set back to `null` by a later layer.

| Field | Default | Meaning |
| --- | --- | --- |
| `judge` | `true` | Review a `complete` claim before accepting it. `false` accepts the agent's word, as before. |
| `judge_agent` | `null` | The agent the reviewer runs on, such as `"claude-code"`. `null`: the session's own agent. |
| `judge_model` | `null` | The reviewer's model, such as `"haiku"`. `null`: that agent's default model. (No agent marks one of its models as its cheapest, so Nori does not guess; name one here to keep reviews cheap.) |
| `judge_timeout` | `120.s` | All the reviewer may take, from starting to answering: above zero and at most `10.min`. |
| `max_disagreements` | `3` | After this many "not yet" verdicts in a row for one goal, the next `complete` pauses the goal for the person to decide. At least `1`. |

The reviewer is a fresh actor, started like a `delegate_task` child but for the person (nothing
is owed to the agent, so no wake follows), with none of Nori's tools. It sees only a prompt: the
objective wrapped as untrusted data, and the newest part of the session's log (the same record
`conversation_context` reads, tool outputs included, since they are the evidence) within the
context budget of the reviewer's agent. It answers with one line of JSON, and the prompt tells it
that the agent claiming the goal is met is not evidence and that when in doubt the answer is
"continue". The prompt is `nori-rs/harness/templates/goal/judge.md`, adapted from OpenCode's
goal judge (MIT).

The reply is read strictly: one JSON object, with only whitespace or a Markdown code fence around
it. Anything else counts as "not yet" with the reason `the judge's answer could not be read`. The
verdicts:

| Verdict | The goal | The agent's `update_goal` result | The person sees |
| --- | --- | --- | --- |
| `done` | becomes `complete` | success; says it was verified, and why | `Goal judge: done (reason)` |
| `continue` | stays `active` | an error, so it is not read as done: what the judge found missing, the next step, and to call `update_goal` again only when the transcript shows it | `Goal judge: not yet (reason)` |
| `blocked` | becomes `blocked` with the reason | success; says what the agent needs from the person | `Goal judge: blocked (reason)` |
| `paused` | becomes `paused` with the reason | success; says to stop until the person resumes | `Goal judge: paused (reason)` |

Nothing is ever stuck behind the judge. If it cannot run (the agent is not installed, its session
fails) or does not answer within `judge_timeout`, the agent's word is accepted: the goal becomes
`complete`, the tool result says it was **not independently verified** and why, and the person sees
`Goal judge: unverified, accepting the agent's word (cause)`. When `max_disagreements` verdicts in
a row were "not yet", the next `complete` is not reviewed again: the goal is paused, the agent is
told that it and the judge disagree and the person decides, and the person sees one line saying
the same (`/goal resume`, `/goal edit` and `/goal clear` are theirs). Pausing or resuming, a new
objective and `/goal clear` start the count again.

The review runs on a task of its own that the `update_goal` call awaits, so it never blocks the
interface, no lock is held across it, and a client that gives up on the call (Codex does after
a minute, so give `judge_timeout` less than that with such an agent) still gets its verdict
applied. If the goal changed while the reviewer was working (the person paused or edited it), the
verdict is dropped and the agent told so. The reviewer shows as a subagent called `Goal judge`.
It runs in the session's working directory with none of Nori's tools, but Nori cannot remove an
agent's own built-in tools; the prompt tells it to run nothing.

```pkl
goals = new Dynamic {
  judge_agent = "claude-code"
  judge_model = "haiku"
  judge_timeout = 45.s
  max_disagreements = 2
}
```

The `goal-judge.pkl` fake world and `nori-rs/world/tests/goal_judge.rs` play the verdicts, the
disagreement pause, a reviewer that fails and one that never answers
([fake-worlds.md](fake-worlds.md)).

## Machine properties

Pkl modules can read the machine snapshot passed by Nori through external
properties:

| Property | Value |
| --- | --- |
| `machine.cpu.available_parallelism` | Logical CPUs available to the process. |
| `machine.cpu.load_average.one_minute` | One-minute system load average. |
| `machine.cpu.load_average.five_minutes` | Five-minute system load average. |
| `machine.cpu.load_average.fifteen_minutes` | Fifteen-minute system load average. |
| `machine.memory.total_bytes` | Total visible memory, in bytes. |
| `machine.memory.available_bytes` | Available memory, in bytes. |

Read a property with Pkl's `read("prop:<name>")`; values are strings, so parse
numeric readings with `toInt()` or `toFloat()`. Unsupported readings are the
literal string `unsupported`, rather than zero. For example:

```pkl
local available = read("prop:machine.memory.available_bytes")
local enoughMemory = available != "unsupported" && available.toInt() > 8_000_000_000

name = "memory-aware"
instructions = if (enoughMemory) List("Run the full test suite") else List("Run focused tests")
```

Memory values are the system values reported by sysinfo; they may not reflect a
container's memory limit. CPU availability uses Rust's process-available
parallelism estimate.

Nori captures one snapshot before evaluating policy, journals its serialized
value, and passes the same values to both workflow and recovery policy loads.
When resuming a recorded session, it reuses that snapshot instead of reading
machine state again. Replaying the conversation reduces recorded data and never
evaluates policy modules or repeats effects.

## Example

`$NORI_HOME/init.pkl`:

```pkl
name = "standard"
recovery = new Dynamic {
  fallback = new Dynamic {
    chain = List("provider/preferred-model", "provider/backup-model")
    ranked = false
  }
}
instructions = List("Run the tests that cover each change")
hooks = new Listing {
  new Dynamic { event = "on-stop"; prompt = "Summarize the work and remaining risks" }
  new Dynamic { event = "on-change"; prompt = "Review the diff and commit focused changes" }
}
commit = new Dynamic {
  style = "conventional"
  batch = "per-change"
  atomic = true
}
pr = new Dynamic {
  sections = List("Why", "What", "Testing")
  base = "main"
  upstream = "ask"
}
```

Replace illustrative model IDs with models advertised by the candidate agent.
With this policy a quota, unavailable-model or capacity failure moves to the
chain in order, and nothing ranked follows it. A fallback to another agent starts
a new provider session with the failed prompt and a checkpoint of visible
conversation context. It cannot inherit provider-private session state.

A commit reviewer can be selected with:

```pkl
commit_reviewer = new Dynamic { agent = "claude"; model = "haiku" }
```

The reviewer runs as a background subagent after a changed user turn. It is
asked to review staged, unstaged, and untracked changes and suggest commit
messages without modifying files or committing. The working tree may contain
older changes; this instruction is not a read-only sandbox.
