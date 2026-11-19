//! Skillset picker component for switching between available skillsets.
//!
//! This module provides functionality for:
//! - Checking if the nori-skillsets CLI is available
//! - Listing available skillsets
//! - Building a picker UI for skillset selection
//! - Installing selected skillsets

use std::path::Path;
use std::path::PathBuf;

use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionAction;
use crate::choice_picker::ChoiceItem;
use crate::choice_picker::ChoiceSpec;

/// The command name for the nori-skillsets CLI.
const NORI_SKILLSETS_CMD: &str = "nori-skillsets";

/// Resolve the `--install-dir` to pass to `nori-skillsets switch`/`install`
/// for the current session.
///
/// Per-session skillsets isolate their managed files (`.nori-managed` markers
/// and manifests) inside the session's git worktree, so we only pass a custom
/// install dir when `cwd` is a worktree; otherwise we return `None` and let the
/// switch target the user's home install.
///
/// `skillset_per_session` must NOT force a non-worktree `cwd` (e.g. the repo
/// root, when auto-worktree is off or was blocked) to be used as the install
/// dir: that scatters managed state into the project tree and orphans it.
///
/// Worktree detection is path-based (`extract_worktree_name`): it recognizes
/// the `.worktrees/<name>` layout that auto-worktree creates, which is the
/// supported per-session mechanism. A hand-made `git worktree` outside
/// `.worktrees/` is not recognized and falls back to the home install.
pub fn session_skillset_install_dir(cwd: &Path) -> Option<PathBuf> {
    crate::system_info::extract_worktree_name(cwd)
        .is_some()
        .then(|| cwd.to_path_buf())
}

/// Check if nori-skillsets command is available in PATH.
pub fn is_nori_skillsets_available() -> bool {
    which::which(NORI_SKILLSETS_CMD).is_ok()
}

/// List available skillsets by running `nori-skillsets list`.
///
/// Returns:
/// - `Ok(names)` with skillset names on success (exit code 0)
/// - `Err(message)` with stdout/stderr on failure (non-zero exit)
pub async fn list_skillsets() -> Result<Vec<String>, String> {
    let output = tokio::process::Command::new(NORI_SKILLSETS_CMD)
        .args(build_list_args())
        .output()
        .await
        .map_err(|e| format!("Failed to run {NORI_SKILLSETS_CMD}: {e}"))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let names: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Ok(names)
    } else {
        // Combine stdout and stderr for error message
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if !stderr.is_empty() {
            stderr.to_string()
        } else if !stdout.is_empty() {
            stdout.to_string()
        } else {
            format!(
                "{NORI_SKILLSETS_CMD} list failed with exit code {}",
                output.status.code().unwrap_or(-1)
            )
        };
        Err(message)
    }
}

/// Install a skillset by running `nori-skillsets install <name>`.
///
/// Returns:
/// - `Ok(message)` with filtered stdout on success (last section for long output)
/// - `Err(message)` with error output on failure
pub async fn install_skillset(name: &str) -> Result<String, String> {
    let args = build_install_args(name);
    let output = tokio::process::Command::new(NORI_SKILLSETS_CMD)
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("Failed to run {NORI_SKILLSETS_CMD} install: {e}"))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(filter_install_output(&stdout, name))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if !stderr.is_empty() {
            stderr.trim().to_string()
        } else if !stdout.is_empty() {
            stdout.trim().to_string()
        } else {
            format!(
                "Failed to install skillset '{name}' (exit code {})",
                output.status.code().unwrap_or(-1)
            )
        };
        Err(message)
    }
}

/// Switch to a skillset by running `nori-skillsets switch <name> --install-dir <dir>`.
///
/// Returns:
/// - `Ok(message)` with filtered stdout on success
/// - `Err(message)` with error output on failure
pub async fn switch_skillset(name: &str, install_dir: &std::path::Path) -> Result<String, String> {
    let args = build_switch_args(name, install_dir);
    let output = tokio::process::Command::new(NORI_SKILLSETS_CMD)
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("Failed to run {NORI_SKILLSETS_CMD} switch: {e}"))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(filter_install_output(&stdout, name))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if !stderr.is_empty() {
            stderr.trim().to_string()
        } else if !stdout.is_empty() {
            stdout.trim().to_string()
        } else {
            format!(
                "Failed to switch to skillset '{name}' (exit code {})",
                output.status.code().unwrap_or(-1)
            )
        };
        Err(message)
    }
}

