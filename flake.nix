{
  description = "Nori: a terminal UI for ACP coding agents, with its adapters and config declared in Nix";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      overlays.default = import ./nix/overlay.nix self;

      # `programs.nori`: the package, one ACP adapter per agent, and config.toml.
      homeModules.default = import ./nix/home-module.nix self;
      homeModules.nori = self.homeModules.default;

      packages = forAllSystems (
        _system: pkgs:
        let
          nori = pkgs.callPackage ./nori-rs {
            opencode = pkgs.opencode;
          };
        in
        {
          inherit nori;
          nori-rs = nori;
          default = nori;
        }
      );

      apps = forAllSystems (
        system: _pkgs: {
          default = {
            type = "app";
            program = nixpkgs.lib.getExe self.packages.${system}.nori;
          };
        }
      );

      # Builds the adapter wrappers and evaluates the module with a minimal stand-in for Home
      # Manager, so a broken option or wrapper fails `nix flake check` instead of a rebuild.
      checks = forAllSystems (
        system: _pkgs:
        let
          # claude-agent-acp runs the unfree Claude Code, so the checks allow exactly that.
          pkgs = import nixpkgs {
            inherit system;
            config.allowUnfreePredicate = pkg: nixpkgs.lib.getName pkg == "claude-code";
          };
          adapters = import ./nix/adapters.nix { inherit pkgs; };
          lib = pkgs.lib;
          evaluated = lib.evalModules {
            specialArgs = { inherit pkgs; };
            modules = [
              (self.homeModules.default)
              (
                { lib, ... }:
                {
                  options = {
                    assertions = lib.mkOption { type = lib.types.listOf lib.types.unspecified; };
                    xdg.dataHome = lib.mkOption { type = lib.types.str; };
                    home = {
                      homeDirectory = lib.mkOption { type = lib.types.str; };
                      packages = lib.mkOption { type = lib.types.listOf lib.types.package; };
                      sessionVariables = lib.mkOption { type = lib.types.attrsOf lib.types.str; };
                      file = lib.mkOption { type = lib.types.attrsOf lib.types.raw; };
                      activation = lib.mkOption { type = lib.types.attrsOf lib.types.raw; };
                    };
                  };
                  config = {
                    _module.args.pkgs = lib.mkForce pkgs;
                    xdg.dataHome = "/home/test/.local/share";
                    home.homeDirectory = "/home/test";
                    # The module reads this from Home Manager's library.
                    _module.args.lib = lib.mkForce (lib.extend (_: _: { hm.dag.entryAfter = _: data: data; }));
                    programs.nori = {
                      enable = true;
                      # The module's wiring is what is checked here; building Nori itself is `.#nori`.
                      package = pkgs.writeShellScriptBin "nori" "";
                      settings.agent = "claude";
                      agents.claude = {
                        name = "Claude";
                        transcriptBaseDir = ".claude/projects";
                        modelOverride.env = "ANTHROPIC_MODEL";
                        adapter.kind = "claude";
                      };
                      agents.codex = {
                        name = "Codex";
                        transcriptBaseDir = ".codex/sessions";
                        adapter = {
                          kind = "codex";
                          codexHome = "/home/test/.codex";
                        };
                      };
                      agents.cursor = {
                        name = "Cursor";
                        transcriptBaseDir = ".cursor/chats";
                        modelOverride.arg = "--model";
                        adapter.kind = "cursor";
                      };
                    };
                  };
                }
              )
            ];
          };
          failed = lib.filter (a: !a.assertion) evaluated.config.assertions;
        in
        {
          adapters = pkgs.linkFarm "nori-adapters" {
            claude = adapters.claude { name = "nori-claude-acp"; };
            codex = adapters.codex {
              name = "nori-codex-acp";
              home = "/tmp/codex";
            };
            cursor = adapters.cursor { name = "nori-cursor-acp"; };
            opencode = adapters.opencode { name = "nori-opencode-acp"; };
            claude-native = adapters.claudeNative {
              name = "nori-claude-native-acp";
              package = pkgs.writeShellScriptBin "nori" "";
            };
          };
          module =
            assert failed == [ ];
            pkgs.linkFarm "nori-module" {
              "bin" = pkgs.linkFarm "bin" (
                lib.imap0 (i: p: {
                  name = toString i;
                  path = p;
                }) evaluated.config.home.packages
              );
            };
        }
      );

      devShells = forAllSystems (
        _system: pkgs:
        let
          # mold is Linux-only; macOS uses the system linker.
          linkerPkgs = if pkgs.stdenv.hostPlatform.isLinux then [ pkgs.mold pkgs.clang ] else [ ];
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ (pkgs.callPackage ./nori-rs { }) ];
            # The Rust toolchain is managed by rustup (nori-rs/rust-toolchain.toml).
            buildInputs = linkerPkgs ++ [ pkgs.sccache ];
            shellHook = ''
              export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [ pkgs.openssl ]}:$LD_LIBRARY_PATH
              export RUSTC_WRAPPER=${pkgs.sccache}/bin/sccache
              export SCCACHE_CACHE_SIZE=10G
            '';
          };
        }
      );
    };
}
