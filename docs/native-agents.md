# Native agents

An agent built into Nori's process, with no subprocess and no adapter between it and the **ACP
host**. Nori gives it one end of an in-memory pipe and keeps the other end for its own ACP client.
Every native agent is built on one crate, `nori-acp-kit` (`nori-rs/acp-kit`), and nothing else
holds in-process plumbing.

| Crate | Registration | What it drives |
| --- | --- | --- |
| `nori-codex-acp` | `codex` | Codex's own engine, linked in |
| `nori-claude-acp` | `claude` | Claude Code's CLI (the only process it starts) |
| `nori-opencode-acp` | `opencode` | A running OpenCode HTTP server |
| `nori-gemini-acp` | `gemini` | Gemini Core in a Node process (opt-in feature `native-gemini`) |
| `nori-antigravity-acp` | `antigravity-sdk` | Antigravity's local harness over its SDK (opt-in feature `native-antigravity-sdk`) |

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

`nori-acp-kit` depends on the ACP library the host speaks and on `nori-host-tools`; it re-exports
both as `acp` and `schema`, so no agent pins a protocol version. It depends on neither
`nori-acp-host` nor any agent crate.

## Serving one to any client

`nori agent serve <slug> [KEY=VALUE...] [--home DIR] [--env KEY=VALUE]` serves a registered agent over
stdio ACP (the process environment, with `--env` on top, is what the agent's resolver reads), so any
ACP client can run any native agent. Only ACP goes to stdout. The kit treats that client as
`Host::External`: it never advertises host tools, which only Nori's own harness can supply.

## Turns

Every agent answers "what does a prompt do while a turn runs" through one `TurnGate` per session and
a `TurnPolicy`: `Exclusive` refuses it (`Busy`), `Steerable` folds it into the running turn
(Codex), `Replace` cancels the running turn, waits a bounded time for it to end, and takes its place
(Claude). A `Turn` ends when dropped, and `gate.cancel()` cancels every turn that is running.
No agent keeps its own turn bookkeeping.

## Errors

Agents fail with `AgentError`. It converts to the ACP error the host classifies (`classify_failure`
in `nori-acp-host`): authentication is `-32000`, an unknown session `-32002`, an unreachable
backing service `-32010`, a session that cannot be reattached `-32012`; provider failures carry
`data.code` (`rate_limited`, `usage_limit_reached`, `model_not_available`, `overloaded`,
`server_error`, `connection_error`, `prompt_too_long`), optional `status`, `retry_after` and
`resetsAt`, and `data.detail` for the reason a person reads. Every message says what is true and
what to do next, and never includes a credential.

## Usage and rate limits

Every native agent reports usage through `Sink::usage`, which puts a `UsageReport` in `_meta.nori`
of the `usage_update` (or of a `session_info_update` when the context size is unknown). The shape
and the host's handling are in [usage.md](usage.md). An agent sends only what its provider told
it; a field it was not told is absent.

## Host tools

An agent in Nori's process gets Nori's tools as `HostBridge` (see [host-tools.md](host-tools.md)):
the specs and instructions to show the model, and a call that is scoped to the right session. How
the specs reach the provider's model is the agent's own concern.
