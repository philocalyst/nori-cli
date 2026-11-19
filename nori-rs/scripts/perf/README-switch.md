# Agent switch and time-to-first-token: measurements

Goal: switching to the Claude agent (or between agents, models, subagents) should feel
instant, under 50 ms. These numbers show where the time goes today.

Environment: macOS, debug build of `nori` (`cargo build --bin nori`, `target/debug/nori`),
Claude Code CLI 2.1.303, `haiku` model, no concurrent builds during the Claude runs.
Every prompt was "Reply with just OK". Medians and p90 are over 5 runs unless noted.

## Scripts

- `cli_floor_probe.py`: drives `claude` in stream-json mode with the arguments the
  `claude-code-agent-sdk` 0.1.39 driver uses (`--output-format stream-json --verbose
  --input-format stream-json --model M --permission-prompt-tool stdio`), sends the SDK's
  `initialize` control request, then one user message.
- `acp_switch_probe.py`: stdlib JSON-RPC client over `nori exec --acp --agent AGENT`
  stdio. Times `initialize`, `session/new`, and the first `agent_message_chunk` after
  `session/prompt`. Also attempts a second `session/new` on the same connection.
- `agent_smoke.py`: the benchmark probe for any agent (see "Agent smoke" below).
- `tui_second_session_probe.py`: pty driver for the TUI (prompt, `/new`, prompt). Unfinished;
  see "Not measured".

## Commands

```sh
cd nori-rs
cargo build --bin nori

# 1. CLI floor (5 prompts)
python3 -I scripts/perf/cli_floor_probe.py --runs 5 --model haiku --timeout 90 \
  --prompt "Reply with just OK"
# handshake only, no model call
python3 -I scripts/perf/cli_floor_probe.py --runs 5 --no-prompt --timeout 30

# 2. Through Nori (5 prompts per condition)
H=$(mktemp -d); cp ~/.nori/cli/config.toml ~/.nori/cli/agent-models.json $H/
RUST_LOG=claude_code_acp=info,nori_harness=debug,nori_acp_host=debug \
  python3 -I scripts/perf/acp_switch_probe.py --nori $PWD/target/debug/nori \
  --agent claude --home $H --runs 5 --timeout 120
# A/B: same, with CLAUDE_ACP_NO_WARM=1 and a second home
# elizacp baseline (no network): config.toml with agent = "elizacp" (CLAUDE.md "Close the Loop")
python3 -I scripts/perf/acp_switch_probe.py --nori $PWD/target/debug/nori \
  --agent elizacp --home $HE --runs 5 --timeout 60
# Codex, no prompts: needs a logged-in Codex (see Codex below)
python3 -I scripts/perf/acp_switch_probe.py --nori $PWD/target/debug/nori \
  --agent codex --home $HC --runs 5 --no-prompt

# 3. Driver phases: parse NORI_HOME/log/nori-acp.*.log for the markers
#    "Creating new ACP session", "Session ready" (elapsed_ms), "Starting prompt processing",
#    "Sending query to Claude CLI", "Successfully connected to Claude CLI" (session_id, elapsed_ms),
#    "Completed prompt task".
```

## Results

### Raw CLI floor (no Nori), haiku, 5 runs

| phase | median ms | p90 ms |
|---|---:|---:|
| spawn -> first stdout line | 953 | 1093 |
| spawn -> `control_response` (SDK `initialize`) | 1016 | 1192 |
| `initialize` only, no model call (separate 5-run set) | 1042 | 1343 |
| user message -> first assistant text | 2072 | 2217 |
| user message -> `result` | 2097 | 2251 |

`system/init` was not emitted before the first prompt in this CLI version. The user's
SessionStart hooks run on every spawn and are part of this floor.

### Through Nori: `nori exec --acp --agent claude` (debug, warm pool on)

| phase (probe clock) | median ms | p90 ms |
|---|---:|---:|
| `initialize` | 10 | 12 |
| `session/new` | 16 | 18 |
| prompt -> first `agent_message_chunk` | 4069 | 4670 |
| prompt -> `session/prompt` response | 4069 | 4670 |

### A/B: warm pool on vs `CLAUDE_ACP_NO_WARM=1` (5 runs each)

