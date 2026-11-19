# Vendored Codex workspace

Copy of `openai/codex` at tag `rust-v0.169.0`
(commit `d32063f96c2dff1dc96af839b3952abb6acfeda4`), limited to the crates and
path dependencies the Nori workspace resolves.

Path dependencies are used instead of a git dependency because resolvers must
scan a git repository for manifests, and the Codex workspace lives in the
repository's nested `codex-rs/` directory. Some Cargo-compatible tools (for
example Mitts) only scan the repository root, so a git dependency on Codex makes
every build fail to find any Codex package.

Changes from upstream:

- The `members` list in `Cargo.toml` is pruned to the vendored directories.
- The Code Mode host, runtime and plugin host are linked into `nori`; the host
  runs as a hidden entry point of the same executable. Code Mode is enabled by
  default, subject to explicit configuration.
- `codex-core` sets `#![recursion_limit = "256"]`; Nori's pinned Rust 1.97.1
  overflows the default query depth while laying out the plugin-install tool's
  async handler.
- `mxc-sdk` uses the crates.io release (`=1.0.0`) instead of the git revision,
  which lives in a nested workspace that some resolvers cannot scan.
- `tree-sitter` is pinned to `0.27.2` and `tree-sitter-bash` to `0.25.1` to match
  the version Nori links; only one copy of the native library may exist in a
  build.
- `codex-apply-patch` and `codex-shell-command` point at the sibling
  `vendor/codex-apply-patch` and `vendor/codex-shell-command` directories, which
  carry the tree-sitter 0.27 API adaptations.

Update by copying the same directories from a newer Codex tag and re-applying
those changes.

The sibling `vendor/rusty-v8` contains the Rust frontend from the same pinned
OpenAI V8 fork (revision `90a65f887a0c0f048f3784afbcb320b373c8a2e0`). Normal
builds use its matching prebuilt artifacts from Codex releases; the archive
omits Chromium's large submodules. For `V8_FROM_SOURCE=1`, clone that revision
with its submodules and override the `v8` patch to point to the full checkout
using Cargo's `--config` option.
