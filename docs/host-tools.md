# Host tools

Nori gives every agent a lean default catalog of communicating and conducting
tools: persistent in-process Bash (`bash`), goals (`get_goal`, `create_goal`,
`update_goal`), delegated tasks and peer-to-peer actor messaging
(`agent_capabilities`, `delegate_task`, `task_status`, `task_wait`, `task_send`,
`task_cancel`, `task_list`), and conversation programs and history
(`conversation`, `conversation_context`; see
[programs.md](programs.md)). Git (`git_status`,
`git_diff`, `git_log`, `git_commit`) and forge (`ci_status`, `pull_request`; see
[forge.md](forge.md)) wrappers are excluded from the default model-facing
catalog and only exposed when explicitly opted in via workflow `tools.enabled`.
Language-server tools (`lsp_diagnostics`, `lsp_definition`, `lsp_references`, `lsp_hover`,
`lsp_symbols`, `lsp_rename`, `lsp_call_hierarchy`; see [lsp.md](lsp.md)) are in the default
catalog, and the file tools (`read`, `write`, `edit`, `multi_edit`, `apply_patch`; see
[tools.md](tools.md)) are opt-in like git and forge.
An agent built into Nori's process gets them as **host tools**: plain function
calls into Nori, with no MCP server, port, socket or process between them. Every
other agent gets the same tools from the `nori-client` MCP server (see
[nori-client-mcp.md](nori-client-mcp.md)).

For cross-provider delegation, agents use `agent_capabilities` to discover Nori's configured
providers and models, then pass the returned values to `delegate_task`. An agent's own spawn tool
may expose only its native provider's models and is not Nori's cross-provider catalog.
The first prompt includes a cached catalog of configured agents, models and settings
when delegation tools are enabled. This adds no capability probes to startup and
leaves the static tool catalog compatible with prewarmed agent processes.
Each Nori home keeps its own in-memory catalog. Background persistence serializes
writes and merges changed agent offers under a file lock, so concurrent harnesses
cannot erase each other's remembered providers with an older snapshot.
In-flight probes are shared only within the same Nori home and agent. Typing
does not probe the active agent while its primary session is being prepared.
The catalog and other injected harness context use separate ACP content blocks
annotated for the assistant, with the static header (session preamble, catalog,
workflow rules) first so that a provider can cache it. Human prompt bytes, visible
user messages and user activity records remain unchanged; the model still receives the
context. The context is taken when the prompt is sent, not when it is admitted, so a
prompt that is refused or dropped does not use it up (see
[turn-lifecycle.md](turn-lifecycle.md#what-nori-says-along-with-a-prompt)).

Both paths serve the same `#[tool]` methods (`nori-rs/harness/src/backend/`),
so names, schemas, descriptions and replies are identical whichever way an
agent reaches them.

## How a built-in agent gets them

```text
Nori (harness)                             built-in agent (codex-acp, claude-acp)
  initialize                       ->      _meta.nori.hostTools = true
  register(tools) -> Registration
  session/new { _meta.nori.hostTools: <token> }
                                   ->      nori_host_tools::from_session_meta
                                           specs() -> the model's tool list
                                           call(name, args) when the model calls one
  tool call { _meta.nori.hostTool: "git_commit" }   <-   so the TUI knows the call
```

- The crate `nori-host-tools` (`nori-rs/host-tools`) holds the trait, the
  process-wide registry and the `_meta` keys. It depends only on std and
  `serde_json`, so that the tools cross the host/agent boundary as plain data and one
  trait object.
- An agent advertises `_meta.nori.hostTools: true` in its initialize response
  only when its client is Nori's own harness (`Host::Nori` in `nori-acp-kit`,
  declared by `Capabilities.host_tools`). Under `nori agent serve` (`Host::External`)
  the kit never advertises them, because a token means nothing to another client.
- Nori hands host tools to a connection only when the connection is in-process
  *and* the agent advertised them. The connection then sends the token as the
  `_meta` of every `session/new`, `session/load`, `session/resume` and
  `session/fork`. The registration lives as long as the backend; dropping it
  unregisters the token.
- With host tools, Nori does not start the `nori-client` server for that
  agent, and the goal tools count as connected as soon as the session exists.

## Per agent

**Codex and OpenCode.** Both run on Nori's own engine (`nori-engine`, see
[native-agents.md](native-agents.md)), in Nori's process: the host tools are handed to the
engine with the session and offered to the model beside the flavor's own tools, and Nori's
`bash` takes the place of the flavor's shell tool. A host tool's answer, an error included,
always goes back to the model, so a turn never waits forever. There is no Codex program and no
vendored Codex core.

