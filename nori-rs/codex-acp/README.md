# nori-codex-acp

Codex as an in-process ACP agent for Nori, built on `nori-acp-kit`. Nori links Codex's core into
its own process, so there is no adapter binary: the crate's one entry point is `registration()`,
which the `nori` binary hands to the ACP host for `native = { agent = "codex" }` distributions.

Vendored from zed-industries/codex-acp (see `UPSTREAM.md`) and reworked: the connection, request
routing and session table belong to the kit, and the translation of Codex's events into ACP updates
is split by event family under `src/thread/` (see the module docs there). The ACP identity it
reports is still `codex-acp`; Nori keys Codex-specific behaviour on that name.

- Usage: each `TokenCount` event becomes a `usage_update` whose `_meta.nori` carries the model, the
  last response's token breakdown and Codex's rate-limit windows.
- Failures: a failed turn carries Codex's own error class (`codex_error_info`) plus the kit's
  structured fields, so the host can retry, fall back or ask for a login without parsing prose.
- Host tools: Nori's tools reach the model as Codex dynamic tools (`src/host_tools.rs`).
