# The ACP adapters Nori talks to, as small wrapper scripts around ordinary Nix packages. Nothing
# is fetched at run time, so a launch never finds a half-installed adapter and fails before Nori
# has a model list.
{ pkgs }:
let
  inherit (pkgs) lib;
in
{
  # Claude Code over ACP. `executable` is the `claude` launcher to run (a name on PATH, or an
  # absolute path to an account's own launcher).
  claude =
    {
      name,
      executable ? "claude",
    }:
    pkgs.writeShellScriptBin name ''
      export CLAUDE_CODE_EXECUTABLE=${lib.escapeShellArg executable}
      exec ${lib.getExe pkgs.claude-agent-acp} "$@"
    '';

  # Codex over ACP. codex-acp reads its login from CODEX_HOME itself, before it starts Codex, so
  # the account's home is set here; without it the adapter reports "Authentication required" and
  # Nori has no model list. `executable` is the account's own launcher.
  codex =
    {
      name,
      home,
      executable ? "codex",
      config ? { },
    }:
    pkgs.writeShellScriptBin name ''
      export CODEX_HOME=${lib.escapeShellArg home}
      export CODEX_PATH=${lib.escapeShellArg executable}
      export CODEX_CONFIG=${lib.escapeShellArg (builtins.toJSON config)}
      unset CODEX_PROFILE_HOME
      exec ${lib.getExe pkgs.codex-acp} "$@"
    '';

  # Cursor wants its global flags before the `acp` subcommand. Nori's model_override appends
  # `--model <id>` to this script's arguments.
  cursor =
    {
      name,
      executable ? "cursor-agent",
    }:
    pkgs.writeShellScriptBin name ''
      set -euo pipefail
      exec ${lib.escapeShellArg executable} "$@" acp
    '';

  # Claude Code through the Rust driver built into Nori, served as its own ACP process by
  # `nori agent serve claude`: no Node, and it starts in about 20 ms. `package` is the Nori package.
  # As with `claude`, `executable` is how an account is chosen, through its own launcher.
  claudeNative =
    {
      name,
      package,
      executable ? "claude",
    }:
    pkgs.writeShellScriptBin name ''
      export CLAUDE_CODE_EXECUTABLE=${lib.escapeShellArg executable}
      exec ${lib.getExe package} agent serve claude "$@"
    '';

  # OpenCode speaks ACP itself through `opencode acp`; the wrapper only pins which binary runs.
  opencode =
    {
      name,
      executable ? "opencode",
    }:
    pkgs.writeShellScriptBin name ''
      exec ${lib.escapeShellArg executable} acp "$@"
    '';
}