**Claude.** The tools are an in-process SDK MCP server named `nori`. The CLI
reaches it over the stream-json pipe it already has (`mcp_message`), and names
the tools `mcp__nori__<tool>`. They are pre-allowed in every permission mode:
they are Nori's own, and Nori enforces their policy. `bash`, `delegate_task`,
`task_status` and `task_wait` carry `anthropic/alwaysLoad`, so they stay loaded;
the rest (`agent_capabilities`, `task_send`, `task_cancel`, `task_list`, the
goal tools, and any opted-in git/forge tools) are found through Claude Code's
tool search when needed. `pull_request` asks the user itself when the workflow
says to (see [forge.md](forge.md)), so being pre-allowed skips no question. The client's own MCP servers (stdio, HTTP,
SSE) are handed to the CLI's `--mcp-config` as they are; the CLI connects to
them itself.

**Antigravity.** The default `antigravity` agent is the official `agy acp` process and gets the tools
from the `nori-client` MCP server like any other subprocess agent. The opt-in in-process
`antigravity-sdk` agent (`nori-antigravity-acp`, feature `native-antigravity-sdk`) connects to a
local `localharness` over WebSocket and Protobuf and consumes Nori's host tools directly as SDK
function tools.

## File and language server tools

`nori-tools` and `nori-lsp` supply their tools as data (name, description, JSON schema), so the
harness registers them as dynamic routes (`nori_client_mcp/file_tools.rs`, `lsp_tools.rs`)
rather than `#[tool]` methods; the MCP server lists them and `host_tools::dispatch` calls them
the same way, so both paths see the same names, schemas and replies.

- **Language server tools** are default-on: no agent has them natively, and a session that
  never calls one never starts a server. Paths may be relative to the session directory.
