# Language servers

Nori gives agents and the editor compiler-grade code intelligence through the language servers
already installed on the machine. The `nori-lsp` crate (`nori-rs/lsp`) starts servers on demand,
keeps their documents in sync, collects their diagnostics, answers code-intelligence requests in
plain Nori types and exposes the agent-facing tools. Its client is Helix's LSP client, vendored
without the editor (see `vendor/helix/NORI.md`).

Nori never installs a language server. A server is used only when its binary is on `PATH`; when none
is, the answer names what to install.

## Servers

Servers are looked up by **language** (the protocol's `languageId`, derived from the file extension
or file name) and started per **root**: one server process per (server, project root). The built-in
table covers rust-analyzer, typescript-language-server, pyright, basedpyright, pylsp, gopls,
clangd, zls, lua-language-server, bash-language-server, nixd, nil, ruby-lsp, jdtls,
kotlin-language-server, sourcekit-lsp, dart, elixir-ls, ocamllsp, haskell-language-server,
terraform-ls and the JSON, YAML, TOML and Markdown servers.

- Within one language, table order is preference: the first server whose binary is on `PATH` serves
  it. User entries (`ServerConfig`) replace a built-in of the same name and otherwise come first.
- A **root** is found by walking up from the file to the closest directory holding one of the
  server's root markers (`Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`,
  `compile_commands.json`, ...). Rust uses the highest `Cargo.toml` that declares a `[workspace]`.
  Servers that need a real project (rust-analyzer, gopls) do not start without a marker; the others
  fall back to the workspace root.
- Files outside the workspace root get their own root.

The registry is typed (`ServerConfig`: command, arguments, languages, root markers, root strategy,
initialization options, environment, `enabled`) so configuration can supply entries later. Until
the settings layer does, hosts build an `LspConfig` themselves.

## Lifecycle

`LspManager` is a cheap-clone handle; the servers belong to an actor inside it.

- The first call that touches a file of a type starts its server (`initialize`, then
  `initialized`) and waits for it, at most `Timing::ready_wait` (6 s). A call that gives up leaves
  the server starting; documents the manager already knows are sent to it as soon as it is ready.
- The manager tracks open documents per server with full-text sync: `didOpen` the first time,
  `didChange` when the text differs, `didSave` after an edit announced with
  `notify_file_changed`, `didClose` on `close_file`. Requests read the file from disk and sync it
  first, so edits made by any tool are seen.
- Server messages are answered by the manager: `workspace/configuration` (null per item),
  capability registration, progress creation, workspace folders. Servers may not apply edits to
  files themselves (`workspace/applyEdit` is declined); Nori applies edits.
- A server that exits or fails to start is marked failed with its reason (including the last
  stderr line) and its diagnostics are dropped. The next call after a backoff (1 s, doubling to
  30 s) starts it again, at most `max_restarts` times (3); after that it stays failed until
  `LspManager::restart`. A server that ran healthily for a minute starts over with a full budget.
- Everything is async and bounded by `Timing` (initialize 30 s, request 20 s, shutdown 2 s). No lock
  is held across an await; document and diagnostic state is touched in short synchronous sections,
  and file reads use `tokio::fs`. Dropping a call abandons the wait and nothing else.
- `shutdown` (or dropping the last `LspManager`) asks every server to `shutdown` and `exit`, then
  kills what is left.

Hosts that run servers elsewhere (a remote machine's shell) or fakes in tests implement
`ServerLauncher` and use `LspManager::with_launcher`.

## Diagnostics after an edit

The contract between an edit tool and the model:

1. After writing a file, the tool calls `notify_file_changed(path, new_text)`. This opens or updates
   the document on the file's server, sends `didSave`, and records what the rest of the project's
   errors looked like at that moment.
2. It then calls `diagnostics_after_edit(path)`, which waits for the first `publishDiagnostics` for
   that file after the change (`Timing::first_diagnostics`, 1 s) and then until the server has been
   quiet for `Timing::settle` (300 ms; at most 3 s in all). Publishes that carry a document version
   older than the one sent are ignored.
3. The report holds the file's diagnostics (errors first) and, separately, errors that appeared in
   *other* files since step 1. Its status is `Fresh` (the server spoke), `Stale` (no publish within
   the window; the last known diagnostics are shown), `NoServer` or `Failed(reason)`.
4. `render_summary(report, RenderOptions)` turns it into model-facing text, or an empty string when
   no server serves the file:

```
<diagnostic_summary>
src/a.rs: 1 error, 0 warnings
  error 12:5 [rustc E0308] mismatched types expected `i32`, found `&str`
New errors in other files:
src/b.rs: 1 error
  error 3:1 [rustc E0425] cannot find value `x` in this scope
</diagnostic_summary>
```

Lines and columns are 1-based (columns count characters, whatever encoding the server uses). At
most 20 errors and 8 warnings of the file, 5 other files with 3 errors each are listed; the text
says how many were left out. `LspManager::subscribe_diagnostics` gives the UI a `watch` counter
that grows on every publish, and `diagnostics(path)` reads the store without waiting.

## Requests

All take a **target**: a path, a 1-based line, and a `symbol` and/or `column`. When `symbol` is
given it is searched on the line (whole words first, closest to `column` if there are several),
then on the nearest lines (within 10), so a wrong column or off-by-one line still lands on the
name. Answers carry the resolved position.

| API | Returns |
| --- | --- |
| `definition`, `type_definition`, `implementation` | locations with path, range and the source line |
| `references(include_declaration)` | locations |
| `hover` | the server's markdown |
| `document_symbols(path)` | nested symbols |
| `workspace_symbols(query, near)` | symbols across the project; `near` starts the right server |
| `rename(new_name)` | an edit preview: per file the edits and each changed line before and after. Nothing is applied. |
| `call_hierarchy(incoming/outgoing)` | callers or callees with call sites |
| `code_actions` | the server's quick fixes and refactorings, with edit previews |

`position::apply_edits` applies a preview's edits to text, for the tool that applies a rename.

## Tools

`nori_lsp::tools::specs()` returns the tool definitions as data (name, description, JSON schema)
and `tools::call(&manager, name, arguments)` runs one, returning `LspToolOutput { text, is_error }`.
A host registers the specs in its tool catalog and routes calls to `call`.

| Tool | Use |
| --- | --- |
| `lsp_diagnostics` | errors and warnings of a file, or an overview of the project and the servers' state |
| `lsp_definition` | definition, type definition or implementation of a symbol |
| `lsp_references` | every use of a symbol, grouped by file |
| `lsp_hover` | type and documentation of a symbol |
| `lsp_symbols` | a file's outline, or a project-wide search by name |
| `lsp_rename` | a semantic rename preview |
| `lsp_call_hierarchy` | callers or callees of a function |

Tool paths are relative to the workspace root or absolute; results print workspace-relative paths.
Errors (no server, server starting or down, unsupported feature, timeout) come back as text with
`is_error` set and say what to do next.
