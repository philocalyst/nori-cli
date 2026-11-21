# nori-codex-acp

Codex as an in-process ACP agent for Nori. It is a flavor of Nori's own agent engine
(`nori-engine`): the engine runs the loop against the Responses API directly, with no Codex core,
no app-server and no subprocess. What is Codex's here is its sign-in (`codex-login` over the
account's `CODEX_HOME`), its `config.toml` layering, its model catalog (`codex-models-manager`),
its prompts (`codex-prompts`) and the shapes of its tools (`shell_command`, `apply_patch`,
`update_plan`, `view_image`).

The crate's one entry point is `registration()`, which the `nori` binary hands to the ACP host for
`native = { agent = "codex" }` distributions. The ACP identity it reports is still `codex-acp`;
Nori keys Codex-specific behaviour on that name. See `src/flavor/mod.rs` for the module map and
`docs/native-agents.md` ("The engine", "Codex") for behaviour.
