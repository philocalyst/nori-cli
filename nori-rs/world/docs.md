# Noridoc: world

Path: @/nori-rs/world

### Overview

`nori-world` is the driver for integration tests and live demos: it boots the real session harness
against a world of fake agents (`nori-fake-agent`) and records everything that happens. It is a
crate of its own so that the harness has no test support in it and any crate can use it in a test.
The guide is [fake-worlds.md](../../docs/fake-worlds.md).

### How it fits into the larger codebase

- `WorldSession::boot` installs the world in the process, publishes its agents as the agent registry
  (`nori_acp_host::registry`, with the built-in agents unlisted), and starts a session with
  `prepare_and_launch_session` in a temporary `NORI_HOME` and working directory.
- The world's agents are in-process agents, so delegation through Nori's host tools starts real child
  sessions on other agents of the same world.

### Core Implementation

- `session.rs`: `WorldSession`, the user's verbs and the event pump. The pump records every
  `SessionEvent` and answers permission requests according to `Approvals`.
- `events.rs`: `Events`, a snapshot with a query API over lanes (the root's and each child's).
- `invariants.rs`: properties checked over the event stream and the flight recorder.
- `approvals.rs`, `boot.rs`: who answers permission requests; the claim on agent slugs.

### Things to Know

- The agent registry is process-wide. A world claims its agent slugs; worlds with the same slug run
  one at a time, and `boot` waits.
- `settle` needs a quiet moment (`WorldOptions::quiet`) longer than the harness's completion-wake
  batching (600 ms), or it can return before a child's result has woken its parent.
- User actions are written to the world's flight recorder before they are sent, so an invariant can
  tell a cancel the user made from one the harness made up.

Created and maintained by Nori.
