# Native agents

An agent built into Nori's process, with no subprocess and no adapter between it and the **ACP
host**. Nori gives it one end of an in-memory pipe and keeps the other end for its own ACP client.
Every native agent is built on one crate, `nori-acp-kit` (`nori-rs/acp-kit`), and nothing else
holds in-process plumbing.

| Crate | Registration | What it drives |
| --- | --- | --- |
| `nori-codex-acp` | `codex` | Nori's engine with Codex's sign-in, configuration, catalog and prompts: no Codex core, no Codex program |
| `nori-claude-acp` | `claude` | Claude Code's CLI (the only process it starts) |
| `nori-opencode-acp` | `opencode` | Nori's engine with OpenCode's gateway and an API key: no OpenCode program |
| `nori-gemini-acp` | `gemini` | Gemini Core in a Node process (opt-in feature `native-gemini`) |
| `nori-antigravity-acp` | `antigravity-sdk` | Antigravity's local harness over its SDK (opt-in feature `native-antigravity-sdk`) |

## The engine

Nori's own agent engine (`nori-engine`, `nori-rs/engine`) runs the agent loop in Nori's process
and talks to the model providers' APIs directly through `nori-llm` (`nori-rs/llm`). There is no
subprocess and no server: an agent is a **flavor** served by the engine.

```text
nori-llm      wire clients: Anthropic Messages, OpenAI Responses, OpenAI-compatible Chat
              Completions; one provider-neutral conversation (messages of text, image,
              reasoning with its provider proof, tool call, tool result); streaming deltas;
              errors as a kind plus the provider's own sentence (never its raw body)
nori-engine   EngineAgent<F: Flavor>: sessions, the loop, tools, permissions, dials, journal
  flavor        what differs per agent: models and defaults, credentials -> Connection,
                configuration -> rules, modes, instructions, commands; system prompt; tools
  turn          read waiting prompts -> ask the model -> run the tools it called -> repeat
  act           one round of tool calls: card, permission question, run, result
  store         one Nori conversation journal per session (nori-conversation)
nori-opencode-acp  OpenCodeFlavor: OpenCode's gateway, its live catalog, an API key
nori-codex-acp     CodexFlavor: Codex's sign-in (codex-login), config.toml, catalog, prompts
```

Why not reuse Codex's core: the vendored Codex core speaks only the OpenAI Responses API (its
`wire_api = "chat"` was removed), carries Codex's own prompts, tools, rollout storage and
approvals, and cannot reach Anthropic or the Chat Completions models OpenCode's gateway serves. The
engine replaces it for Codex too (see "Codex" below); Nori's host tools and `nori-bash` were reused.

- **The loop.** Each step reads the session's dials afresh (model, effort, mode), so a change made
  mid-turn is accepted at once and applies from the next model call. Prompts that arrive while a
  turn runs are steered in (`TurnPolicy::Steerable`): they join at the next step, or keep the turn
  going when it would have ended. A turn stops after 200 model calls (`max_turn_requests`).
- **Streaming.** Text and reasoning go out as message and thought chunks as they arrive; a tool
  call's card appears the moment the model starts it, and is updated with its title, kind,
  locations and diff once its arguments are complete.
- **Retries.** A model call that fails in a way waiting can fix (rate limit, overload, 5xx,
  network, stall) is retried up to four times, honouring `retry-after` (at most 60 s), but only
  when nothing has been shown yet; after that the error is classified (`AgentError::Provider` with
  `rate_limited`, `usage_limit_reached`, `overloaded`, `prompt_too_long`, ...; authentication is
  `AuthRequired`). A content filter ends the turn as `refusal`, running out of output tokens as
  `max_tokens`.
- **Cancellation** stops the stream or the running tool at once. What the model had written is
  kept; every tool call it made gets a result (cancelled ones say so), and prompts steered into
  the cancelled turn join the conversation, so the next turn starts from a valid conversation.
- **Tools.** A tool states what a call is about before it runs (`prepare`: title, kind, paths,
  the permission checks it needs, a diff preview), then runs. The file tools are the shared
  `nori-tools` ones (see [tools.md](tools.md)): `SharedTool::catalog(EditStyle)` wraps each catalog
  entry as an engine tool, working in one `nori_tools::Workspace` per session. A flavor picks the
  style: `Replace` (`read`, `write`, `edit`, `multi_edit`) or `Patch` (`read`, `apply_patch`, Codex's
  format). The adapter derives the title, kind, paths and permission checks (including
  `external_directory`) from the arguments, and maps the result's changes to ACP diffs. An edit or
  overwrite needs the file to have been read in the session and not changed since (the read
  ledger), unless the flavor turns that off (`Flavor::file_tools`; Codex does, as it reads through
  its shell). The engine's own tools are `list`, `glob`, `grep` (`.gitignore`-aware), a plan tool
  (`todowrite` or `update_plan`, shown as the ACP plan) and a shell. When Nori lends its host tools
  (`Host::Nori`), they are offered as plain function calls, and Nori's persistent `bash` replaces
  the engine's own shell (`nori-bash`, in process).