/// The skillset choices. `install_dir` is the session worktree to switch in, when there is
/// one; without it a choice installs globally. With `show_no_skillset` a first choice starts
/// without one. `on_dismiss` runs when the picker is closed without choosing, and
/// `auto_worktree_off` warns that files land in the current directory (or globally).
pub fn skillset_choices(
    skillset_names: Vec<String>,
    install_dir: Option<PathBuf>,
    on_dismiss: Option<SelectionAction>,
    show_no_skillset: bool,
    auto_worktree_off: bool,
) -> ChoiceSpec {
    let mut items: Vec<ChoiceItem> = Vec::new();
    if show_no_skillset {
        items.push(
            ChoiceItem::run(
                "No Skillset",
                Box::new(|tx| tx.send(AppEvent::SkillsetPickerDismissed)),
            )
            .detail("Start without a skillset"),
        );
    }
    items.extend(skillset_names.into_iter().map(|name| {
        let chosen = name.clone();
        let install_dir = install_dir.clone();
        ChoiceItem::run(
            name,
            Box::new(move |tx| match install_dir.clone() {
                Some(dir) => tx.send(AppEvent::SwitchSkillset {
                    name: chosen.clone(),
                    install_dir: dir,
                }),
                None => tx.send(AppEvent::InstallSkillset {
                    name: chosen.clone(),
                }),
            }),
        )
    }));
    let subtitle = match (&install_dir, auto_worktree_off) {
        (Some(dir), true) => format!("Warning: skillset files will be added to {}", dir.display()),
        (Some(dir), false) => format!("Switching skillset in {}", dir.display()),
        // Per-session is on but there is no session worktree to isolate into (auto-worktree
        // is off), so the skillset installs globally. Say so rather than silently doing a
        // home install.
        (None, true) => "No session worktree - installing to your global skillset".to_owned(),
        (None, false) => "Install a skillset to customize Nori's capabilities".to_owned(),
    };
    let mut spec = ChoiceSpec::new("Select skillset", items);
    spec.subtitle = Some(subtitle);
    spec.on_dismiss = on_dismiss;
    spec
}

/// Filter install output to extract the meaningful message.
///
/// If stdout has more than 3 lines, splits on double newlines and takes the
/// first non-empty trimmed line from the last section. Otherwise, takes the
/// first non-empty trimmed line. Falls back to a default message.
fn filter_install_output(stdout: &str, name: &str) -> String {
    let line_count = stdout.lines().count();

    let section = if line_count > 3 {
        // For long output, take the section after the last "\n\n"
        if let Some(pos) = stdout.rfind("\n\n") {
            &stdout[pos..]
        } else {
            stdout
        }
    } else {
        stdout
    };

    section
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("Skillset '{name}' installed successfully"))
}

/// Build the argument list for the `nori-skillsets switch` command.
///
/// Extracted for testability.
fn build_switch_args(name: &str, install_dir: &std::path::Path) -> Vec<String> {
    vec![
        "--non-interactive".to_string(),
        "switch".to_string(),
        name.to_string(),
        "--install-dir".to_string(),
        install_dir.to_string_lossy().to_string(),
    ]
}

/// Build the argument list for the `nori-skillsets list` command.
///
/// `list` is spawned via `Command::output()`, which pipes stdout but inherits
/// the terminal's TTY stdin. Passing `--non-interactive` (before the
/// subcommand, for correct parsing) stops nori-skillsets from rendering a
/// blocking update prompt into the captured stdout and hanging the TUI.
///
/// Extracted for testability.
fn build_list_args() -> Vec<String> {
    vec!["--non-interactive".to_string(), "list".to_string()]
}

/// Build the argument list for the `nori-skillsets install` command.
///
/// Extracted for testability.
fn build_install_args(name: &str) -> Vec<String> {
    vec![
        "--non-interactive".to_string(),
        "install".to_string(),
        name.to_string(),
    ]
}