| condition | initialize med / p90 | session/new med / p90 | first chunk med / p90 |
|---|---:|---:|---:|
| warm on | 13 / 5003* | 18 / 40 | 3568 / 4274 |
| warm off | 19 / 43 | 15 / 19 | 3863 / 4070 |

\* one run's `initialize` took 5 s (the process overlapped a refill); the other four were
about 10-13 ms. A single probe session per process cannot show the pool's benefit, because
the pool serves a *second* session. This A/B measures only that the pool does not hurt
the first turn.

### Driver phases from the log (same 5 rounds as the first table)

| phase | median ms | p90 ms |
|---|---:|---:|
| `session/new` -> "Session ready" (session CLI connected) | 1033 | 1458 |
| prompt waits for that connect (probe sends prompt right after `session/new`) | 1027 | 1451 |
| `session/prompt` send -> "Completed prompt task" (includes model) | 3103 | 3296 |

The probe's prompt-to-first-chunk (4069 ms) is about 1.0 s longer than the raw CLI
first-text (2072 ms). That gap is the session connect the probe waits for, because it sends
the prompt immediately after `session/new`. A real client that waits for the user to type
does not pay it.

### Through Nori: `elizacp` (no network baseline)

| phase | median ms | p90 ms |
|---|---:|---:|
| `initialize` | 17 | 28 |
| `session/new` | 23 | 277 |
| prompt -> first chunk | 4 | 6 |

Nori's facade and the ACP plumbing cost tens of milliseconds, not hundreds.

### Codex (in-process, `--agent codex`), no prompts, 5 runs

| phase | median ms | p90 ms |
|---|---:|---:|
| `initialize` | 19 | 118 |
| `session/new` | not measured | |

`session/new` fails with "Authentication required for Codex ACP. Run /login ..." in about
0.2 s. This machine has no Codex credentials (no `auth.json` under `~/.codex`, no
`OPENAI_API_KEY`), so `load_agent` / `CodexAgent::new` / `start_thread` timings could not be
captured. The log has no timing lines for them.

## Findings

1. The warm-pool standby is not gated at agent creation. `nori-rs/claude-acp/src/agent/runner.rs:397`
   calls `warm::refill` immediately. The gated refill in `agent/handlers.rs:198`
   (`wait_until_idle(REFILL_QUIET)`) is a separate path. In the warm-on log, the standby
   session (different session_id, created at session creation) connects about 12 ms
   after the first query is sent, during the first turn.
2. A correction to the earlier draft: the second "Successfully connected to Claude CLI" per
   prompt is that standby, not a per-prompt query process. The query path uses the session's
   own CLI. The "1.34 s second-spawn" figure in the earlier draft therefore does not belong on
   the query's critical path.
3. The probe cannot test the switch case: the exec ACP facade holds one session per
   connection, so a second `session/new` returns "Invalid params". Switch timing needs the TUI
   or a second process.
4. The driver logs `rate_limit_event` as an unknown message type on stderr
   ("unknown variant `rate_limit_event`"). Harmless to timing, but noisy.
5. `nori exec` with the default config starts two MCP servers for the Claude session
   (`mcp_server_count=2`). The driver answers the CLI's MCP `initialize` and `tools/list` at
   about +1.3 s after the query is sent, which falls inside the first-chunk window.

## Not measured

- **Second session in one TUI process (warm claim).** `tui_second_session_probe.py` starts the
  TUI and creates a session, but its keystrokes did not submit a prompt. The TUI did not
  accept the typed prompt, and no prompt was sent. Neither tmux nor Homebrew is installed on
  this machine, so the harness needs a fix (for example, the answers to the kitty-keyboard and
  OSC queries that `pty_probe.py` already provides). The warm claim is therefore unmeasured.
- **Codex `load_agent` / `start_thread` timings** (no Codex credentials).
- **`--profile perf` build.** Not built, to keep the shared target free of a second release
  build; the debug numbers above are the ones measured.
- **MCP attribution by config.** The user config has no `[mcp_servers]` table, so the
  "with and without configured MCP servers" comparison is a no-op. The sdk MCP server test
  (`cli_floor_probe.py --mcp-sdk`) hangs without an SDK host answering `mcp_message`, so it
  cannot give a timing either.

## Three changes that would most cut switch time and TTFT

