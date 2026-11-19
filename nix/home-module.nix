# Home Manager module: Nori, its ACP adapters and its config.toml, declared in one place.
#
#   programs.nori = {
#     enable = true;
#     settings.agent = "claude";
#     agents.claude = {
#       name = "Claude";
#       authHint = "Run: claude, then /login";
#       transcriptBaseDir = ".claude/projects";
#       modelOverride.env = "ANTHROPIC_MODEL";
#       adapter.kind = "claude";
#     };
#   };
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.nori;
  inherit (lib) mkOption mkEnableOption mkIf types;
  tomlFormat = pkgs.formats.toml { };
  adapters = import ./adapters.nix { inherit pkgs; };

  adapterPackage =
    slug: agent:
    let
      name = "nori-${slug}-acp";
      inherit (agent.adapter) kind executable;
    in
    if kind == "claude" then
      adapters.claude (
        { inherit name; } // lib.optionalAttrs (executable != null) { inherit executable; }
      )
    else if kind == "codex" then
      adapters.codex (
        {
          inherit name;
          home = agent.adapter.codexHome;
          config = agent.adapter.codexConfig;
        }
        // lib.optionalAttrs (executable != null) { inherit executable; }
      )
    else if kind == "cursor" then
      adapters.cursor ({ inherit name; } // lib.optionalAttrs (executable != null) { inherit executable; })
    else if kind == "opencode" then
      adapters.opencode ({ inherit name; } // lib.optionalAttrs (executable != null) { inherit executable; })
    else if kind == "claude-native" then
      adapters.claudeNative (
        {
          inherit name;
          package = if agent.adapter.package != null then agent.adapter.package else cfg.package;
        }
        // lib.optionalAttrs (executable != null) { inherit executable; }
      )
    else
      null;

  adapterPackages = lib.filterAttrs (_: package: package != null) (
    lib.mapAttrs adapterPackage cfg.agents
  );

  agentEntry =
    slug: agent:
    let
      package = adapterPackages.${slug} or null;
      command =
        if agent.command != null then
          agent.command
        else if package != null then
          lib.getExe package
        else
          "";
    in
    {
      inherit (agent) name;
      inherit slug;
      transcript_base_dir = agent.transcriptBaseDir;
      distribution =
        if agent.native != null then
          {
            native = {
              inherit (agent.native) agent;
              config = agent.native.config;
            } // lib.optionalAttrs (agent.native.home != null) { inherit (agent.native) home; };
          }
        else
          {
            local = {
              inherit command;
            } // lib.optionalAttrs (agent.args != [ ]) { inherit (agent) args; };
          };
    }
    // lib.optionalAttrs (agent.authHint != null) { auth_hint = agent.authHint; }
    // lib.optionalAttrs (agent.modelOverride != null) { model_override = agent.modelOverride; };

  configFile = tomlFormat.generate "nori-config.toml" (
    cfg.settings // { agents = lib.mapAttrsToList agentEntry cfg.agents; }
  );

  # A NORI_HOME the caller already set wins, so an isolated run (a scratch home, a second account)
  # is not silently redirected to the configured one.
  wrapped = pkgs.writeShellScriptBin "nori" ''
    if [ -z "''${NORI_HOME:-}" ]; then
      export NORI_HOME=${lib.escapeShellArg cfg.home}
    fi
    exec ${lib.getExe cfg.package} ${lib.optionalString cfg.bypassApprovals "--dangerously-bypass-approvals-and-sandbox "}"$@"
  '';

  agentModule = types.submodule (
    _: {
      options = {
        name = mkOption {
          type = types.str;
          description = "Name shown in the agent picker.";
        };
        authHint = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "What Nori tells the user to run when the agent needs a login.";
        };
        transcriptBaseDir = mkOption {
          type = types.str;
          description = "Where the agent keeps its transcripts, relative to the home directory.";
        };
        modelOverride = mkOption {
          type = types.nullOr (types.attrsOf types.str);
          default = null;
          example = {
            env = "ANTHROPIC_MODEL";
          };
          description = "How Nori hands the agent a model at spawn: `{ env = NAME; }` or `{ arg = FLAG; }`.";
        };
        command = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "The ACP command to run. Left unset, it is the wrapper built from `adapter`.";
        };
        args = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = "Arguments for `command`.";
        };
        native = mkOption {
          type = types.nullOr (
            types.submodule {
              options = {
                agent = mkOption {
                  type = types.enum [
                    "claude"
                    "codex"
                  ];
                  description = "The agent built into Nori to run in its own process, with no adapter.";
                };
                home = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                  description = "Where this account's state and login live (Codex's CODEX_HOME).";
                };
                config = mkOption {
                  type = types.listOf types.str;
                  default = [ ];
                  description = "key=value overrides, as `codex -c` takes them.";
                };
              };
            }
          );
          default = null;
          description = ''
            Run an agent built into Nori inside its own process instead of through an adapter. Needs
            a Nori built with native Codex (the default).
          '';
        };
        adapter = {
          kind = mkOption {
            type = types.nullOr (
              types.enum [
                "claude"
                "claude-native"
                "codex"
                "cursor"
                "opencode"
              ]
            );
            default = null;
            description = "Build the ACP adapter wrapper for this agent from Nix packages.";
          };
          executable = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "The agent's own launcher the adapter runs: a name on PATH or an absolute path.";
          };
          package = mkOption {
            type = types.nullOr types.package;
            default = null;
            description = "The Nori package whose `nori agent serve claude` the `claude-native` adapter runs. Defaults to `programs.nori.package`.";
          };
          codexHome = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "CODEX_HOME of the account, for the `codex` adapter.";
          };
          codexConfig = mkOption {
            type = types.attrs;
            default = { };
            description = "Codex config overrides the `codex` adapter passes as CODEX_CONFIG.";
          };
        };
      };
    }
  );
