# Helix LSP client, vendored for Nori

Source: https://github.com/helix-editor/helix at commit `9de242bfac2aac9dfd0e4496eb25190e85c24e69`
(shallow clone of `master`). `helix-lsp` and `helix-stdx` are Mozilla Public License 2.0 (see
`LICENSE`); `helix-lsp-types` is a fork of `lsp-types` and stays MIT (see
`helix-lsp-types/LICENSE`, Copyright (c) 2016 Markus Westerlind). Only the pieces of an async LSP
client are kept; the editor (`helix-core`, `helix-view`, `helix-term`, `helix-loader`,
`helix-event`, tree-sitter, ropes) is not vendored.

The crates are path dependencies of `nori-rs/lsp` (`nori-lsp`). They are not workspace members and
carry no workspace settings of their own.

## What is kept

| Path | Upstream | State |
| --- | --- | --- |
| `helix-lsp-types/` | `helix-lsp-types` | As vendored. Only `Cargo.toml` changed (workspace dependencies replaced by versions, `publish = false`) and a notice comment added to `src/lib.rs`. |
| `helix-stdx/src/uri.rs` | `helix-stdx/src/uri.rs` | As vendored: the RFC3986 `Url` newtype `helix-lsp-types` is written against. |
| `helix-lsp/src/jsonrpc.rs` | `helix-lsp/src/jsonrpc.rs` | As vendored. |
| `helix-lsp/src/transport.rs` | `helix-lsp/src/transport.rs` | Modified for Nori. |
| `helix-lsp/src/client.rs` | `helix-lsp/src/client.rs` | Modified for Nori. |
| `helix-lsp/src/lib.rs` | `helix-lsp/src/lib.rs` | Modified for Nori: reduced to what the client needs. |

## What Nori changed

Modified files say `Modified for Nori` in their header.

- `helix-stdx/src/lib.rs` is new: it exports only `Url`. The rest of `helix-stdx` (rope helpers,
  paths, `faccess`, regex-cursor, `which`) is dropped.
- `transport.rs`: generic `AsyncBufRead`/`AsyncWrite` streams instead of child-process pipes (so
  tests run an in-process server and a host can run one over any channel); `serde_json` instead of
  `sonic-rs`; message traffic logged at `trace`; no lock held across an await; the server's last 40
  stderr lines are kept for crash reports; `Transport::start` returns a `TransportIo` struct.
- `client.rs`: no `helix-core` (no `Rope`, `ChangeSet`, workspace discovery, language
  configuration) or `helix-loader`; `LanguageServerFeature` is a local enum; full-text document
  sync only (`text_document_did_open/did_change/did_save` take `&str`); `try_add_doc`, multi-root
  bookkeeping and the file-operation requests (`will_create`, `did_rename`, ...) removed;
  `Client::start` takes an already resolved binary; new `Client::start_with_io`, `Client::handshake`
  and `Client::stderr_tail`; client capabilities no longer claim dynamic file watching, pull
  diagnostics or `window/showDocument`, and hierarchical document symbols are on; the client
  identifies itself as `nori`.
- `lib.rs`: only the error type, `OffsetEncoding`, `LanguageServerFeature`, `LanguageServerId` (a
  plain `i64` newtype instead of a `slotmap` key) and the server-to-client `MethodCall` and
  `Notification` parsers remain. Helix's `Registry`, progress map, rope-based edit conversion and
  `find_lsp_workspace` are dropped; Nori's registry and root detection live in `nori-lsp`.
- `helix-lsp` is compiled as edition 2021, as upstream is: its request helpers return
  `impl Future` that must not capture `&self`.

Dropped entirely: `file_event.rs`, `file_operations.rs`, `helix-lsp`'s `globset`, `slotmap`,
`arc-swap`, `sonic-rs`, `futures-*` and `tokio-stream` dependencies.

## Updating

Diff the three upstream files against the commit above, re-apply the changes listed here, and
keep the `Modified for Nori` headers.