- **File tools** are opt-in: every agent Nori runs already has its own read and edit tools (the
  Claude adapter's `Read`/`Edit`/`Write` run on `nori-tools` itself), and a second set under
  another name confuses the model. A workflow lists the ones an agent lacks in `tools.enabled`;
  `tools.disabled` and `tools.descriptions` apply as for any tool.
- One `nori_tools::Workspace` per caller (each agent keeps its own read ledger) and one
  `nori_lsp::LspManager` per session, both created lazily in the session's `NoriClientShared`
  with the session directory as root. A restricted actor's sandbox `allowed_paths` is enforced
  on the paths of every call (and on the file headers of an `apply_patch`).
- **Machines.** Each file tool takes `machine`, like `bash` (the enum and description are patched
  into the schemas by `describe_machines`). `local` is the default; another machine's tools run
  through `MachineToolFs` over its own connection, in a workspace per caller and machine rooted
  where the caller's last `bash` there ended, else this project's path or the machine's
  configured directory when it exists, else its home. Language server diagnostics are local
  only, and a restricted actor stays on this computer.
- A tool result is the model's text plus, for an `image` read, image content blocks, and for a
  change to files, `structuredContent` `{ "changes": [{ path, kind, lines_added, lines_removed,
  unified_diff }], "summary": ... }` (no file bodies).
- **Diagnostics after an edit.** After a successful `write`, `edit`, `multi_edit` or
  `apply_patch`, every changed file is announced to its language server
  (`notify_file_changed`; a deleted file is closed), the diagnostics are awaited together
  (`diagnostics_after_edit`, bounded by the server timing) and the rendered
  `<diagnostic_summary>` is appended to the result text. It is empty, and so nothing is
  appended, when no server serves the file. A session that disables `lsp_diagnostics` gets no
  diagnostics after edits.

## Tool results and cancellation

The in-process host boundary retains the complete MCP result as JSON, including
content blocks, `structuredContent`, and `_meta`; its text view remains
available to older callers. Claude's SDK server returns that MCP result as-is.
Gemini receives Nori tools through MCP and its engine handles the MCP
response; replay retains the full persisted tool state. Codex dynamic tools support text, images, and audio: Nori passes media as
data URLs, represents resource and unknown blocks as JSON text, and
includes structured content and metadata as JSON text. Conversation-program
receipts retain the original MCP result along with the text view.

On Codex turn cancellation, Nori cancels the pending host-call future and
submits an error result whose structured content identifies the cancelled call
and tool. Claude's host server tracks MCP request IDs and cancels calls on an
MCP cancellation notification or a session cancellation; its cancelled result
also identifies the request and tool. Dropping a future cannot undo an effect
already performed or stop work that a tool deliberately detached; cancellation
does not promise rollback.

## The bash tool fails closed

A field run printed a trailing `..._PASS` marker although its tests failed and a Python step
raised a `SyntaxError`. The operator concluded that "the command runner did not fail closed".
`nori-bash` (brush, in process) now has a differential harness that decides the question for
each construct: `cargo run -p nori-bash --example differential` runs every case through a fresh
`BashSession` (in a child process, so a script that signals "itself" cannot take the harness
down) and through a reference GNU bash 5 (`$DIFF_BASH`, Homebrew's, or a Nix-store bash; macOS's
`/bin/bash` is 3.2), compares status, stdout and stderr, and exits non-zero on a **dangerous**
divergence: Nori reporting success where bash fails, or printing a `PASS` bash does not.

**Conclusion.** For everything that decides fail-closed behaviour in a script (`set -e` in
functions, subshells, `$(...)`, `&&`/`||`, `!`, conditions and pipelines; `pipefail`;
`PIPESTATUS`; `ERR` traps; `exit`/`return`; 126/127; failing `python3 - <<'EOF'` and
`python3 -c` with a syntax error), brush behaves exactly like bash. The marker pattern in the
field run is therefore reproducible with `/bin/bash` itself whenever the script is not strict:
`python3 - <<'EOF' ... EOF; echo X_PASS` prints the marker, because a failing step only stops a
script under `set -e`/`set -euo pipefail` or `&&`. Nori's shell did **not** fail open there.
It did have four real divergences that can print success after a failure; they are fixed in the
vendored brush and the integration layer, and the field transcript cannot rule out the first:

| divergence (before) | effect | fix |
| --- | --- | --- |
| `$!` was empty for every background job and `wait <pid>` was unimplemented, so `cmd & wait $!` waited for all jobs and returned 0 | a background test run's failure vanished: `pytest & pid=$!; wait $pid; echo PASS` printed `PASS` | `$!` is the job's pid (a made-up pid for jobs that run inside the shell), `wait <pid>` returns that job's status, also after it was reaped (`vendor/brush/brush-core/src/jobs.rs`, `brush-builtins/src/wait.rs`) |
| the `EXIT` trap never ran | cleanup and `trap ... EXIT` reporting silently skipped; `trap 'exit 0' EXIT` could not change the status | each call is a script: its `EXIT` trap runs when it ends, with the call's own streams, then is removed (`nori-bash/src/session.rs`) |
| `kill $$`, `kill -9 $BASHPID`, `kill 0`, `kill -1` signalled the host process (`$$` is Nori's pid) | one script line could kill Nori | the script (or subshell) ends with the status the signal would give, the host is never signalled; a signal with a trap is refused loudly (`brush-builtins/src/kill.rs`) |
| a timeout or cancel left background jobs running | the tool said "timed out" while work went on | the session tracks every background task and aborts them (and their processes) on timeout or cancel (`nori-bash/src/tracked_tasks.rs`) |

Still different, by design or limitation: `kill $!`/`kill %1` on a background job fails loudly
(status 1) because such jobs run as tasks inside the host and have no process to signal;
diagnostics are worded differently (`error: command not found: x` for bash's
`bash: line 1: x: command not found`); `set -u` on an unset variable exits 1 where bash 5.3's
`-c` exits 127.

**What the tool reports.** The first block of every `bash` result is one status line: `exit 0`,
`exit 1`, `exit 143 (signal TERM)`, `killed: timed out after 120s ...`, `killed: cancelled`.
It is a block of its own, so truncation of the output cannot cut it off, and anything but
`exit 0` sets `isError`. Each stream is capped at 64 KiB (the start and end are kept). The tool
instructions tell the model to chain steps with `&&` or start with `set -euo pipefail`.

Reference: /nix/store/0xvlm2gh32710pjdswinpjqyahpfifiq-bash-5.3p20/bin/bash (GNU bash, version 5.3.20(1)-release (aarch64-apple-darwin27.0.0)). 0 dangerous, 5 differing, of 88 cases.

Timeouts (nori only, status after the call, work after a 300 ms timeout):

- timeout stops a foreground child: status `timeout after 300ms`, the work was stopped after the timeout
- timeout stops a background job: status `timeout after 300ms`, the work was stopped after the timeout

<details><summary>Differential table</summary>

| case | bash | nori | stdout | stderr | verdict |
| --- | --- | --- | --- | --- | --- |
| e: function | exit 1 | exit 1 | = | = | same |
| e: function in || | exit 0 | exit 0 | = | = | same |
| e: subshell | exit 1 | exit 1 | = | = | same |
| e: subshell exit status | exit 2 | exit 2 | = | = | same |
| e: $(...) | exit 0 | exit 0 | = | = | same |
| e: inherit_errexit $(...) | exit 1 | exit 1 | = | = | same |
| e: assignment status | exit 1 | exit 1 | = | = | same |
| e: false && true | exit 0 | exit 0 | = | = | same |
| e: true && false | exit 1 | exit 1 | = | = | same |
| e: false || false | exit 1 | exit 1 | = | = | same |
| e: pipeline last ok | exit 0 | exit 0 | = | = | same |
| e: pipeline last fails | exit 1 | exit 1 | = | = | same |
| e: pipefail | exit 1 | exit 1 | = | = | same |
| e: ! | exit 0 | exit 0 | = | = | same |
| e: if/while conditions | exit 0 | exit 0 | = | = | same |
| e: until + [[ ]] | exit 1 | exit 1 | = | = | same |
| e: ((expr)) | exit 1 | exit 1 | = | = | same |
| e: group { } | exit 1 | exit 1 | = | = | same |
| e: for body | exit 1 | exit 1 | = | = | same |
| e: command group in && | exit 0 | exit 0 | = | = | same |
| e: set +e then -e | exit 1 | exit 1 | = | = | same |
| pipefail status | exit 0 | exit 0 | = | = | same |
| pipefail off | exit 0 | exit 0 | = | = | same |
| PIPESTATUS | exit 0 | exit 0 | = | = | same |
| pipefail rightmost failure | exit 0 | exit 0 | = | = | same |
| trap ERR | exit 0 | exit 0 | = | = | same |
| trap ERR + set -e | exit 1 | exit 1 | = | = | same |
| trap ERR errtrace fn | exit 0 | exit 0 | = | = | same |
| trap EXIT | exit 3 | exit 3 | = | = | same |
| trap EXIT status | exit 1 | exit 1 | = | = | same |
| trap EXIT + set -e | exit 1 | exit 1 | = | = | same |
| trap EXIT overrides status | exit 0 | exit 0 | = | = | same |
| exit in subshell | exit 0 | exit 0 | = | = | same |
| exit in function | exit 4 | exit 4 | = | = | same |
| return code | exit 0 | exit 0 | = | = | same |
| exit in $(...) | exit 0 | exit 0 | = | = | same |
| exit status of last | exit 1 | exit 1 | = | = | same |
| exit n > 255 | exit 44 | exit 44 | = | = | same |
| exit negative | exit 255 | exit 255 | = | = | same |
| py heredoc syntax error | exit 0 | exit 0 | = | = | same |
| py -c f-string error | exit 0 | exit 0 | = | = | same |
| py heredoc + set -e | exit 1 | exit 1 | = | = | same |
| py failing assert | exit 0 | exit 0 | = | = | same |
| not found 127 | exit 0 | exit 0 | = | ≠ | same |
| not executable 126 | exit 0 | exit 0 | = | ≠ | same |
| directory 126 | exit 0 | exit 0 | = | ≠ | same |
| not found + set -e | exit 127 | exit 127 | = | ≠ | same |
| not found in && | exit 0 | exit 0 | = | ≠ | same |
| not found pipeline | exit 0 | exit 0 | = | ≠ | same |
| child killed TERM | exit 0 | exit 0 | ≠ | ≠ | differs |
| child killed KILL | exit 0 | exit 0 | = | ≠ | same |
| subshell killed ($BASHPID) | exit 0 | exit 0 | = | ≠ | same |
| kill $$ (the shell itself) | signal 15 | exit 143 | = | = | same |
| set -e child killed | exit 143 | exit 143 | = | ≠ | same |
| bg pid is set | exit 0 | exit 0 | = | = | same |
| wait $! status | exit 0 | exit 0 | = | = | same |
| wait all status | exit 0 | exit 0 | = | = | same |
| kill background job | exit 0 | exit 0 | ≠ | ≠ | differs |
| kill background %1 | exit 0 | exit 0 | ≠ | ≠ | differs |
| last command killed TERM | signal 15 | exit 143 | = | = | same |
| last command killed KILL | signal 9 | exit 137 | = | = | same |
| pipeline member killed | exit 0 | exit 0 | = | ≠ | same |
| kill 0 (process group) | signal 15 | exit 143 | = | = | same |
| PASS after failed test, no set -e | exit 0 | exit 0 | = | = | same |
| PASS with ; | exit 0 | exit 0 | = | = | same |
| PASS with && | exit 1 | exit 1 | = | = | same |
| PASS strict | exit 1 | exit 1 | = | = | same |
| PASS strict pipe | exit 1 | exit 1 | = | = | same |
| PASS strict heredoc syntax | exit 1 | exit 1 | = | = | same |
| PASS strict ok | exit 0 | exit 0 | = | = | same |
| PASS strict unset var | exit 1 | exit 1 | = | ≠ | same |
| PASS strict nested fn | exit 2 | exit 2 | = | = | same |
| PASS strict if-wrapped fn | exit 0 | exit 0 | = | = | same |
| PASS strict cmd subst arg | exit 0 | exit 0 | = | = | same |
| PASS strict && chain | exit 0 | exit 0 | = | = | same |
| PASS strict local assign | exit 0 | exit 0 | = | = | same |
| PASS strict for/read | exit 1 | exit 1 | = | = | same |
| PASS strict arithmetic | exit 1 | exit 1 | = | = | same |
| command not found in subshell | exit 0 | exit 0 | = | ≠ | same |
| cd failure + set -e | exit 1 | exit 1 | = | ≠ | same |
| source missing | exit 0 | exit 0 | = | ≠ | same |
| redirect failure | exit 0 | exit 0 | = | ≠ | same |
| redirect failure + set -e | exit 1 | exit 1 | = | ≠ | same |
| read from closed | exit 0 | exit 0 | = | ≠ | same |
| unset var -u | exit 127 | exit 1 | = | ≠ | differs |
| bad substitution | exit 0 | exit 0 | ≠ | = | differs |
| syntax error | exit 2 | exit 2 | = | ≠ | same |
| timeout | timeout after 400ms | timeout after 400ms | = | = | same |

</details>

## In the TUI

A call carrying `_meta.nori.hostTool` is recognized by that name
(`NativeCall::from_tool`, `DelegationCall::from_tool`). A call from the MCP
path is recognized from its title, which must name `nori-client` and one of the
tools. Either way the call renders as what it did: `⎇` status and history, `±`
diff with per-file `+added −removed`, `◆` commit, `◎` goal, `◉` CI, `⇡` pull request,
and delegation as subagents.

## Adding a tool

1. Write it as a `#[tool]` method on `NoriClientService` (in
   `harness/src/backend/`), with a description that says what the call does and
   what comes back, in as few words as that takes. Policy (when to call it)
   goes in the module's `INSTRUCTIONS`, once.
2. Add its arm to `dispatch` in `harness/src/backend/host_tools.rs`. The test
   `every_listed_tool_is_dispatched` fails until you do.
3. If it deserves a view, add a `NativeCall` variant, its `from_tool` arm and a
   `view` arm in `tui/src/presentation/`, with a snapshot test.
