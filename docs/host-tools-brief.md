# Brief: host tools, instant switching, workflows — done and open

Status: updated 2026-11-16 on branch `perf/instant-core`. The design that was planned here is
built; how it works now lives in the docs below. What follows is only what a next session needs.
Nothing from the forge, CI, process-cleanup or fork work is committed yet.

## Done

- **Host tools** ([host-tools.md](host-tools.md)). Built-in Codex and Claude get Nori's tools as
  plain function calls (crate `nori-host-tools`): Codex as dynamic tools plus thread
  instructions, Claude as an in-process SDK server named `nori`. The `nori-client` MCP server
  is now only for external agents. Calls carry `_meta.nori.hostTool`, which the TUI recognizes
  by name. Verified live: native Claude ran `git_status`, `git_diff`, `git_log` and `git_commit`
  in a scratch repo and committed exactly one file.
- **New git tools** `git_diff` (per-file `+/−` cell) and `git_log` (history cell), read with gix.
- **Tool and prompt diet**, after reading t3code and Pi. t3code serves its own tools over MCP,
  so Nori's in-process path goes further than t3's. Descriptions are shorter and state what
  comes back. Each rule is said once, and goal instructions no longer name a server. Claude
  keeps the git, delegate, status and wait tools loaded; the rest are found by tool search.
- **Warm sessions** ([warm-sessions.md](warm-sessions.md)). Claude CLIs are started ahead of
  time, process-wide. Nori primes other built-in agents 5 s after a root session starts.
  Measurements are in `nori-rs/scripts/perf/README-switch.md`: CLI spawn to `initialize` is
  about 1.0 s, Nori `initialize` 10 ms, and `session/new` 16 ms.
- **Workflows** ([workflows.md](workflows.md)). Pkl policy (crate `nori-pkl`)
  loads from `$NORI_HOME/init.pkl` and trusted `.nori/nori.pkl`. Hooks, commit
  guidance, a designated reviewer, and ranked subscription fallback use the
  same captured machine properties. Hook turns have no default policy cap.
  [Programs](programs.md) run declarative multi-actor workflows with journaled steps.
- **Forge** ([forge.md](forge.md)). Crate `nori-forge` is a GitHub REST client with
  conditional reads; nothing shells out to `gh`. Built and unit-tested (14 crate tests, with
  wiremock for the GitHub calls).
  - `ci_status` reports the branch's pull request and checks, with failed jobs' log tails.
  - `pull_request` opens or updates the branch's pull request. It enforces the workflow's
    rules (sections, size, base, draft, `upstream-when`), gives the `on-pr` prompt before
    opening. Nori runs these actions without an approval prompt.
  - New hooks `on-ci-fail` and `on-ci-pass`: Nori watches the checks on what the agent
    pushed.
  - TUI views for both tools, with snapshot tests (`ci_status`, opened pull request, refused
    pull request).
  - Read against github.com anonymously: `main`'s nine check runs read correctly, and a
    conditional re-read took 37 ms.
- **Ctrl+Space subagent switcher**, with fuzzy search, a live preview, and a focused
  follow view. Verified live (empty state).
- **Claude driver fixes**:
  - Tool results now reach the client. Every tool used to stay "in progress": `user` messages
    were dropped.
  - The client's MCP servers (stdio, HTTP, SSE) go to the CLI as they are. HTTP servers used
    to be dropped silently, and stdio ones were never listed.
  - `session/resume` keeps the session id.
  - New conversations get `--session-id`.
  - Forks get `--session-id` too, so a forked conversation keeps the driver's id. The CLI
    accepts the pair (checked by hand with claude 2.1.303: `--resume … --fork-session
    --session-id` reports the given id). No automated test covers it yet.
  - Agent errors show their reason instead of "Internal error" (Nori side).
- **Codex fixes**:
  - Generated images display again.
  - A dynamic tool call is always answered, so a turn never hangs on one.
  - The 21 translation tests run again (25/25).

## Still open

- **Forge and CI: built, not yet proven**
  - The harness tests `a_pull_request_waits_for_the_push_the_hook_and_the_rules` and
    `a_workflow_that_never_upstreams_opens_nothing` (`harness/src/backend/forge_tools_tests.rs`)
    have not been compiled or run.
  - Not run live: `pull_request` opening a real pull request, and the CI watch firing
    `on-ci-fail` and `on-ci-pass`. Both need a pushed branch and a `GH_TOKEN`.
  - `gh` here keeps its token in the keychain, which Nori does not read; without `GH_TOKEN`
    only public repositories answer, at 60 requests an hour.
  - A branch pushed to a fork opens nothing upstream: the repository is the push remote's.
  - Only GitHub is supported.
