{
  lib,
  rustPlatform,
  stdenv,
  cmake,
  pkg-config,
  dbus,
  zstd,
  sqlite,
  fetchurl,
  fetchFromGitHub,
  makeWrapper,
  opencode ? null,
  nodejs ? null,
  geminiCoreEntry ? null,
  nativeGemini ? false,
  # Run Codex inside Nori. It links all of codex-core, so turning it off gives a much smaller and
  # faster build, at the cost of needing an external Codex adapter.
  nativeCodex ? true,
  ...
}:
let
  sourceRoot = ../.;
  # A local Cargo target can contain tens of gigabytes of native build output.
  # Keep the filter path-specific: fixture `target` directories and vendored
  # sources must remain part of the Nix source tree.
  sourceFilter = path: type:
    let
      relativePath = lib.removePrefix "${toString sourceRoot}/" (toString path);
      excludedRoots = [
        "nori-rs/target"
        "vendor/pkl-native/target"
        ".nori-captures"
        ".direnv"
        "node_modules"
      ];
      isExcluded = lib.any (
        root: relativePath == root || lib.hasPrefix "${root}/" relativePath
      ) excludedRoots;
    in
    !isExcluded;

  source = lib.cleanSourceWith {
    src = sourceRoot;
    filter = sourceFilter;
  };

  # Codex publishes a prebuilt V8 archive/binding pair for Darwin arm64. This
  # avoids compiling V8 in the Nix sandbox (the other targets retain Cargo's
  # normal source-build path until their release pair is pinned here).
  v8Artifacts = lib.optionalAttrs (stdenv.hostPlatform.isDarwin && stdenv.hostPlatform.isAarch64) {
    archive = fetchurl {
      url = "https://github.com/openai/codex/releases/download/rusty-v8-v152.2.0/librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz";
      hash = "sha256-/qua4DhL66qZwdAAZURiydQhI9WHGFx4dqru99dv90k=";
    };
    binding = fetchurl {
      url = "https://github.com/openai/codex/releases/download/rusty-v8-v152.2.0/src_binding_ptrcomp_sandbox_release_aarch64-apple-darwin.rs";
      hash = "sha256-udVlLrylFesr9MLuSmVRslZxF7Wqcg9DDSAyYW3X47U=";
    };
  };

in
assert !nativeGemini || (nodejs != null && geminiCoreEntry != null);
rustPlatform.buildRustPackage {
  pname = "nori";
  version = "0.0.0";
  src = source;
  cargoRoot = "nori-rs";

  cargoLock = {
    lockFile = ./Cargo.lock;
    # The git dependencies are the crossterm and tungstenite forks pinned in Cargo.toml, and the
    # upstream Codex crates native Codex is built from.
    allowBuiltinFetchGit = true;
  };
  cargoBuildFlags = [
    "--bin"
    "nori"
  ] ++ lib.optionals nativeGemini [ "--features" "native-gemini" ];
  buildNoDefaultFeatures = !nativeCodex;

  # aws-lc-sys (through rustls) builds AWS-LC from source. The cmake builder matches nixpkgs' own
  # override for this crate.
  env = {
    AWS_LC_SYS_CMAKE_BUILDER = "1";
  } // lib.optionalAttrs (nativeCodex && v8Artifacts != { }) {
    RUSTY_V8_ARCHIVE = v8Artifacts.archive;
    RUSTY_V8_SRC_BINDING_PATH = v8Artifacts.binding;
  };
  # The build shell sets AWS_LC_SYS_USE_SYSTEM=1, which makes aws-lc-sys demand a system AWS-LC
  # that nixpkgs does not provide. The derivation env value of 0 does not reach cargo, so the
  # variable is exported again in the build shell.
  preBuild = ''
    export AWS_LC_SYS_USE_SYSTEM=0
  '';

  # mxc-sdk's build script checks the `windows` version in the first Cargo.lock above it, which in
  # the vendor directory is Nori's (windows 0.58 from Codex's Windows sandbox). Its bindings are only
  # used on Windows, and without the generation record the check is skipped.
  postPatch = ''
    chmod -R u+w ../cargo-vendor-dir/mxc-sdk-1.0.0/build/isolation_session_bindings
    rm -f ../cargo-vendor-dir/mxc-sdk-1.0.0/build/isolation_session_bindings/GENERATION_INFO.toml
  '';

  nativeBuildInputs = [
    pkg-config
    cmake
    makeWrapper
  ];
  # zstd-sys probes pkg-config for libzstd before it falls back to its bundled copy, so the
  # library is provided here.
  # sqlite is linked by name (-lsqlite3); the Nix SDK on macOS does not provide it.
  buildInputs = [
    zstd
    sqlite
  ] ++ lib.optionals stdenv.hostPlatform.isLinux [ dbus ];
  doCheck = false;

  postInstall = lib.optionalString (opencode != null || nativeGemini) ''
    mv $out/bin/nori $out/bin/.nori-real
    makeWrapper $out/bin/.nori-real $out/bin/nori \
      --prefix PATH : ${lib.makeBinPath (lib.optional (opencode != null) opencode ++ lib.optional nativeGemini nodejs)} \
      ${lib.optionalString nativeGemini "--set-default NORI_GEMINI_CORE_MODULE ${geminiCoreEntry}"}
  '';

  meta = {
    description = "Nori command-line interface, an agent-agnostic terminal UI over ACP";
    homepage = "https://github.com/tilework-tech/nori-cli";
    license = lib.licenses.asl20;
    mainProgram = "nori";
  };
}
