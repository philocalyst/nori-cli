//! Path utility functions

use std::collections::BTreeMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// The directory Claude Code keeps one login's settings, plans and sessions in: `CLAUDE_CONFIG_DIR`
/// from the agent's own environment (an account's home), then from the process, then
/// `~/.claude`. Every in-process Claude agent shares the process, so an account's own
/// environment decides first.
pub fn claude_config_dir(environment: &BTreeMap<String, String>) -> Option<PathBuf> {
    environment
        .get("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

/// Check if a file path is within a login's plans directory (`<config dir>/plans/`)
///
/// This function handles:
/// - Absolute paths: /Users/soddy/.claude/plans/plan.md
/// - Home-relative paths: ~/.claude/plans/plan.md
/// - Cross-platform compatibility (Windows, macOS, Linux)
///
/// # Arguments
///
/// * `path_str` - The file path to check
/// * `plans_dir` - The login's plans directory (`<config dir>/plans`)
///
/// # Returns
///
/// * `true` if the path is within `plans_dir`
/// * `false` otherwise
pub fn is_plans_directory_path(path_str: &str, plans_dir: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        tracing::warn!("Could not determine home directory for plans path check");
        return false;
    };

    let plans_dir = plans_dir.to_path_buf();

    let normalized_input = if let Some(rest) = path_str.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(path_str).is_absolute() {
        Path::new(path_str).to_path_buf()
    } else {
        return false;
    };

    let plans_canonical = match plans_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => plans_dir,
    };

    if normalized_input.exists() {
        let input_canonical = match normalized_input.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                match normalized_input
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                {
                    Some(parent) => parent.join(normalized_input.file_name().unwrap_or_default()),
                    None => return false,
                }
            }
        };
        return input_canonical.starts_with(&plans_canonical);
    }

    if normalized_input.starts_with(&plans_canonical) {
        return true;
    }

    let input_components = normalize_path_components(&normalized_input);
    let plans_components = normalize_path_components(&plans_canonical);

    if input_components.len() >= plans_components.len() {
        for (i, input_comp) in input_components
            .iter()
            .enumerate()
            .take(plans_components.len())
        {
            if input_comp != &plans_components[i] {
                return false;
            }
        }
        return true;
    }

    false
}

/// Normalize path components for cross-platform comparison
///
/// This function decomposes a path into its components and returns
/// a vector of component strings. It handles:
/// - Filtering out `.` (current directory) components
/// - Preserving `..` (parent directory) components
/// - Converting root directory to platform-specific format
/// - Including Windows drive prefix for accurate comparison
/// - Handling non-UTF-8 path components gracefully
fn normalize_path_components(path: &Path) -> Vec<String> {
    let mut components = Vec::new();

    for c in path.components() {
        match c {
            Component::Prefix(prefix) => {
                components.push(prefix.as_os_str().to_string_lossy().to_string());
            }
            Component::RootDir => {
                components.push("/".to_string());
            }
            Component::Normal(s) => {
                components.push(s.to_string_lossy().to_string());
            }
            Component::ParentDir => components.push("..".to_string()),
            Component::CurDir => {}
        }
    }

    components
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_plans_directory_path() {
        let home = dirs::home_dir().unwrap();

        // Test absolute path
        let plans_path = home.join(".claude").join("plans").join("plan.md");
        assert!(is_plans_directory_path(
            plans_path.to_str().unwrap(),
            &dirs::home_dir().unwrap().join(".claude/plans"),
        ));

        // Test ~ expansion
        assert!(is_plans_directory_path(
            "~/.claude/plans/plan.md",
            &dirs::home_dir().unwrap().join(".claude/plans")
        ));

        // Test non-plans path
        assert!(!is_plans_directory_path(
            "/tmp/plan.md",
            &dirs::home_dir().unwrap().join(".claude/plans")
        ));
        assert!(!is_plans_directory_path(
            "~/other/path/plan.md",
            &dirs::home_dir().unwrap().join(".claude/plans")
        ));

        // Test edge case: similar but not plans directory
        assert!(!is_plans_directory_path(
            "~/../.claude/plans/plan.md",
            &dirs::home_dir().unwrap().join(".claude/plans")
        ));
    }

    #[test]
    fn test_normalize_path_components() {
        use std::path::Path;

        // Test Unix path
        let path = Path::new("/home/user/.claude/plans/plan.md");
        let components = normalize_path_components(path);
        assert_eq!(components[0], "/");
        assert_eq!(components[1], "home");
        assert_eq!(components[2], "user");
        assert_eq!(components[3], ".claude");
        assert_eq!(components[4], "plans");
        assert_eq!(components[5], "plan.md");
    }
}