/// Message shown when nori-skillsets is not installed.
pub fn not_installed_message() -> String {
    "nori-skillsets is not installed. Install it with: `npm i -g nori-skillsets`".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn install_dir_is_worktree_path_when_under_dot_worktrees() {
        let cwd = PathBuf::from("/home/user/project/.worktrees/swift-oak");
        assert_eq!(session_skillset_install_dir(&cwd), Some(cwd.clone()));
    }

    #[test]
    fn install_dir_is_none_at_repo_root() {
        // Regression: a per-session skillset switch must not use a non-worktree
        // cwd (e.g. the repo root, when auto-worktree is off or blocked) as the
        // install dir, which would scatter managed state into the project tree.
        let cwd = PathBuf::from("/home/user/project");
        assert_eq!(session_skillset_install_dir(&cwd), None);
    }

    #[test]
    fn test_switch_skillset_command_includes_non_interactive() {
        // The switch command must include --non-interactive since the TUI
        // captures stdout/stderr and provides no stdin. Without it, the
        // CLI prompts for confirmation and hangs forever.
        // --non-interactive must precede the subcommand for correct parsing.
        assert_eq!(
            build_switch_args("my-skillset", std::path::Path::new("/tmp/worktree")),
            vec![
                "--non-interactive",
                "switch",
                "my-skillset",
                "--install-dir",
                "/tmp/worktree",
            ]
        );
    }

    #[test]
    fn test_list_skillset_command_includes_non_interactive() {
        // `list` is spawned via tokio's Command::output(), which pipes stdout
        // but inherits the terminal's TTY stdin. Without --non-interactive,
        // nori-skillsets detects interactive mode and renders a blocking update
        // prompt into the captured stdout, hanging the TUI forever.
        // --non-interactive must precede the subcommand for correct parsing.
        assert_eq!(build_list_args(), vec!["--non-interactive", "list"]);
    }

    #[test]
    fn test_install_skillset_command_includes_non_interactive() {
        // Same as switch: install must not prompt interactively.
        // --non-interactive must precede the subcommand for correct parsing.
        assert_eq!(
            build_install_args("my-skillset"),
            vec!["--non-interactive", "install", "my-skillset"]
        );
    }

    #[test]
    fn test_not_installed_message() {
        let msg = not_installed_message();
        assert!(msg.contains("npm i -g nori-skillsets"));
    }

    #[test]
    fn test_filter_short_output_three_lines() {
        let stdout = "Line one\nLine two\nLine three\n";
        let result = filter_install_output(stdout, "test");
        assert_eq!(result, "Line one");
    }

    #[test]
    fn test_filter_long_output_takes_last_section() {
        let stdout = "Setting up Nori...\nDoing stuff\nMore stuff\nEven more\n\nSkillset \"test\" is now active.\nRestart Claude Code to apply.\n";
        let result = filter_install_output(stdout, "test");
        assert_eq!(result, "Skillset \"test\" is now active.");
    }

    #[test]
    fn test_filter_long_output_no_double_newline_falls_back() {
        let stdout = "Line one\nLine two\nLine three\nLine four\n";
        let result = filter_install_output(stdout, "test");
        assert_eq!(result, "Line one");
    }

    #[test]
    fn test_filter_empty_output_returns_default() {
        let stdout = "";
        let result = filter_install_output(stdout, "my-skillset");
        assert_eq!(result, "Skillset 'my-skillset' installed successfully");
    }

    #[test]
    fn test_filter_real_world_example() {
        let stdout = r#"Setting up Nori for first time use...

Warning: ⚠️  Nori managed installation detected in ancestor directory!

Claude Code loads CLAUDE.md files from all parent directories.
Having multiple Nori managed installations can cause duplicate or conflicting configurations.

Existing Nori managed installations found at:
  • /home/clifford/Documents/source/nori/registrar
  • /home/clifford/Documents/source/nori

Please remove the conflicting managed installation before continuing.

✓ Nori initialized successfully
Error: You do not have access to organization "dev".

Cannot download "dev/test-onboard" from https://dev.noriskillsets.dev.

Your available organizations: org-alpha, org-beta
Warning: Skillset "dev/test-onboard" not found in registry. Using locally installed version.
Switched to "dev/test-onboard" profile for Claude Code
Restart Claude Code to load the new profile configuration

Skillset "dev/test-onboard" is now active.
Restart Claude Code to apply the new skillset."#;
        let result = filter_install_output(stdout, "dev/test-onboard");
        assert_eq!(result, r#"Skillset "dev/test-onboard" is now active."#);
    }

    #[test]
    fn test_filter_long_output_last_section_has_empty_first_line() {
        let stdout = "Line 1\nLine 2\nLine 3\nLine 4\n\n\nActual message here.\n";
        let result = filter_install_output(stdout, "test");
        assert_eq!(result, "Actual message here.");
    }

    #[test]
    fn test_parse_skillset_names() {
        // Test the parsing logic used in list_skillsets
        let stdout = "rust-dev\npython-ml\n  web-frontend  \n\njava-backend\n";
        let names: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        assert_eq!(names.len(), 4);
        assert_eq!(names[0], "rust-dev");
        assert_eq!(names[1], "python-ml");
        assert_eq!(names[2], "web-frontend");
        assert_eq!(names[3], "java-backend");
    }
}