- **Claude process cleanup: written, not tested**
  - `nori-rs/claude-acp/src/session/session/mod.rs` reads the session's process tree before
    disconnecting, waits up to `CLI_EXIT_GRACE` (2 s) for the CLI to exit, then terminates the
    tree (`terminate`, 300 ms grace).
  - `nori-rs/claude-acp/src/utils/descendants.rs` holds `session_tree`, with the test
    `a_session_tree_is_its_cli_and_what_the_cli_started`. Neither has been compiled or run.
  - The CLI re-initializes MCP about 0.85 s after the first query, and the user's
    SessionStart hooks run on every spawn. Both sit before the first token, even on a warm
    session.
  - `rate_limit_event` is dropped inside the SDK; surfacing it as a toast needs an SDK change
    or a raw stream hook.
  - The account's `cli_path` is not filled in from the active account automatically.
- **Verification still owed**
  - Verified live in Rex against in-process Claude: an `on-change` hook fired after an edit,
    and the agent committed by the workflow's rules; parallel `git_status` and `git_log`
    finished with their data; `/new` took a warm session ("Session ready" at once, against
    about 1 s cold).
  - Not yet run live: the commit agent (a delegated task), and the switch from Codex to a
    warm Claude, since there is no Codex login. No Codex turn has been seen.
  - Toasts have never been seen on screen.
  - The user's Ctrl+Shift and "doesn't render to the bottom" reports were never reproduced.
  - No tmux check of the TUI changes, and no showcase capture of the `ci_status` and
    `pull_request` views (CLAUDE.md asks for both).
  - `policy_change_is_applied_to_the_active_session` (tui-pty-e2e) failed in the full run. It
    has not been rerun, so it is unknown whether it is flaky or caused by this work.
  - `just fmt` and `just fix -p` have not been rerun on nori-forge, nori-config, nori-script,
    nori-harness, nori-tui and nori-claude-acp since the last edits.
- **Nix**
  - `nix build .#nori` failed when the new `nori-forge` crate was untracked: the flake only
    copies git-tracked files. The new files are marked intent-to-add with `git add -N`
    (nothing is committed; `git reset -- <paths>` undoes it). With them visible, the release
    build succeeds (`/nix/store/6bxk7xs5gsi266l29zmxb0akifrmsrxl-nori-0.0.0`).
  - `nix flake check` exits 0 on aarch64-darwin (Linux systems are omitted by default).
  - The release binary runs and was opened in a Rex tab; it shows the prompt. Its `--version`
    prints `nori-ai-cli 0.0.0`, which is the wrong name and needs a look.
  - Linux builds are not tried. The build takes about 53 min with `--max-jobs 1 --cores 4`.
- **Code debts**
  - Codex `guardian_action_summary` has a catch-all arm.
  - Codex `ApprovedMcpPolicyAmendment` has a placeholder label.
  - `nori-skillsets` and `nori-ai` are still shelled out.
  - The dev binary's `__eh_frame` linker warning remains.
  - Unused `insta::assert_snapshot` imports in several tui-pty-e2e files pre-date this work.
- **Environment**
  - Tests that fail on this machine for reasons outside the code:
    - `harness shell::tests::{detects_sh, detects_zsh, fish_fallback_to_zsh}` (Nix zsh path),
    - `tui remote_control::tests::disable_disconnects_the_controller_and_stops_every_listener`,
    - `acp-host remote_ws one_server_listens_on_loopback_...`, which hangs.
  - The default `cargo` here is a `mitts` shim that cannot resolve renamed dependencies such as the
    `crossterm_0_29` alias used by `ratatui-crossterm`. Use `just release` or
    `cargo-stock build --release --bin nori` for release builds. The Codex crates are vendored under
    `vendor/codex`, so dependency resolution no longer depends on scanning a nested workspace inside
    a git repository.
- **Housekeeping**
  - Decide whether to commit or delete `PERF_PLAN.md` (untracked, repo root).
  - Commit the work when asked; commit messages end with
    `Co-Authored-By: Claude Haiku 5.5 <noreply@anthropic.com>`.
