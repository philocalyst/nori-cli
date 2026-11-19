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
and `instructions` values are replaced as whole values when set. The `recovery`
block is the exception: it merges field by field (see below). Unknown policy
fields are errors.

## Policy fields

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | `String` | Workflow name. |
| `max_turns` | `Int` | Maximum hidden hook turns after a user turn. Omit it for no policy cap; `0` disables hook turns. Negative values are invalid. |
| `instructions` | `List<String>` | Standing instructions included with workflow prompts. |
| `hooks` | `List<Dynamic>` | Ordered `{ event, prompt }` rules. Events: `on-change`, `on-turn`, `on-commit`, `on-pr`, `on-stop`, `on-ci-fail`, `on-ci-pass`. |
| `recovery` | `Dynamic` | Retry, fallback and return behaviour when a request to the model fails. See [Recovery](#recovery). |
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
| `prompt_too_long` | The prompt does not fit the context window. |

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
   is skipped; when none can, the old agent stays and the failure is shown. Esc
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
`agent`, `model` and optional `options` (other select settings, such as reasoning
effort); an omitted `agent` means the running one. Durations are Pkl durations
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