in
{
  options.programs.nori = {
    enable = mkEnableOption "Nori, a terminal UI for ACP coding agents";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.nori;
      defaultText = lib.literalExpression "nori.packages.\${system}.nori";
      description = "The Nori package.";
    };

    home = mkOption {
      type = types.str;
      default = "${config.xdg.dataHome}/nori/cli";
      defaultText = lib.literalExpression ''"''${config.xdg.dataHome}/nori/cli"'';
      description = "NORI_HOME: where Nori keeps its config, history and transcripts.";
    };

    bypassApprovals = mkOption {
      type = types.bool;
      default = false;
      description = "Start Nori with --dangerously-bypass-approvals-and-sandbox.";
    };

    settings = mkOption {
      type = tomlFormat.type;
      default = { };
      example = lib.literalExpression ''
        {
          agent = "claude";
          default_models.claude = "claude-sonnet-5-5";
          tui.resize_reflow = true;
        }
      '';
      description = "The top level of config.toml, apart from `agents`.";
    };

    agents = mkOption {
      type = types.attrsOf agentModule;
      default = { };
      description = "ACP agents Nori offers, by slug.";
    };

    configMethod = mkOption {
      type = types.enum [
        "copy"
        "symlink"
      ];
      default = "copy";
      description = ''
        `copy` writes config.toml as a plain file on each activation, so Nori can still save
        settings to it between activations; `symlink` makes it read-only.
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions = lib.mapAttrsToList (slug: agent: {
      assertion = agent.command != null || agent.adapter.kind != null || agent.native != null;
      message = "programs.nori.agents.${slug} needs a `command`, an `adapter.kind` or `native`.";
    }) cfg.agents
    ++ lib.mapAttrsToList (slug: agent: {
      assertion = agent.adapter.kind != "codex" || agent.adapter.codexHome != null;
      message = "programs.nori.agents.${slug}.adapter.codexHome is required for the codex adapter.";
    }) cfg.agents;

    home.packages = [ wrapped ] ++ lib.attrValues adapterPackages;

    home.sessionVariables.NORI_HOME = cfg.home;

    home.file = mkIf (cfg.configMethod == "symlink") {
      "${lib.removePrefix "${config.home.homeDirectory}/" cfg.home}/config.toml".source = configFile;
    };

    home.activation = mkIf (cfg.configMethod == "copy") {
      noriConfig = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        run mkdir -p ${lib.escapeShellArg cfg.home}
        run install -m 0644 ${configFile} ${lib.escapeShellArg "${cfg.home}/config.toml"}
      '';
    };
  };
}
