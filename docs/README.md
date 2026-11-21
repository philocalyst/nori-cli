# Nori CLI Docs

One prescriptive document per cross-cutting topic. Crate-level details live in
each crate's `docs.md`.

| Doc | Topic |
| --- | --- |
| [glossary.md](glossary.md) | Canonical terms for actors, turns, sessions, and tools |
| [turn-lifecycle.md](turn-lifecycle.md) | Turn state, `session/update` routing, and cancellation |
| [remote-control.md](remote-control.md) | Remote ACP transport behind `--remote` and `/remote-control` |
| [nori-client-mcp.md](nori-client-mcp.md) | The backend-owned `nori-client` MCP server and goals |
| [monitors.md](monitors.md) | Background watchers an agent arms instead of polling: sources, rules, wake modes, delivery through the turn ledger, journal and resume |
| [host-tools.md](host-tools.md) | Nori's tools handed straight to built-in agents, with no MCP server |
| [machines.md](machines.md) | The user's other computers: the roster (Rex hosts, `[machines]`), `bash` on a named machine over ssh, the job supervisor, and moving a whole session to another machine |
| [resilience.md](resilience.md) | Disk full, long disconnects, usage limits waited out to their reset, and crashes: what keeps going and what the person is told |
| [tools.md](tools.md) | The shared agent file tools (`read`, `write`, `edit`, `multi_edit`, `apply_patch`): catalog and aliases, the read ledger, the tiered edit engine, results |
| [accounts.md](accounts.md) | Several logins of one provider: `[[accounts]]`, wrapper agents read as accounts, adding one from `/providers`, identity |
| [native-agents.md](native-agents.md) | Agents built into Nori's process: `nori-acp-kit`, registrations, errors, usage |
| [forge.md](forge.md) | GitHub pull requests and CI: `ci_status`, `pull_request`, and CI hooks |
| [responsiveness.md](responsiveness.md) | The 100 ms rule: what runs off the event loop, the latency table (`NORI_LATENCY_LOG`), and changes made during a response |
| [warm-sessions.md](warm-sessions.md) | Agent sessions started ahead of time so switching is instant |
| [headless.md](headless.md) | `nori exec` plaintext mode and the ACP stdio facade |
| [transcripts.md](transcripts.md) | Transcript file locations and schema |
| [pickers.md](pickers.md) | The Helix picker engine, `/resume` full-text search and its on-disk index, and the actor panel |
| [nudox.md](nudox.md) | nudox code intelligence in every session by default (`nudox = false` opts out) and the built-in `impact` skill |
| [lsp.md](lsp.md) | Language servers: the registry, lifecycle, the diagnostics-after-edit contract, request APIs and the agent-facing `lsp_*` tools |
| [usage.md](usage.md) | Agent-reported tokens and quota, the usage log, price overrides, and `/usage` |
| [pkl-runtime.md](pkl-runtime.md) | Nori's programmable core: one Pkl evaluator and catalog for worlds, policies, jobs, monitors (triggers and actions), skills and plugins |
| [workflows.md](workflows.md) | Pkl workflow policies, machine properties, hooks, and declarative retry and fallback |
| [programs.md](programs.md) | Declarative multi-actor Pkl programs: schema, runs, recovery, journals, the wire envelope, and tools |
| [actions.md](actions.md) | Actions defined once (`nori-actions`): the palette (`Alt+X`, Helix `:`), typed `/name value`, directives applied on send |
| [editor.md](editor.md) | The one text editor (composer and walked-back prompts): vendored modalkit, the Helix, Vim and modeless keymaps, stable layout, chips |
| [tui-stage.md](tui-stage.md) | The stage: primary-screen rendering, scrollback commit, resize, `Ctrl+L`, focus-backwards rewind, status line, input routing |
| [terminal-integration.md](terminal-integration.md) | What Nori tells its terminal: the Program Status Protocol (OSC 7501), records for the conversation and each subagent, detection, rate limits, clearing, `NORI_PROGRAM_STATUS` |
| [tui-captures.md](tui-captures.md) | Rex captures and frozen replays of rendered TUI state |
| [fault-catalog.md](fault-catalog.md) | Every ACP coordination failure the fake agent plays against the real harness, with the test that reproduces it and `just faults` |
| [fake-worlds.md](fake-worlds.md) | Scripted fake agents (worlds in Pkl or JSON), their faults, the virtual clock, the `nori-world` driver, and the showcase worlds |

Rules for these docs:

- Describe current behavior in prescriptive terms. No status lines, history,
  implementation plans, or investigation logs; git history keeps those.
- A PR that changes behavior covered here updates the doc in the same PR.
- Add a new doc only for a topic that crosses crate boundaries and is easy to
  get wrong. Everything else belongs in the owning crate's `docs.md`.