- **MCP servers** the client names in `session/new` (load, resume, fork), stdio and HTTP, and
  those the flavor's workspace names, start in the background when the
  session opens; their tools (`<server>_<tool>`) join the model's list from the first step after
  they are ready. A server that fails to start is logged and left out.
- **Permissions.** Rules (`action` on a `resource` glob: `edit`, `shell`, `read`,
  `external_directory`, a tool's name) come from the flavor's configuration, the mode adds its own,
  and "Always allow" answers add more for the session; the last matching rule decides. `ask` puts
  the call's card, with its diff, to the person (Allow once, Always allow, Reject). A rejection
  ends the turn so the person can say what to do instead; a denial goes back to the model.
  The harness also saves the user's "always" answers in the conversation journal (see
  [transcripts.md](transcripts.md#saved-permission-rules)) and sends them in
  `_meta.nori.permissionRules` of `session/new`, `load`, `resume` and `fork`. A session starts with
  them as granted rules: the tool maps to the engine's action (`execute`, `bash`, `shell` to `shell`;
  edit kinds to `edit`; any other tool keeps its name) and the pattern is the resource. Denials are
  placed after the allows, so they win. Nori answers a matching request itself in any case, so the
  engine mostly sees these rules for what it does before asking.
- **Nori's extensions** (`_auth/status_update`) go only to Nori's own harness (`Host::Nori`).
- **Usage.** Every response reports its tokens and the context window (`usage_update`), and the
  flavor's rate-limit windows from the response headers.
- **Sessions** are Nori's own journals: `$NORI_HOME/agents/<agent>/sessions/<id>.pkl`, one
  append-only conversation log per session holding the session record, every message exactly as
  the model reads it (reasoning proofs included, so a resumed session continues where it was), the
  dials whenever they change, and the title. `session/resume` replays it into the session;
  `session/load` also sends the history to the client; `session/fork` copies it; `session/list`
  reads only the start of each journal. Journal writes run off the async threads.
- **Prompt caching.** Anthropic gets cache breakpoints on the last system block and the last two
  messages; OpenAI gets `prompt_cache_key` = the session id. Reasoning proofs are sent back only to
  the model that produced them (another model would reject them).

### OpenCode

`OpenCodeFlavor` (`nori-rs/opencode-acp/`) is OpenCode's model gateway (OpenCode Zen) reached with
an API key, run by the engine. Nothing of the OpenCode program is read or run: not its
configuration, database, sign-ins (Claude subscriptions, Copilot, z.ai), plugins or agents. Other
providers are Nori's own agents, never OpenCode's.

- **Key**: `OPENCODE_API_KEY`, else the key Nori keeps for the account (`opencode-api-key` in the
  account's home, or beside Nori's OpenCode sessions for the default account; readable by its owner
  only). Signing in is the ACP `opencode-api-key` method: the sign-in panel asks for the key and
  hands it to the agent, which keeps it; signing out removes it. The agent reports
  `_auth/status_update` (`OpenCode · API key`) or signed out.
- **Catalog**: models.dev's `opencode` provider, live. The catalog served starts as Nori's last
  copy (`<store>/models.json`), however old, and is asked for again in the background when a
  session opens (at most every five minutes) and every half hour while Nori runs. A catalog that
  differs replaces it, and open sessions are offered the new models at once (the engine's
  `Flavor::workspace_changes`). Only a first run with no copy waits for models.dev.
  `NORI_OPENCODE_MODELS_URL` moves it.
- **Models**: the gateway's models that call tools and are not deprecated, newest first, as
  `opencode/<model>`. A model's package picks the wire: `@ai-sdk/anthropic` is Anthropic Messages,
  `@ai-sdk/openai` the Responses API, everything else Chat Completions. Every request names the
  conversation in `x-opencode-session`, which the gateway routes and caches by.
- **Effort** (`thought_level`): the efforts the catalog lists for the model, else for reasoning
  models `none`/`low`/`medium`/`high`/`max` on Anthropic (a thinking budget; `none` by default) and
  `low`/`medium`/`high` on the Responses API.
- **Modes**: `build` (everything) and `plan` (edits denied).
- **Instructions**: `AGENTS.md` (else `CLAUDE.md`) in each directory from the project root down to
  the working directory.

### Codex

`CodexFlavor` (`nori-rs/codex-acp/src/flavor/`) keeps what makes the agent Codex and drops the
Codex core. It links only Codex's leaf crates (`codex-login`, `codex-config`,
`codex-models-manager`, `codex-model-provider-info`, `codex-prompts`, `codex-protocol`).

