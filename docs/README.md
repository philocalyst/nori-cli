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
| [accounts.md](accounts.md) | Several logins of one provider: `[[accounts]]`, wrapper agents read as accounts, adding one from `/providers`, identity |
| [native-agents.md](native-agents.md) | Agents built into Nori's process: `nori-acp-kit`, registrations, errors, usage |
| [forge.md](forge.md) | GitHub pull requests and CI: `ci_status`, `pull_request`, and CI hooks |
| [warm-sessions.md](warm-sessions.md) | Agent sessions started ahead of time so switching is instant |
| [headless.md](headless.md) | `nori exec` plaintext mode and the ACP stdio facade |
| [transcripts.md](transcripts.md) | Transcript file locations and schema |
| [pickers.md](pickers.md) | The Helix picker engine, `/resume` full-text search and its on-disk index, and the actor panel |
| [usage.md](usage.md) | Agent-reported tokens and quota, the usage log, price overrides, and `/usage` |
| [workflows.md](workflows.md) | Pkl workflow policies, machine properties, hooks, and declarative retry and fallback |
| [programs.md](programs.md) | Declarative multi-actor Pkl programs: schema, runs, recovery, journals, the wire envelope, and tools |
| [tui-stage.md](tui-stage.md) | The stage: primary-screen rendering, scrollback commit, resize, `Ctrl+L`, focus-backwards rewind, status line, input routing |
| [tui-captures.md](tui-captures.md) | Rex captures and frozen replays of rendered TUI state |
| [fake-worlds.md](fake-worlds.md) | Scripted fake agents (worlds in Pkl or JSON), their faults, the virtual clock, the `nori-world` driver, and the showcase worlds |

Rules for these docs:

- Describe current behavior in prescriptive terms. No status lines, history,
  implementation plans, or investigation logs; git history keeps those.
- A PR that changes behavior covered here updates the doc in the same PR.
- Add a new doc only for a topic that crosses crate boundaries and is easy to
  get wrong. Everything else belongs in the owning crate's `docs.md`.
