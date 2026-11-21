//! The language-server registry: which files belong to which language, which servers handle a
//! language, and how a server's project root is found. The built-in table is overridable by
//! [`ServerConfig`] entries the user supplies; Nori never installs a server, it only starts one
//! whose binary is on `PATH`.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// One language and the files that belong to it.
struct Language {
    id: &'static str,
    extensions: &'static [&'static str],
    file_names: &'static [&'static str],
}

const LANGUAGES: &[Language] = &[
    Language {
        id: "rust",
        extensions: &["rs"],
        file_names: &[],
    },
    Language {
        id: "typescript",
        extensions: &["ts", "mts", "cts"],
        file_names: &[],
    },
    Language {
        id: "typescriptreact",
        extensions: &["tsx"],
        file_names: &[],
    },
    Language {
        id: "javascript",
        extensions: &["js", "mjs", "cjs"],
        file_names: &[],
    },
    Language {
        id: "javascriptreact",
        extensions: &["jsx"],
        file_names: &[],
    },
    Language {
        id: "python",
        extensions: &["py", "pyi", "pyw"],
        file_names: &[],
    },
    Language {
        id: "go",
        extensions: &["go"],
        file_names: &[],
    },
    Language {
        id: "c",
        extensions: &["c", "h"],
        file_names: &[],
    },
    Language {
        id: "cpp",
        extensions: &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "ipp"],
        file_names: &[],
    },
    Language {
        id: "objective-c",
        extensions: &["m"],
        file_names: &[],
    },
    Language {
        id: "objective-cpp",
        extensions: &["mm"],
        file_names: &[],
    },
    Language {
        id: "zig",
        extensions: &["zig", "zon"],
        file_names: &[],
    },
    Language {
        id: "lua",
        extensions: &["lua"],
        file_names: &[],
    },
    Language {
        id: "shellscript",
        extensions: &["sh", "bash", "zsh"],
        file_names: &[
            ".bashrc",
            ".bash_profile",
            ".zshrc",
            ".zprofile",
            ".profile",
        ],
    },
    Language {
        id: "nix",
        extensions: &["nix"],
        file_names: &[],
    },
    Language {
        id: "ruby",
        extensions: &["rb", "rake", "gemspec"],
        file_names: &["Gemfile", "Rakefile"],
    },
    Language {
        id: "java",
        extensions: &["java"],
        file_names: &[],
    },
    Language {
        id: "kotlin",
        extensions: &["kt", "kts"],
        file_names: &[],
    },
    Language {
        id: "swift",
        extensions: &["swift"],
        file_names: &[],
    },
    Language {
        id: "dart",
        extensions: &["dart"],
        file_names: &[],
    },
    Language {
        id: "elixir",
        extensions: &["ex", "exs"],
        file_names: &[],
    },
    Language {
        id: "ocaml",
        extensions: &["ml", "mli"],
        file_names: &[],
    },
    Language {
        id: "haskell",
        extensions: &["hs", "lhs"],
        file_names: &[],
    },
    Language {
        id: "terraform",
        extensions: &["tf", "tfvars"],
        file_names: &[],
    },
    Language {
        id: "json",
        extensions: &["json", "jsonc"],
        file_names: &[],
    },
    Language {
        id: "yaml",
        extensions: &["yml", "yaml"],
        file_names: &[],
    },
    Language {
        id: "toml",
        extensions: &["toml"],
        file_names: &[],
    },
    Language {
        id: "markdown",
        extensions: &["md", "markdown"],
        file_names: &[],
    },
];

/// The language id of `path` (the protocol's `languageId`), if Nori knows the file type.
pub fn language_for_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    let extension = path.extension().and_then(|extension| extension.to_str());
    LANGUAGES
        .iter()
        .find(|language| {
            language.file_names.contains(&name)
                || extension.is_some_and(|extension| {
                    language
                        .extensions
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(extension))
                })
        })
        .map(|language| language.id)
}

/// Which directory above a file is the server's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootStrategy {
    /// The closest directory that holds a marker.
    #[default]
    Nearest,
    /// The highest directory that holds a marker.
    Topmost,
    /// The highest `Cargo.toml` that declares a `[workspace]`, else the closest `Cargo.toml`.
    CargoWorkspace,
}

