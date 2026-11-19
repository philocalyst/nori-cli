# Monitors

A **monitor** is a background watcher an agent arms with `monitor_start` and then stops thinking
about. It is how an agent waits: for a build, a test run, CI, a dev server, a log, a file, a URL or a
peer. The agent never polls and never holds its turn open. It arms the monitor and ends its turn, and
what the monitor sees reaches it as a new message, through the same turn ledger as every other prompt
nobody typed ([turn-lifecycle.md](turn-lifecycle.md)).

Monitors are Nori host tools ([nori-client-mcp.md](nori-client-mcp.md), [host-tools.md](host-tools.md)).
The agent-facing guidance is `nori_host_tools::MONITOR_INSTRUCTIONS`, delivered with the tools next to
the delegation instructions.

## Tools

| Tool | What it does |
| --- | --- |
| `monitor_start` | Arm a monitor; returns at once with its id. |
| `monitor_list` | Every monitor with its state, counts and expiry, plus the events the agent has not been given yet (consumed by the read). |
| `monitor_stop` | Stop one by id or label, or `all`. Returns its unread events. |
| `monitor_rearm` | Start an ended, expired or dormant monitor again (fresh timeout and budget; `timeout_seconds`, `max_events`, `wake` may change), or extend a running one's timeout. |

### `monitor_start`

The schema is flat: `label`, `source`, the fields of that source, and the options common to all.
Fields that belong to another source are an error, not ignored.

| Source | Fields | Events |
| --- | --- | --- |
| `command` | `command`, `cwd`, `stderr` | Each stdout line (stderr too with `stderr: true`). The command runs in its own in-process `nori-bash` shell, under the actor's sandbox. When it exits, its exit code is the last event; non-zero is a failure and carries the last stderr lines. |
| `file` | `path`, `from_start` | Lines appended, as `tail -F`: rotation and truncation are followed, removal is waited out. |
| `watch` | `paths`, `kinds` (`created`, `modified`, `deleted`) | `created src/a.rs` and the like, read on an interval, so a burst is one event. `.git`, `node_modules`, `target` below a root are skipped. |
| `actor` | `actor`, `track` (`state`, `results`, `replies`) | The actor directory's view of a peer: state changes, results of its tasks, what it writes to this actor. Children this actor delegated to already wake it with their results. |
| `ci` | `sha`, `branch` | Each check as it fails, then the verdict with the end of each failed job's log. Ends there. Default commit: the branch's pushed commit. |
| `http_poll` | `url`, `interval_seconds`, `json_path` or `regex` | The extracted value, only when it changes. Without an extractor, the HTTP status. The first reading is a baseline and is not an event unless a rule matches it. Three failed reads in a row are one failure event; answering again is one event. No credentials: use `command` with curl. |

Common options:

| Option | Meaning |
| --- | --- |
| `filter` | Regex; only matching lines are events. |
| `fail_on` | Regexes; a match is always an event, marked FAILED, whatever `filter` says. |
| `until` | Regex; the monitor ends after emitting the first matching line. |
| `batch_ms` | Lines this close together are one event (300; 600 for `watch`). A monitor that emits five events within ten seconds doubles its window up to five seconds. |
| `max_events` | Default 50, at most 500. At the limit the monitor stops and says it flooded. |
| `timeout_seconds` | Default 1800 (2700 `ci`, 3600 `file` and `watch`), at most 28800. Expiry is reported, with the event count, as a failure: nothing observed says the wait is over. |
| `wake`, `digest_seconds` | See below. |

Limits: eight running monitors per actor; a label is unique among the running ones; a line is cut at
2000 characters, a burst keeps 40 lines of each kind, a wake message holds 24 KiB (failures first);
at most 200 events wait for the agent (the oldest informational ones are dropped, and the wake says
how many).

## Delivery

Events are queued per actor and delivered by `AcpBackend::deliver_monitor_events_if_idle`, which
offers one hidden prompt (`QueuedPromptKind::MonitorEvents`, `<monitor_events batch="...">`) to the
ledger. The ledger starts it only if no turn of any owner is open at that instant; a running turn is
never interrupted. Refused, the events go back where they were and are offered at the next turn
boundary.

| `wake` | Idle agent |
| --- | --- |
| `immediate` (default) | Woken as soon as the burst closes. |
| `digest` | Woken once per `digest_seconds` (default 30), with everything gathered. |
| `boundary` | Not woken: events come after the next turn that is not itself a monitor wake. |

A failure event, and the end of a monitor that failed, wake the agent whatever the mode. Six wakes in a
minute is a storm: informational events are gathered into a thirty-second digest until it passes. A
lease takes every queued event, so a wake carries everything that is waiting, in order.

Invariants:

1. An event is journaled before it is queued, queued once, leased once, delivered once. A tool result
   that carries events (`monitor_list`, `monitor_stop`) consumes them.
2. A monitor says how it ended: its exit code, that it expired with N events, that it flooded, that
   its source failed. A source that stops without saying why is a failure. Nothing reports success
   that was not observed.
3. A monitor survives the end of the agent's turn. It ends with its actor's session, on timeout, on
   `until`, at `max_events`, or on `monitor_stop`. A child's monitors belong to the child's own
   session and wake the child, not its parent. A provider's own child cannot arm monitors.
4. A rewind stops every monitor and drops what they had not delivered: the agent no longer remembers
   arming them.

## Journal and resume

Each change is one `monitor` command in the conversation journal: monitor armed, event queued, batch
offered, batch delivered, events consumed, monitor ended. On resume, folding them restores:

- monitors that were armed and not ended, as **dormant**: nothing runs, `monitor_list` says
  `dormant`, and the first wake carries a note saying what each was watching and that
  `monitor_rearm` restarts it;
- events nobody was handed, queued again. An event counts as handed if its batch was delivered, a
  tool result carried it, or the conversation itself contains the batch's message, so a crash
  between the agent receiving a batch and the journal saying so neither loses nor repeats it.

A shutdown does not journal an end for running monitors: that is what makes them dormant.

## CI and workflows

The workflow's CI hooks ([forge.md](forge.md), [workflows.md](workflows.md)) are a `ci` monitor armed by
Nori after a turn that pushed. It is labelled `ci`, a newer push replaces it, and it carries the
workflow's `on-ci-fail` or `on-ci-pass` prompt, appended to the verdict. A pass the workflow has no
prompt for does not wake the agent; a failure always does. A commit that shows no checks within three
minutes ends the watch quietly when a workflow armed it, and as a failure ("nothing observed says the
checks passed") when an agent did.

## What the user sees

The status line shows `◉ 2 monitors` while monitors run. Each delivered batch is one transcript row
per monitor: `◉ build · 3 events · FAILED: error[E0308] mismatched types`, in theme tokens (coral for a
failure, mint for a monitor that ended, cyan otherwise). `nori exec` prints one line per monitor.

## Sources of truth

`nori-rs/harness/src/backend/monitor/` (`spec`, `source/`, `pipeline`, `registry`, `report`, `lease`,
`view`, `restore`, `delivery`, `journal`, `text`, `request`), the tools in
`backend/nori_client_mcp/monitor_tools.rs`, the actor source's view of the directory in
`backend/orchestrator/actor_watch.rs`, the wire type `nori_protocol::MonitorWire`, and the scripted
world `nori-rs/fake-agent/worlds/monitors.pkl` ([fake-worlds.md](fake-worlds.md)).
