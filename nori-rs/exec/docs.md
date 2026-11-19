# Noridoc: nori-exec

Path: @/nori-rs/exec

### Overview

- `nori-exec` is the terminal-independent frontend for finite and script-controlled Nori sessions.
- It offers a headless runner for one prompt and everything that prompt sets going, and a bounded ACP-over-stdio agent facade.
- Both surfaces execute through the same ACP harness used by the interactive TUI.

### How it fits into the larger codebase

- `nori-cli` resolves configuration, selects the mode (`nori exec` or its `-p` / `--print` alias), and owns process stdin, stdout, stderr, and exit status.
- Prompt ingestion is entirely a `nori-cli` concern: it composes the argument prompt and any piped stdin into one string and hands `run_headless` a fully resolved prompt. `nori-exec` never reads a prompt from the process itself.
- `nori-exec` prepares and activates sessions through `nori-harness`; it does not spawn agents or reduce ACP state independently.
- `nori-harness` supplies the ordered `SessionEvent` stream and preserves exact ACP request IDs used for prompt and permission correlation.
- `nori-installed` supplies the optional authenticated activity reporter. Both
  execution surfaces attach it to the downstream harness handle, so capture uses
  the same first-prompt-on-ACP boundary as the interactive frontend.
- The headless runner is a frontend of the same turn ledger the TUI drives (see `docs/turn-lifecycle.md`): it streams answer text as it arrives, separating messages with the same boundaries the live turn uses, and keeps running while the session has work in flight.
- ACP mode uses the upstream ACP SDK to present an agent endpoint while acting as a client of the configured downstream ACP agent.

### Core Implementation

- `run_headless` passes `AgentPrepareSpec` and the already-chosen
  `SessionStart::New` to `prepare_and_launch_session`, submits one text prompt,
  and then follows the ordered `SessionEvent` stream. When supplied, its reporter
  emits `nori_agent_session_started` with `session_mode = exec` after that prompt
  receives a downstream wire request ID, then performs a bounded final flush
  without changing the outcome.
- It finishes only when the root is idle **and** nothing is pending: it asks the harness
  for `pending_work()` (a turn open, prompts queued, delegated tasks running, results owed,
  mail queued, provider subagents working) and requires it to read idle twice, 400 ms apart,
  with the event stream drained in between. Wake turns and delivered mail therefore stream
  like any turn. `--max-wait <dur>` (`90s`, `5m`, `2h`) bounds the time spent waiting for
  pending work once the root is idle; when it expires exec prints `nori exec: stopped waiting
  with 2 tasks still running` (naming what is outstanding), shuts the session down, and exits.
- Output: answer text goes to stdout as it streams; a route change is a line on stdout of its
  own (`· continued on X instead of Y: reason`); retries, notices, delivered results, and
  delegated-child progress (`◈ child: running`, `✉ a → b: ...`) are compact lines on stderr.
  `--json` instead writes every session event to stdout as one JSON object per line, for
  scripts, live tests and benchmarks; stderr then carries only failures.
- Exit status is the truth about the **final** turn: 0 only if it completed (`end_turn`) and
  nothing was left pending; 1 for a failed or cancelled turn, a stop reason other than
  `end_turn`, a lost connection or ended session, a permission request with no allow option, or
  a session that could not be shut down cleanly; 124 when `--max-wait` expired with work pending.
  The cause is printed before exit. Nothing prints a success line for a failed path.
- Unattended permission requests are rejected immediately using a schema-provided reject option, with cancellation as the safe fallback.
- `run_acp` handles standard `initialize`, `session/new`,
  `session/set_config_option`, `session/prompt`, and `session/cancel` traffic
  over line-delimited JSON-RPC stdio. Its downstream `session/new` also uses
  `prepare_and_launch_session` with the explicit New choice. The facade assigns
  `session_mode = acp`; initialization and `session/new` alone are silent, and
  the attached reporter fires only after the facade's accepted `session/prompt`
  reaches the downstream ACP transport.
- The ACP facade exposes the downstream session ID and effective configuration options, then emits one complete `agent_message_chunk` before the correlated prompt response.
- Delegated permission requests are forwarded to the upstream ACP caller and their responses are returned to the downstream agent under the original downstream request ID.

### Things to Know

- The facade is intentionally bounded to one downstream session and one prompt per process.
- Analytics is optional at this crate boundary. The binary selects the mode and
  constructs the reporter; embedders that pass `None` retain identical session
  behavior without capture.
- Both headless surfaces choose `SessionStart::New` before activation and use
  `prepare_and_launch_session`. Preparation and activation retain one
  downstream subprocess even though the API keeps those lifecycle phases
  distinct.
- The combined harness path returns its handle immediately and keeps
  preparation inside the runtime's connect lifecycle: commands queue until the
  backend is ready, shutdown can win the race, the runtime emits a warning
  event after eight seconds, and an unresponsive connection is aborted after
  another thirty.
  `nori-exec` must not await `prepare_agent` outside that bounded lifecycle.
- Caller-provided MCP servers and additional directories are rejected in the first version because Nori cannot merge those inputs into an already resolved client configuration safely.
- ACP mode is a facade, not a transport trace: internal notifications and Nori-owned lifecycle events are not passed through wholesale.
- Only assistant text is projected into the final facade update. The prompt response retains the downstream ACP stop reason.
- ACP mode owns process stdin exclusively, in both directions of the contract: stdin EOF cancels active work and shuts down the downstream harness, and machine-readable stdout is owned exclusively by the ACP connection writer. The caller must therefore not consume stdin before dispatching into the facade -- any eager read (piped-prompt ingestion, in particular) belongs strictly after the ACP branch is ruled out, or the JSON-RPC stream is silently corrupted.
- The dangerous approval bypass can govern only permission requests that cross an ACP boundary; it cannot constrain or approve provider-internal tools.

Created and maintained by Nori.