/// How to start one language server and which files it serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Protocol language ids this server handles.
    pub languages: Vec<String>,
    /// File or directory names that mark a project root.
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default)]
    pub root_strategy: RootStrategy,
    /// Do not start without a marker (the server needs a real project). Otherwise the workspace
    /// root is the fallback root.
    #[serde(default)]
    pub require_root_marker: bool,
    #[serde(default)]
    pub initialization_options: Option<Value>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Shown when the binary is missing; Nori does not run it.
    #[serde(default)]
    pub install_hint: Option<String>,
}

fn enabled_by_default() -> bool {
    true
}

impl ServerConfig {
    fn builtin(
        name: &str,
        command: &str,
        args: &[&str],
        languages: &[&str],
        root_markers: &[&str],
    ) -> Self {
        Self {
            name: name.to_owned(),
            command: command.to_owned(),
            args: args.iter().map(ToString::to_string).collect(),
            languages: languages.iter().map(ToString::to_string).collect(),
            root_markers: root_markers.iter().map(ToString::to_string).collect(),
            root_strategy: RootStrategy::Nearest,
            require_root_marker: false,
            initialization_options: None,
            env: Vec::new(),
            enabled: true,
            install_hint: None,
        }
    }

    fn requiring_marker(mut self) -> Self {
        self.require_root_marker = true;
        self
    }

    fn with_strategy(mut self, strategy: RootStrategy) -> Self {
        self.root_strategy = strategy;
        self
    }

    fn hint(mut self, hint: &str) -> Self {
        self.install_hint = Some(hint.to_owned());
        self
    }
}

/// The servers Nori knows. Within one language the order is the preference: the first one whose
/// binary is on `PATH` serves it.
pub fn builtin_servers() -> Vec<ServerConfig> {
    use ServerConfig as S;
    vec![
        S::builtin(
            "rust-analyzer",
            "rust-analyzer",
            &[],
            &["rust"],
            &["Cargo.toml", "rust-project.json"],
        )
        .requiring_marker()
        .with_strategy(RootStrategy::CargoWorkspace)
        .hint("rustup component add rust-analyzer"),
        S::builtin(
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            &[
                "typescript",
                "typescriptreact",
                "javascript",
                "javascriptreact",
            ],
            &["tsconfig.json", "jsconfig.json", "package.json"],
        )
        .hint("npm install -g typescript-language-server typescript"),
        S::builtin(
            "pyright",
            "pyright-langserver",
            &["--stdio"],
            &["python"],
            &[
                "pyrightconfig.json",
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                "Pipfile",
            ],
        )
        .hint("npm install -g pyright"),
        S::builtin(
            "basedpyright",
            "basedpyright-langserver",
            &["--stdio"],
            &["python"],
            &[
                "pyrightconfig.json",
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                "Pipfile",
            ],
        )
        .hint("pip install basedpyright"),
        S::builtin(
            "pylsp",
            "pylsp",
            &[],
            &["python"],
            &[
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                "Pipfile",
            ],
        ),
        S::builtin("gopls", "gopls", &[], &["go"], &["go.work", "go.mod"])
            .requiring_marker()
            .hint("go install golang.org/x/tools/gopls@latest"),
        S::builtin(
            "clangd",
            "clangd",
            &["--log=error"],
            &["c", "cpp", "objective-c", "objective-cpp"],
            &[
                "compile_commands.json",
                "compile_flags.txt",
                ".clangd",
                "CMakeLists.txt",
            ],
        ),
        S::builtin(
            "zls",
            "zls",
            &[],
            &["zig"],
            &["build.zig", "zls.json", "build.zig.zon"],
        ),
        S::builtin(
            "lua-language-server",
            "lua-language-server",
            &[],
            &["lua"],
            &[".luarc.json", ".luarc.jsonc"],
        ),
        S::builtin(
            "bash-language-server",
            "bash-language-server",
            &["start"],
            &["shellscript"],
            &[],
        )
        .hint("npm install -g bash-language-server"),
        S::builtin("nixd", "nixd", &[], &["nix"], &["flake.nix"]),
        S::builtin("nil", "nil", &[], &["nix"], &["flake.nix"]),
        S::builtin("ruby-lsp", "ruby-lsp", &[], &["ruby"], &["Gemfile"]),
        S::builtin(
            "jdtls",
            "jdtls",
            &[],
            &["java"],
            &[
                "pom.xml",
                "build.gradle",
                "build.gradle.kts",
                "settings.gradle",
            ],
        ),
        S::builtin(
            "kotlin-language-server",
            "kotlin-language-server",
            &[],
            &["kotlin"],
            &[
                "settings.gradle",
                "settings.gradle.kts",
                "build.gradle",
                "build.gradle.kts",
                "pom.xml",
            ],
        ),
        S::builtin(
            "sourcekit-lsp",
            "sourcekit-lsp",
            &[],
            &["swift"],
            &["Package.swift", "compile_commands.json"],
        ),
        S::builtin(
            "dart",
            "dart",
            &["language-server", "--protocol=lsp"],
            &["dart"],
            &["pubspec.yaml"],
        ),
        S::builtin("elixir-ls", "elixir-ls", &[], &["elixir"], &["mix.exs"]),
        S::builtin(
            "ocamllsp",
            "ocamllsp",
            &[],
            &["ocaml"],
            &["dune-project", "dune-workspace"],
        ),
        S::builtin(
            "haskell-language-server",
            "haskell-language-server-wrapper",
            &["--lsp"],
            &["haskell"],
            &["hie.yaml", "stack.yaml", "cabal.project"],
        ),
        S::builtin(
            "terraform-ls",
            "terraform-ls",
            &["serve"],
            &["terraform"],
            &[".terraform", "main.tf"],
        ),
        S::builtin(
            "vscode-json-language-server",
            "vscode-json-language-server",
            &["--stdio"],
            &["json"],
            &[],
        ),
        S::builtin(
            "yaml-language-server",
            "yaml-language-server",
            &["--stdio"],
            &["yaml"],
            &[],
        ),
        S::builtin("taplo", "taplo", &["lsp", "stdio"], &["toml"], &[]),
        S::builtin(
            "marksman",
            "marksman",
            &["server"],
            &["markdown"],
            &[".marksman.toml"],
        ),
    ]
}