- **Sign-in**: Codex's own login in the account's `CODEX_HOME` (the account's `home`, else the
  `CODEX_HOME` variable, else `~/.codex`), so the Codex CLI and Nori share one login. `codex-login`
  reads `auth.json` or the keyring (`cli_auth_credentials_store`), refreshes a ChatGPT access token
  shortly before it expires and writes the new tokens back to the home. A token the backend refuses
  (401) before anything streamed is recovered once as Codex recovers it (read the login again from
  disk, then refresh) and the request is sent again. `authenticate` offers `chatgpt` (the browser
  login; not offered when `NO_BROWSER` is set), `codex-api-key` and `openai-api-key`; `logout`
  clears the login.
- **Backend**: a ChatGPT login talks to `https://chatgpt.com/backend-api/codex` with
  `Authorization: Bearer <access token>`, `chatgpt-account-id`, `originator` (`codex_cli_rs`) and
  `session_id` (also `session-id`, `thread-id`, `x-client-request-id`) set to the session id. An API
  key (login or `CODEX_API_KEY`/`OPENAI_API_KEY`) talks to `https://api.openai.com/v1`.
  `openai_base_url` and a provider's `base_url` override either. The ChatGPT backend takes no
  `max_output_tokens`, so none is sent. Requests are stateless (`store: false`).
- **Configuration**, layered as Codex layers it (later wins): the home's `config.toml`; each
  trusted project's `.codex/config.toml` from the project root down to the working directory (an
  untrusted project's is skipped with a warning); the selected `profile`; the account's `codex -c`
  overrides and `CODEX_CONFIG`. Read: `model`, `model_reasoning_effort`, `model_reasoning_summary`,
  `model_verbosity`, `service_tier`, `model_provider` with `[model_providers]` (`base_url`,
  `env_key`, `http_headers`, `env_http_headers`, `query_params`, `experimental_bearer_token`; the
  Responses wire only), `model_instructions_file`, `project_doc_max_bytes`, `mcp_servers`, forced
  workspaces and the credential store. Approval and sandbox settings are not read: see Modes.
- **Instructions**: `AGENTS.override.md` (else `AGENTS.md`) in the home, then the project's
  `AGENTS.md` files from the root down to the working directory, within `project_doc_max_bytes`.
  The system prompt is the model's own Codex base instructions (`codex-prompts`), or
  `model_instructions_file`.
- **Models, efforts and service tier**: the account's catalog, live: what the backend's `/models`
  last returned for the account. It starts as the home's `models_cache.json` (shared with the Codex
  CLI), however old and whichever client wrote it, and is asked for again in the background when a
  session opens (at most every five minutes) and every half hour while Nori runs; a catalog that
  differs is written back and offered to open sessions at once (`Flavor::workspace_changes`). Only
  an account with no catalog anywhere waits for the backend, and only one that cannot reach it
  falls back to the catalog the build shipped with. Each model offers exactly the efforts it lists (Codex's
  `ultra` and `persistent`, which drive Codex runtime features the engine does not have, are left
  out); the configured effort is the default where the model takes it, and switching models keeps
  the effort only if the new model takes it, else the nearest one it does. `service_tier` offers
  `default` ("Standard", no field sent), the tiers the model advertises (such as `priority`) and
  `flex`; the choice is sent as the request's `service_tier`. It is the engine's generic **dial**
  (`Dial`), so any flavor can add a select option that is one body field.
- **Tools**: `shell_command`, `read` and `apply_patch` (the shared file tools, `EditStyle::Patch`), `update_plan` (Codex's description) and `view_image`
  (offered only to models that read images; `Tool::offered`), plus Nori's host tools (the host's
  `bash` replaces `shell_command` when Nori lends host tools, and `delegate_task` runs subagents).
- **Modes** are rules, with no sandbox: the engine has none, so Codex's approval presets become
  the closest rules. `read-only`: edits refused, commands ask. `auto` ("Default"): edits run,
  commands ask. Both let commands the engine knows only read (`ls`, `cat`, `rg`, `git status`,
  and so on; rule name `read_only_shell`) run without asking. `full-access` (the default, as
  before, because Nori and not Codex owns approvals): everything runs. Work outside the workspace
  asks in every mode but `full-access`. Commands: `/init` and `/review`.