1. **Warm claim for `/new`, agent switch, and delegated tasks.** The session CLI connect is
   about 1.03 s median (p90 1.46 s) and lands on the first prompt if the user types quickly.
   A claimed warm CLI removes it from the switch path. Evidence: the connect phase above. Gate
   the agent-creation refill too (finding 1), so the standby does not compete with the first turn.
2. **Start the next session's connect before the user acts, and keep it idle-gated.** The
   standby should wait for idle (already in `handlers.rs`) and also at agent creation
   (`runner.rs:397`). Evidence: the 12 ms overlap in the warm-on log, and the A/B showing the
   pool does not help the first turn yet.
3. **Shrink the CLI's own startup.** The user's SessionStart hooks and the MCP servers the
   driver passes run on every spawn. The raw CLI floor (about 1.0 s to initialize, about 2.1 s
   to first text) is mostly the CLI, not Nori. Lazy MCP startup or fewer hooks would cut every
   spawn. Evidence: the raw floor table, and the MCP handshake timing in finding 5.

## Agent smoke (`agent_smoke.py`)

One agent, end to end, repeatable and machine-readable: it starts a fresh agent per run
(`nori agent serve AGENT`, or `--via facade` for `nori exec --acp --agent AGENT`, which is how
agents that are not built in, such as `antigravity`, are reached), then `initialize`,
`session/new`, optionally `session/set_config_option` for the model, and one prompt. It uses the
accounts the agent is already logged in to and never reads credentials. Stdlib only.

```sh
cd nori-rs
python3 -I scripts/perf/agent_smoke.py --nori $PWD/target/debug/nori --agent claude --model haiku --runs 5
python3 -I scripts/perf/agent_smoke.py --nori $PWD/target/debug/nori --agent codex --model gpt-6-luna --runs 5 --json
python3 -I scripts/perf/agent_smoke.py --nori $PWD/target/debug/nori --agent opencode \
  --model opencode-go/deepseek-v4.1-flash --runs 5
python3 -I scripts/perf/agent_smoke.py --nori $PWD/target/debug/nori --agent antigravity --via facade --no-prompt
```

Options: `--nori`, `--agent`, `--model` (selected with `session/set_config_option` after
`session/new`), `--runs N`, `--json`, `--prompt` (default "Reply with just OK"), `--no-prompt`
(stop after `session/new`), `--via serve|facade`, `--option K=V` (passed to `agent serve`),
`--home` (NORI_HOME; default a fresh temp dir), `--timeout` (seconds per request, default 120).

Per run, in milliseconds: `initialize_ms`, `session_new_ms`, `set_model_ms` (with `--model`),
`first_chunk_ms` (prompt to first `agent_message_chunk`), `prompt_done_ms`; plus `stop_reason`,
`reply`, `agent_info`, `capabilities`, and `usage`: `present` (any `_meta.nori` report arrived on a
`usage_update` or `session_info_update`), `valid` (every report satisfies the contract in
`docs/usage.md`: token fields are non-negative integers, `tokensScope` is `session` when present,
rate-limit windows have `id` and `label`, `usedPercent` in 0..100, `resetsAt` RFC 3339,
`windowMinutes` a positive integer), `problems`, `reports`, and `last`, the final report
verbatim. A failed run has `error` (and `stderr_tail`) and no timings after the failure.

Without `--json` it prints a table and a median/p90 summary per timing. With `--json` stdout is
exactly one object, keys sorted, so it can be compared run to run:

```json
{"schema": 1, "agent": "claude", "model": "haiku", "via": "serve", "prompt": "Reply with just OK",
 "runs": [{"error": null, "initialize_ms": 33, "session_new_ms": 1, "set_model_ms": 658,
           "first_chunk_ms": 1959, "prompt_done_ms": 1973, "stop_reason": "end_turn",
           "reply": "OK", "usage": {"present": true, "valid": true, "problems": [], "reports": 2,
                                    "last": {"sessionUpdate": "usage_update", "...": "..."}}}],
 "summary": {"runs": 5, "ok": 5, "usage_valid": 5,
             "initialize_ms": {"n": 5, "median": 33, "p90": 3682}, "...": "..."}}
```

`summary.<timing>` is `null` when no run produced it. The exit status is 0 when every run
completed and, unless `--no-prompt`, its usage meta was valid; 1 otherwise. The first run of a
process can be far slower than the rest (cold binary and CLI start): compare medians.