/// The built-in table plus the user's entries.
#[derive(Debug, Clone)]
pub struct Registry {
    servers: Vec<ServerConfig>,
}

impl Registry {
    pub fn builtin() -> Self {
        Self {
            servers: builtin_servers(),
        }
    }

    /// A user entry with the name of a built-in replaces it in place; the others come first, so
    /// the user's choices win within a language.
    pub fn with_overrides(user: Vec<ServerConfig>) -> Self {
        let mut servers = builtin_servers();
        let mut added = Vec::new();
        for entry in user {
            match servers.iter_mut().find(|known| known.name == entry.name) {
                Some(known) => *known = entry,
                None => added.push(entry),
            }
        }
        added.extend(servers);
        Self { servers: added }
    }

    pub fn servers(&self) -> &[ServerConfig] {
        &self.servers
    }

    /// Enabled servers for `language`, most preferred first.
    pub fn for_language(&self, language: &str) -> Vec<&ServerConfig> {
        self.servers
            .iter()
            .filter(|server| {
                server.enabled && server.languages.iter().any(|known| known == language)
            })
            .collect()
    }

    /// A sentence telling the user what to install, for the "no server" error.
    pub fn install_hint(&self, language: &str) -> String {
        let candidates = self.for_language(language);
        if candidates.is_empty() {
            return format!(" (no language server is known for {language} files)");
        }
        let names: Vec<String> = candidates
            .iter()
            .map(|server| match &server.install_hint {
                Some(hint) => format!("{} (`{}`; e.g. `{hint}`)", server.name, server.command),
                None => format!("{} (`{}`)", server.name, server.command),
            })
            .collect();
        format!("; install one on PATH: {}", names.join(", "))
    }
}

