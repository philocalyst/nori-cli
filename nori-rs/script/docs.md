# Noridoc: nori-script

Path: @/nori-rs/script

### Overview

The ACP harness uses `nori-pkl` for active workflow policy and conversation
programs. See `@/docs/workflows.md` and `@/docs/programs.md`.
`nori-script` supplies shared `Workflow`, `Hook`, `CommitRules`, and `PrRules`
data types and their rendering. Its legacy Steel evaluator remains available
to library callers and compatibility tests; Nori does not invoke it for ACP
workflow loading. The evaluator APIs described below are that legacy surface.

### How it fits into the larger codebase

The crate depends only on `steel-core` (features: `std`). Its
callers never see Scheme:

- `ScriptHost::spawn(global, project)` starts a thread that loads
  `$NORI_HOME/init.scm` and, for a trusted directory, `<repo>/.nori/nori.scm`,
  then serves `loaded()` and `apply_inline(prompt)` from one warm engine.
- `load(global, project)` does the same loading on the calling thread and
  returns `Loaded`; `Loaded::apply_inline(prompt)` re-runs the scripts in a
  fresh engine per call.
- `Workflow::instructions()` renders the rules for the agent's first prompt.
- `Workflow::hook_prompts(event)` lists what to send when a session event
  happens.

### Core implementation

| Module | Owns |
| --- | --- |
| `model.rs` | The public data types. |
| `dsl/` | The `nori/workflow` builtin module, argument checking, and the Scheme prelude that defines `define-workflow`. |
| `sandbox.rs` | The denied names and modules, and the static check run before Steel sees a source. |
| `engine.rs` | Engine creation (denied globals rebound, denied modules emptied), evaluation under a watchdog, and error mapping. |
| `load.rs` | Script paths and loading order. |
| `host.rs` | `ScriptHost`: the thread that owns a long-lived engine. |
| `inline.rs` | Finding DSL in a prompt and applying it. |
| `render.rs` | The instruction text. |

### Things to know

- Steel engines are not `Send` (the `sync` feature is off), so an engine lives
  on the thread that made it: the `ScriptHost` thread, or the caller of `load`
  or `Loaded::apply_inline`. `Loaded` keeps the script text, not the engine,
  so it is `Send + Sync`.
- `Engine::new_sandboxed()` costs about 90 ms optimized and 2 to 3 s in a
  debug build, every time, on any thread; running a script costs about 1 ms.
  That is why `ScriptHost` exists. Tests that create engines are slow in debug
  builds for the same reason.
- Steel is never given a script's path: with one it reads the file itself.
  Errors are located against the source text instead.
- Steel expands macros in its compiler, out of the watchdog's reach, so user
  macro definitions are denied outright (a self-referential `syntax-rules`
  macro hangs or overflows the stack).
- `Engine::new_sandboxed()` alone is not a sandbox: it still binds `command`,
  `env-var`, `eval`, `Engine::new`, and terminal I/O. Every layer in
  `sandbox.rs` is load-bearing.
- `reviewed_globals.txt` lists every Steel global judged safe. The
  `every_global_is_reviewed` test fails when a Steel upgrade adds a global that
  is neither denied nor reviewed.
- Unshadowed references to builtins compile to `#%prim.<name>`, so the engine
  rebinds both spellings of each denied global.