- **Usage and limits**: the plan's windows come from the response headers
  (`x-codex-primary-used-percent`, `-window-minutes`, `-reset-at` or `-reset-after-seconds`, and
  the secondary) as `five_hour` and `seven_day` rate-limit windows in `_meta.nori`, as other limits
  by their own family. The account label is the ChatGPT email.
- **Sessions** are Nori's: journals under `$NORI_HOME/agents/codex/`, shared by every account.
  Threads the old vendored-core Codex agent kept (Codex's own rollout files) are not resumable.

The test `nori-rs/codex-acp/tests/codex_engine.rs` runs all of this over ACP against a fake
ChatGPT backend and a fixture `CODEX_HOME` (an expired fake token refreshed against a fake token
endpoint, a refused token recovered, a tool round, effort and tier switches, usage windows).

The default `antigravity` agent is not native: it is the official `agy acp` process (see
[nix.md](nix.md)).

## Writing one

Two layers; use the shorter one that fits.

- `Provider`: the engine has sessions that are plain values. Implement `open` and `run_turn`, and
  `interrupt`, `configure` and `close` where they apply. `ProviderAgent` supplies the session
  table, one prompt at a time per session, cancellation, `session/close`, `set_config_option` and
  shutdown. A harness-harness is a `Provider` and nothing more.
- `NativeAgent`: the agent manages its own sessions, including load, resume and fork. The kit
  still serves the connection and routes requests; it offers `Sessions`, `TurnGate`, `Sink`,
  `ToolLedger`, `HostBridge`, `UsageReport` and `AgentError` to assemble the rest.

An agent declares what it does in `Profile.capabilities`. The kit answers `initialize` from it
and refuses every request for a feature it did not declare, so an agent cannot claim more than it
implements. Capabilities come from what the agent implements, never from what another agent
supports.

Each agent crate exposes `registration() -> Registration`. A registration holds the agent's name,
a function that builds the agent from an `InProcessSpec` (options, per-agent environment, home), an
optional prewarmer, and an optional resolver that folds the environment a custom adapter would
have read into explicit options. `nori-cli`'s `native_agents::register` is the list of them behind
their cargo features; the host keeps them in `nori_acp_host::in_process`. Unknown `key=value`
options are errors (`SpecOptions::parse`), never ignored.

An agent that has a **model mode** offers it as a select option with category `model_config` that
Nori maps into `ModelMode` (see the glossary). The built-in agents do: `claude-code` offers `fast`
(`off` "Standard", `on` "Fast"), applied to the running CLI as the `fastMode` flag setting, and
`codex` offers `service_tier` (`default` "Standard", then the tiers the model advertises, such as
`priority`, and `flex`). An option is recognized by its id (`fast`, `service_tier`, `tier`, `speed`,
`model_mode`) or by offering a `fast`, `flex` or `priority` value; a value Nori does not know is shown
under the agent's own label.

**Dial effects.** Changing a dial (model, effort, mode) is never refused; the client shows what the
change does where it is made. An agent declares that on each choice of the option as `_meta.nori.dial`,
`{"appliesAt": "now" | "nextStep" | "nextTurn" | "afterThisTurn", "costs": [..]}`, which is
`nori_protocol::DialEffect` (`Consequence`: `promptCacheRestarts`, `sessionRestarts`, `thinkingReset`,
`differentPrice {from, to}`, `contextShrinks {from, to}`; `nori_acp_kit::Choice::with_meta` sets it).
The engine agents declare it for the model (`nextStep`, `promptCacheRestarts` for any model but the
current one, plus `contextShrinks` when its context window is smaller), the effort (`nextStep`) and the
mode (`now`). A choice that declares nothing is `nextTurn` with no costs.

`nori-acp-kit` depends on the ACP library the host speaks and on `nori-host-tools`; it re-exports
both as `acp` and `schema`, so no agent pins a protocol version. It depends on neither
`nori-acp-host` nor any agent crate.

## Serving one to any client

`nori agent serve <slug> [KEY=VALUE...] [--home DIR] [--env KEY=VALUE]` serves a registered agent over
stdio ACP (the process environment, with `--env` on top, is what the agent's resolver reads), so any
ACP client can run any native agent. Only ACP goes to stdout. The kit treats that client as
`Host::External`: it never advertises host tools, which only Nori's own harness can supply, and
tells the agent (`NativeAgent::on_host`) so it sends none of Nori's own notifications either
(`_auth/status_update` goes to `Host::Nori` only).

## Turns

Every agent answers "what does a prompt do while a turn runs" through one `TurnGate` per session and
a `TurnPolicy`: `Exclusive` refuses it (`Busy`), `Steerable` admits it beside the running turn,
sharing its cancellation (Codex folds it into the running turn; Claude queues it: prompts reach the
CLI one turn at a time, in the order they came, and a prompt never cancels the work under way),
`Replace` cancels the running turn, waits a bounded time for it to end, and takes its place. A `Turn`
ends when dropped, and `gate.cancel()` cancels every turn that is running or waiting. No agent keeps
its own turn bookkeeping.

## Errors

Agents fail with `AgentError`. It converts to the ACP error the host classifies (`classify_failure`
in `nori-acp-host`): authentication is `-32000`, an unknown session `-32002`, an unreachable
backing service `-32010`, a session that cannot be reattached `-32012`, a backing service that is
not running at all `-32016` (`NotRunning`; the host classifies it as `ExecutableNotFound`, so the
agent counts as not available rather than as a network failure), a program the agent runs that is
not installed (`ExecutableNotFound`, `data.code` `executable_not_found`, shown with the agent's own
install hint); provider failures carry
`data.code` (`rate_limited`, `usage_limit_reached`, `model_not_available`, `overloaded`,
`server_error`, `connection_error`, `prompt_too_long`), optional `status`, `retry_after` and
`resetsAt`, and `data.detail` for the reason a person reads. Every message says what is true and
what to do next, and never includes a credential.

`AgentError::provider` reads a provider's JSON body out of the text it is given (Claude Code's
`API Error: 529 {"type":"error",...}`, an OpenAI `{"error":{"code":"string_above_max_length",...}}`):
the body's `type` and `code` classify the failure before its HTTP status and its prose do, a
sign-in prompt (`Not logged in · Please run /login`) is an authentication failure, a spent credit
balance is `usage_limit_reached`, and `data.detail` is the provider's sentence, never the body.

## Claude Code's CLI

The Claude agent drives one `claude` process per session (vendored `claude-code-agent-sdk`).

- **Not installed.** No `claude` to run (none found, or `CLAUDE_CODE_EXECUTABLE` names a missing
  file) is `ExecutableNotFound`, never an unreachable backend.
- **The CLI exits.** Its standard error is always drained (a full pipe would stop it) and its last
  lines are kept. When its output ends, every control request waiting on it fails at once, the turn's
  stream ends with how it ended (`Claude Code stopped unexpectedly (exit status 1): <last line>`),
  and tools it was running are ended as failed. The next prompt starts a new CLI that resumes the
  conversation by its id (`--resume`), with the session's current model and flag settings; nothing
  else on the connection waits for it.
- **A CLI that does not answer.** A control request names itself when it times out (`Claude Code
  did not answer its initialize request within 60s`); a CLI that never finished starting is killed,
  not left behind. A control response of subtype `error` is an error (a refused `set_model` is not
  reported as applied). A cancel waits at most 2 s for the CLI to acknowledge its interrupt.
- **Stop reasons.** A turn whose model stopped at `max_tokens` ends `max_tokens`, one it declined
  ends `refusal`; a result kind the adapter does not know ends the turn normally.
- **Client terminals.** The Bash tools use the client's ACP `terminal/*` methods only when the
  client advertises `terminal`; Nori does not, so commands run in the adapter and stream their output.
  A background command is always a shell of the session's own, which `BashOutput` and `KillShell`
  reach by its id.
- **Sessions** are released by `session/close` (the CLI, its tool servers and background shells are
  stopped; the conversation stays on disk) and listed from the login's own `projects/` directory
  (an account's `CLAUDE_CONFIG_DIR`), off the async path, a page of 50 at a time with a cursor.
- **Tool calls** are announced once each: the CLI shows a call when it starts (empty input) and again
  when its input is complete, and the second is a `tool_call_update`. A message kind newer than the
  adapter reaches the client whole as `_meta["_claude/rawEvent"]` of a `session_info_update`.

## Usage and rate limits

Every native agent reports usage through `Sink::usage`, which puts a `UsageReport` in `_meta.nori`
of the `usage_update` (or of a `session_info_update` when the context size is unknown). The shape
and the host's handling are in [usage.md](usage.md). An agent sends only what its provider told
it; a field it was not told is absent.

## Host tools

An agent in Nori's process gets Nori's tools as `HostBridge` (see [host-tools.md](host-tools.md)):
the specs and instructions to show the model, and a call that is scoped to the right session. How
the specs reach the provider's model is the agent's own concern.