/// Finds the project root of `file` for `server`. Blocking filesystem reads: call from
/// `spawn_blocking`. `workspace_root` bounds the upward search when the file lies inside it.
pub fn find_root(file: &Path, server: &ServerConfig, workspace_root: &Path) -> Option<PathBuf> {
    let start = file.parent()?;
    let inside = file.starts_with(workspace_root);
    let mut chosen: Option<PathBuf> = None;
    for dir in start.ancestors() {
        if inside && !dir.starts_with(workspace_root) {
            break;
        }
        let has_marker = server
            .root_markers
            .iter()
            .any(|marker| dir.join(marker).exists());
        if has_marker {
            match server.root_strategy {
                RootStrategy::Nearest => return Some(dir.to_path_buf()),
                RootStrategy::Topmost => chosen = Some(dir.to_path_buf()),
                RootStrategy::CargoWorkspace => {
                    let declares_workspace = std::fs::read_to_string(dir.join("Cargo.toml"))
                        .is_ok_and(|manifest| manifest.contains("[workspace]"));
                    if declares_workspace || chosen.is_none() {
                        chosen = Some(dir.to_path_buf());
                    }
                }
            }
        }
    }
    chosen.or_else(|| {
        if server.require_root_marker {
            None
        } else if inside {
            Some(workspace_root.to_path_buf())
        } else {
            Some(start.to_path_buf())
        }
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn touch(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn server(name: &str) -> ServerConfig {
        builtin_servers()
            .into_iter()
            .find(|server| server.name == name)
            .unwrap()
    }

    #[test]
    fn language_by_extension_and_name() {
        assert_eq!(language_for_path(Path::new("src/main.rs")), Some("rust"));
        assert_eq!(
            language_for_path(Path::new("a/b.TSX")),
            Some("typescriptreact")
        );
        assert_eq!(
            language_for_path(Path::new("x/.zshrc")),
            Some("shellscript")
        );
        assert_eq!(language_for_path(Path::new("Gemfile")), Some("ruby"));
        assert_eq!(language_for_path(Path::new("notes.txt")), None);
        assert_eq!(language_for_path(Path::new("Makefile")), None);
    }

    #[test]
    fn python_prefers_pyright_then_basedpyright() {
        let registry = Registry::builtin();
        let names: Vec<&str> = registry
            .for_language("python")
            .iter()
            .map(|server| server.name.as_str())
            .collect();
        assert_eq!(names, vec!["pyright", "basedpyright", "pylsp"]);
    }

    #[test]
    fn user_entries_win_and_replace_by_name() {
        let mut mine = server("pylsp");
        mine.command = "my-pylsp".to_owned();
        let mut extra = server("zls");
        extra.name = "my-zls".to_owned();
        let registry = Registry::with_overrides(vec![mine, extra]);
        let python = registry.for_language("python");
        assert_eq!(
            python
                .iter()
                .map(|server| server.command.as_str())
                .collect::<Vec<_>>(),
            vec!["pyright-langserver", "basedpyright-langserver", "my-pylsp"]
        );
        assert_eq!(registry.for_language("zig")[0].name, "my-zls");
    }

    #[test]
    fn disabled_servers_are_skipped() {
        let mut off = server("gopls");
        off.enabled = false;
        let registry = Registry::with_overrides(vec![off]);
        assert!(registry.for_language("go").is_empty());
        assert!(
            registry
                .install_hint("go")
                .contains("no language server is known")
        );
    }

    #[test]
    fn nearest_marker_wins_for_typescript() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("package.json"), "{}");
        touch(&root.join("apps/web/package.json"), "{}");
        touch(&root.join("apps/web/src/a.ts"), "");
        let found = find_root(
            &root.join("apps/web/src/a.ts"),
            &server("typescript-language-server"),
            root,
        );
        assert_eq!(found, Some(root.join("apps/web")));
    }

    #[test]
    fn cargo_workspace_root_beats_member_crate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"lsp\"]\n",
        );
        touch(&root.join("lsp/Cargo.toml"), "[package]\nname = \"x\"\n");
        touch(&root.join("lsp/src/lib.rs"), "");
        let found = find_root(&root.join("lsp/src/lib.rs"), &server("rust-analyzer"), root);
        assert_eq!(found, Some(root.to_path_buf()));

        let lone = tempfile::tempdir().unwrap();
        touch(&lone.path().join("a/Cargo.toml"), "[package]\n");
        touch(&lone.path().join("a/src/lib.rs"), "");
        let found = find_root(
            &lone.path().join("a/src/lib.rs"),
            &server("rust-analyzer"),
            lone.path(),
        );
        assert_eq!(found, Some(lone.path().join("a")));
    }

    #[test]
    fn markerless_roots_fall_back_or_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("src/a.go"), "");
        touch(&root.join("scripts/run.sh"), "");
        assert_eq!(
            find_root(&root.join("src/a.go"), &server("gopls"), root),
            None
        );
        assert_eq!(
            find_root(
                &root.join("scripts/run.sh"),
                &server("bash-language-server"),
                root
            ),
            Some(root.to_path_buf())
        );
        let outside = tempfile::tempdir().unwrap();
        touch(&outside.path().join("x/run.sh"), "");
        assert_eq!(
            find_root(
                &outside.path().join("x/run.sh"),
                &server("bash-language-server"),
                root
            ),
            Some(outside.path().join("x"))
        );
    }
}
