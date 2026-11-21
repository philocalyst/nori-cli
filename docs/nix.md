# Running Nori declaratively with Nix

The flake builds Nori from this checkout, builds the ACP adapters it talks to from Nix packages, and
writes `config.toml` from a Home Manager module. Nothing is fetched at run time.
The source includes the repository's vendored runtimes and excludes local Cargo
output, captures, development environments, and root Node dependencies.

```nix
{
  inputs.nori.url = "github:tilework-tech/nori-cli";

  # in your Home Manager configuration
  imports = [ nori.homeModules.default ];

  programs.nori = {
    enable = true;
    bypassApprovals = false;
    settings = {
      agent = "claude";
      default_models.claude = "claude-sonnet-5-5";
    };
    agents = {
      claude = {
        name = "Claude";
        authHint = "Run: claude, then /login";
        transcriptBaseDir = ".claude/projects";
        modelOverride.env = "ANTHROPIC_MODEL";
        adapter.kind = "claude";            # builds nori-claude-acp
      };
      codex = {
        name = "Codex";
        authHint = "Run: codex, then /login";
        transcriptBaseDir = ".codex/sessions";
        adapter = {
          kind = "codex";                   # builds nori-codex-acp
          codexHome = "/home/me/.codex";
        };
      };
      cursor = {
        name = "Cursor";
        transcriptBaseDir = ".cursor/chats";
        modelOverride.arg = "--model";
        adapter.kind = "cursor";            # builds nori-cursor-acp
      };
    };
  };
}
```

- `adapter.executable` points an adapter at an account's own launcher (a name on `PATH` or an
  absolute path); several accounts are several entries under `agents`.
- `adapter.kind = "opencode"` runs `opencode acp`.
- `adapter.kind = "claude-native"` runs `nori agent serve claude` from the Nori package
  (`adapter.package`, default `programs.nori.package`) as a separate ACP process; `executable` picks
  the account's `claude` launcher. `nori agent serve <slug> [KEY=VALUE...] [--home DIR] [--env K=V]`
  serves any agent built into Nori over stdio ACP for any client.
- Nori runs OpenCode in its own process when built with the default
  `native-opencode` feature: Nori's engine calls OpenCode Zen directly using
  a key stored by Nori and a model catalog from models.dev. It does not read
  OpenCode's configuration or sign-ins, run `opencode`, or connect to an
  OpenCode server. A local distribution with
  `command = "opencode"; args = [ "acp" ];` uses the upstream ACP adapter
  instead. See [native-agents.md](native-agents.md).
- The experimental Gemini Core adapter can be included with `nativeGemini = true`
  when calling `nori-rs/default.nix`, supplying `nodejs` and `geminiCoreEntry`
  for the official Core 0.68.0 entry point. The wrapper sets its Core path;
  select it explicitly with a native `gemini` distribution and API-key or
  Vertex authentication. The built-in Gemini distribution remains the
  official CLI for Google-account sign-in. See `nori-rs/config.md`.
- `native = { agent = "codex"; home = "..."; }` runs Codex inside Nori, on Nori's engine with
  Codex's sign-in, config and catalog: no Codex program, no adapter subprocess and no Node.
  `native = { agent = "claude"; config = [ "cli_path=/path/to/claude2" ]; }`
  does the same for Claude Code through the Rust driver, which starts only Claude Code's own CLI;
  `cli_path` is how an account is chosen. `home` is that account's `CODEX_HOME`, set per agent so several accounts
  can run in one process. It needs a Nori built with native Codex, which is the default
  (`nori.override { nativeCodex = false; }` builds without it; that build is much smaller, but then
  needs a Codex adapter).
- An agent can use its own `command` instead of an adapter.
- `configMethod = "copy"` (the default) writes `config.toml` as a plain file on each activation so
  Nori can still save settings between them; `"symlink"` makes it read-only.
- The wrapper `nori` exports `NORI_HOME` (default `$XDG_DATA_HOME/nori/cli`) only when the caller has
  none: a `NORI_HOME` already in the environment wins, so a scratch home or a second account is never
  silently redirected to the configured one. Set `programs.nori.home` to change the default.
- `antigravity` runs the signed-in official CLI as `agy acp --process-per-session` and takes its
  capabilities from the CLI's own `initialize` response; no adapter and no in-process code are
  involved. Nori picks the newest `agy` it finds on `PATH` or at
  `$XDG_DATA_HOME/nori/antigravity-cli/<version>/agy` (default `~/.local/share`), verifies it with
  `agy --version`, and refuses anything older than 1.5.10 (no `acp` subcommand) with the installed
  version and `agy update`. Install a newer release side by side in that directory; a Nix-packaged
  `agy` older than 1.5.10 on `PATH` is skipped when a newer one exists. Sign in with
  `agy acp --login`. For an API key or Vertex account, build Nori with the `native-antigravity-sdk`
  feature and add `native = { agent = "antigravity-sdk"; }` as its own agent entry; that adapter
  drives Antigravity's local harness over its SDK and needs `GEMINI_API_KEY` or Vertex.

Without Home Manager: `nix run github:tilework-tech/nori-cli` runs the package, `overlays.default`
adds `pkgs.nori`, and `nix develop` gives a build shell. `nix flake check` builds the adapters and
evaluates the module. The `nori` package includes OpenCode on its runtime
`PATH` for upstream ACP configurations. Pkl policy and conversation programs run
in Nori's native Rust runtime and do not require a Pkl or Java executable. The native OpenCode adapter
does not require an OpenCode executable or service.

The Claude adapter runs Claude Code, which is unfree: allow `claude-code` in your nixpkgs config.
