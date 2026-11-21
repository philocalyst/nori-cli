# Noridoc: fake-agent

Path: @/nori-rs/fake-agent

### Overview

`nori-fake-agent` is a scriptable, deterministic ACP agent for integration tests, demos and
terminal captures. A **world** (Pkl or JSON) declares fake agents and, per agent, the scenes it
plays; the crate interprets them on top of `nori-acp-kit`. The full guide is
[fake-worlds.md](../../docs/fake-worlds.md).

### How it fits into the larger codebase

- Built on `nori-acp-kit` (`NativeAgent`, `Sink`, `TurnGate`, `Options`, `UsageReport`,
  `AgentError`, `HostBridge`, `Registration`) and `nori-pkl` (the native evaluator). It depends on
  neither the harness nor any real agent.
- `registration()` registers the agent `fake`; the application registers it with the host
  (`nori-cli`'s `fake_world` module) and selects it with `native = { agent = "fake" }`.
- `nori-world` boots the real harness against a world; `nori --world` and `nori agent serve fake`
  serve it to the TUI and to any ACP client.

### Core Implementation

- `world/`: serde model (`World`, `AgentDef`, `Scene`, `Step`), `value.rs` (durations, text,
  chunking), `load.rs` (Pkl and JSON), `validate.rs` (errors that name the step). The Pkl schema is
  `schema/World.pkl`, embedded as the module `nori:World`; the Rust types and the schema are the
  same shape and change together.
- `exec/`: the interpreter. `Ctx` is one task's view (its session lane, its share of the clock,
  whether a cancel interrupts it); a step that ends the turn breaks out with `Break`.
  `turn.rs` selects the scene and applies the cancel policy, `tools.rs` runs tool lifecycles and
  permissions, `host.rs` calls Nori's host tools, `faults.rs` holds faults and composition,
  `provider.rs` is the provider behind the agent (the `limits` a prompt is refused over, in the
  provider's own error body).
- `world/provider.rs`: `Limits`, `ConfigFault` (a setting that is acknowledged and ignored,
  substituted or refused) and the `ConfigUpdate` step.
- `wire.rs`: the wire tap. It forwards JSON-RPC lines between the host's pipe and the kit and applies
  faults the kit cannot express (dropped, repeated and stale responses, noise, crash, slow
  `initialize`).
- `clock.rs`: virtual and real time. Virtual time jumps to the next deadline when every participant
  sleeps; external waits hold time back for a short real-time grace.
- `runtime.rs`, `session.rs`, `record.rs`: state shared by a world's agents (clock, counters,
  sessions that outlive a connection) and the flight recorder.

### Things to Know

- Updates that the typed ACP schema has no type for (`subagent_update`, `session_message`) and
  deliberately malformed ones go through `acp::UntypedMessage` on the same connection as typed
  updates, so order is kept.
- Scenes match the user's words: Nori's `<nori_*>` context and `<conversation-checkpoint>` blocks are
  stripped from the prompt first.
- A `Permission` that times out fails its tool call; `ToolStart` leaves one open on purpose.
- Faults that need the tap report a divergence when the agent is served without one.
- A refused prompt (`limits`) never reaches a scene, and is recorded as `PromptRefused`, not as a
  divergence: the provider refusing is the world working. An agent whose models are announced later
  (`startup.models_announced_after`) offers no model option at `session/new`.
- The suite that plays these faults against the harness is `nori-rs/world/tests/`; see
  [fault-catalog.md](../../docs/fault-catalog.md).
- Agent-facing strings in scripts are authored text; the crate adds none of its own beyond the
  echo for an unmatched prompt.

Created and maintained by Nori.
