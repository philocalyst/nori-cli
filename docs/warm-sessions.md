# Warm sessions

Starting an agent should feel instant. For a built-in agent the ACP side already
is: the connection is an in-memory pipe and `initialize` is a function call.
What remains is the agent's own start-up, and for Claude that is the `claude`
CLI: about one second from spawn to its `initialize` reply on a typical
machine, longer with SessionStart hooks. A **warm session** pays that second
before anyone asks for the session.

## What is warm

A warm session is a whole Claude driver session (`claude-acp`
`session::warm`) whose CLI is spawned and connected, with no ACP connection
bound yet. It is keyed by everything that shapes the CLI's arguments and
environment:

- the working directory,
- the driver configuration (CLI path, model, base URL, credentials, thinking
  budget),
- the client's MCP servers, and
- whether the session gets Nori's host tools.

The pool is process-wide, because an agent switch opens a new ACP connection.
It holds at most two warm sessions; the oldest is stopped first, and one older
than 30 minutes is never used (the user's settings may have changed).

## How sessions use it

- A plain `session/new` takes the warm session that fits it, binds its host
  tools (see [host-tools.md](host-tools.md)), and adopts it under the warm
  session's id. "Plain" means no `_meta` that changes how the CLI starts:
  no system prompt override, no `claudeCode` options, no resume or fork.
- `session/load`, `session/resume` and `session/fork` never take one: they
  start the CLI with `--resume`.
- After a plain new session's own CLI is up, the driver starts the next warm
  session for the same key, so the two never compete for start-up time.
- A warm session lists Nori's tools from the published catalog
  (`nori_host_tools::Catalog`) until it is claimed; calls made before then fail
  with an error, which cannot happen in practice because no prompt reaches it
  first.
- A new Claude conversation is started with `--session-id` set to the ACP
  session id, so loading or resuming it later by that id finds it.

## How Nori primes it

When a root session of any agent is created, the harness
(`backend::prewarm`) asks every other installed built-in agent to prewarm for
the same working directory and MCP servers, through
`nori_acp_host::in_process::prewarm`. Only agents that register a prewarmer do
anything; today that is Claude (its `Registration` in `nori-claude-acp`). Delegated child
sessions do not prime others. The result: switching from Codex to Claude, a
`/new` on Claude, and a task delegated to Claude all find a running CLI.

## Cost

Each warm session is one idle `claude` process. When Nori exits, the CLI sees
its stdin close and exits too.
