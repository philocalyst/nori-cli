//! What Codex tells the model about the user and the project, found as Codex finds it.
//!
//! - The user's: `$CODEX_HOME/AGENTS.override.md`, else `$CODEX_HOME/AGENTS.md`.
//! - The project's (`project_doc`): in every directory from the project root down to the working
//!   directory, the first of `AGENTS.override.md`, `AGENTS.md` and the configured
//!   `project_doc_fallback_filenames`; root first, within `project_doc_max_bytes` in total
//!   (32 KiB by default; `0` turns them off). A project marked untrusted gets none.

use std::path::Path;
use std::path::PathBuf;

use nori_engine::Instruction;

use super::config::CodexConfig;
use super::config::Trust;

const OVERRIDE: &str = "AGENTS.override.md";
const DEFAULT: &str = "AGENTS.md";

/// The instructions for `cwd`, user's first. Blocking: call it off the async threads.
pub(crate) fn find(
    home: &Path,
    cwd: &Path,
    config: &CodexConfig,
) -> (Vec<Instruction>, Vec<String>) {
    let mut found = Vec::new();
    let mut warnings = Vec::new();
    if let Some((source, text)) = [OVERRIDE, DEFAULT]
        .into_iter()
        .map(|name| home.join(name))
        .find_map(|path| Some((path.clone(), std::fs::read_to_string(&path).ok()?)))
        .filter(|(_, text)| !text.trim().is_empty())
    {
        found.push(Instruction { source, text });
    }
    let budget = config.project_doc_max_bytes();
    if config.trust == Trust::Untrusted || budget <= 0 {
        return (found, warnings);
    }
    let mut names = vec![OVERRIDE.to_owned(), DEFAULT.to_owned()];
    for name in config.project_doc_fallback_filenames() {
        if !name.is_empty()
            && !matches!(name.as_str(), "." | "..")
            && !name.contains(['/', '\\', '\0'])
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    let top = config
        .project_root
        .clone()
        .unwrap_or_else(|| cwd.to_path_buf());
    let mut directories: Vec<&Path> = cwd
        .ancestors()
        .take_while(|directory| directory.starts_with(&top))
        .collect();
    directories.reverse();
    let mut remaining = usize::try_from(budget).unwrap_or(usize::MAX);
    for directory in directories {
        let Some(path) = names
            .iter()
            .map(|name| directory.join(name))
            .find(|path: &PathBuf| path.is_file())
        else {
            continue;
        };
        if remaining == 0 {
            warnings.push(format!(
                "{} was skipped: the {budget}-byte project instruction budget is spent (raise `project_doc_max_bytes`).",
                path.display()
            ));
            break;
        }
        let Ok(mut bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() > remaining {
            warnings.push(format!(
                "Only {remaining} of {} bytes of {} were loaded under the {budget}-byte project instruction budget.",
                bytes.len(),
                path.display()
            ));
            bytes.truncate(remaining);
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.trim().is_empty() {
            continue;
        }
        remaining -= bytes.len();
        found.push(Instruction { source: path, text });
    }
    (found, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_user_file_then_the_project_files_from_the_root_down() {
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let work = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let root = work.path();
        let sub = root.join("crate");
        std::fs::create_dir_all(root.join(".git")).unwrap_or_else(|error| panic!("{error}"));
        std::fs::create_dir_all(&sub).unwrap_or_else(|error| panic!("{error}"));
        for (path, text) in [
            (home.path().join(DEFAULT), "user"),
            (root.join(DEFAULT), "root"),
            (sub.join(DEFAULT), "ignored"),
            (sub.join(OVERRIDE), "override"),
        ] {
            std::fs::write(path, text).unwrap_or_else(|error| panic!("{error}"));
        }
        let config = CodexConfig::load(home.path(), Some(&sub), &[]);
        let (found, _) = find(home.path(), &sub, &config);
        let texts: Vec<&str> = found
            .iter()
            .map(|instruction| instruction.text.as_str())
            .collect();
        assert_eq!(texts, vec!["user", "root", "override"]);
    }
}
