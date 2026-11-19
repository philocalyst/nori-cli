# Headless execution

`nori exec` runs a configured ACP agent without starting the terminal UI. It has two intentionally separate interfaces: a plaintext (or `--json`) command for one-off shell use and a standard ACP stdio facade for programmatic callers.

## Plaintext mode

Pass the prompt as an argument:

```sh
nori exec "Summarize this repository"
```

Or pipe it on stdin, either by omitting the argument or by passing `-`:

```sh
printf '%s\n' "Explain the failing tests" | nori exec
printf '%s\n' "Explain the failing tests" | nori exec -
```

`nori -p` (`--print`) selects the same non-interactive mode without the
subcommand, matching the flag other agent CLIs use:

```sh
nori -p "Summarize this repository"
```

To use piped text as *context* for an instruction, ask for it with `--stdin`:

```sh
git diff | nori -p --stdin "Review this change"
```

The argument is the instruction and the piped text is the context it operates
on. The two are joined into one prompt, instruction first, separated by a blank
line.

**A prompt argument on its own never reads stdin.** Only an omitted prompt, a `-`
prompt, or `--stdin` does. This matters because a process inherits its parent's
stdin: without that rule, `nori exec "..."` inside a `while read` loop, a git
hook, or a CI step would consume input intended for something else, or block
until an unrelated pipe closed. Supplying no prompt from any source is an error.

A piped prompt is capped at 10 MiB; anything larger is rejected rather than
buffered.

Answer text is written to stdout as it streams; consecutive messages (for example the text before and after a tool call) are separated by a newline. Diagnostics, failures and progress are written to stderr, so stdout can be redirected or piped without filtering terminal rendering or progress messages. A route change (the session moved to another model or agent after a failure) is a line of its own on stdout: `· continued on X instead of Y: reason`.

### It waits for everything the prompt set going

`nori exec` drives the same turn ledger as the interactive CLI (see [turn-lifecycle.md](turn-lifecycle.md)) and does not stop at the first answer. It finishes when the root agent is idle **and** nothing is pending: no running delegated tasks, no results or wakes owed, no queued mail, no working provider subagents. Wake turns that hand delegated results back to the agent stream like any turn, and delegated-child progress appears as compact lines on stderr (`◈ child: running`, `✉ a → b: ...`).

`--max-wait <DURATION>` (`90s`, `5m`, `2h`, or plain seconds) bounds how long exec waits for pending work once the root is idle. When it expires exec prints, for example, `nori exec: stopped waiting with 2 tasks still running`, shuts the session down, and exits with status 124.

### Exit status

The exit status is the truth about the **final** turn:

| Status | Meaning |
| --- | --- |
| 0 | The final turn completed (`end_turn`) and nothing was left pending |
| 1 | The final turn failed or was cancelled, stopped early (`max_tokens`, refusal, ...), the connection or session was lost, a permission request offered no allow option, or the session could not be shut down cleanly |
| 124 | `--max-wait` expired with work still pending |

The cause is always printed on stderr first. exec never prints a success line for a failed path.

### JSON output

`nori exec --json "..."` writes every session event to stdout as one JSON object per line instead of the plaintext answer (the normalized `SessionEvent` stream: ACP notifications, requests and responses, and Nori events such as phase changes, route changes, notices and subagent updates), for scripts, live tests and benchmarks. Exit status and the wait for pending work are the same; stderr carries only failures.

Nori starts headless sessions with full access and automatically selects an allow option for ACP permission requests. If an agent offers no allow option, the request is cancelled. These settings also apply to interactive sessions. Provider-internal tools may execute without an ACP permission request and are governed by the provider itself.

## ACP stdio facade

```sh
nori exec --acp
```

This mode makes Nori an ACP agent over line-delimited JSON-RPC on stdin and stdout. There is no Nori event envelope and no raw pass-through of the downstream agent's complete event stream.

The initial facade supports:

- `initialize`
- `session/new`
- `session/set_config_option` before the prompt starts
- `session/prompt`
- `session/cancel`

`session/new` returns the downstream ACP session ID and its effective configuration options. During `session/prompt`, Nori collects the downstream assistant text and sends one complete standard `agent_message_chunk` update, followed by the correlated prompt response with the downstream `stopReason`. As in plaintext mode, Nori automatically allows permission requests that offer an allow option; it does not forward them to the caller.

Version one is deliberately bounded to one session and one prompt per process. Caller-provided `mcpServers` and `additionalDirectories` are rejected. Configure the selected agent, MCP servers, and other supported runtime settings through normal Nori configuration and command-line overrides instead.

Closing stdin cancels active work, resolves outstanding downstream activity safely, and shuts down the process. Stdout remains exclusively ACP JSON-RPC; operational diagnostics use stderr or Nori's configured tracing destination.

## Common options

```text
--agent <AGENT>   Select the configured ACP agent
-C, --cwd <DIR>  Set the execution working directory
-c <KEY=VALUE>   Apply a normal Nori configuration override
```

Headless behavior is enabled only through the explicit `exec` subcommand or its `-p` / `--print` alias. Piping into bare `nori` does not make the run headless — see below.

## Piping into an interactive session

Bare `nori` is always interactive, whether or not stdin is a pipe. Piped text
seeds the first turn and the terminal UI then starts normally, so the
conversation continues from there:

```sh
echo "Explain this repository" | nori
git diff | nori --stdin "Review this change"
```

The same rules apply as for `exec`: a prompt argument on its own leaves stdin
alone, and piped text is read only when the prompt is omitted, is `-`, or
`--stdin` is passed. Unlike `exec`, supplying no prompt at all is fine — that is
just an ordinary interactive session with an empty composer.

Because the piped prompt is submitted as the first turn, be deliberate about
what you pipe in. Redirecting a file you did not intend to send (`nori < notes`)
sends it to the provider.

This requires a controlling terminal, because the UI reads keys from it once
stdin is consumed. Stdout must still be a terminal. In an environment with
neither — a CI job or a detached process — use `nori exec` or `nori -p`, which
never open a UI. After the pipe is drained Nori re-points its stdin at the
controlling terminal, so `$EDITOR` and the file browser still work normally.
