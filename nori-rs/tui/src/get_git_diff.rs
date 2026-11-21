//! The working tree's changes, for `/diff`: what a pull request of it would show (the changes
//! since the merge-base with the default branch, plus untracked files), as both versions of every
//! file, so they are drawn like any other edit.
//!
//! The repository is read in-process; nothing here starts `git`.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use nori_config::GitRepo;

use crate::ui_types::FileChange;

/// What `/diff` found.
#[derive(Debug)]
pub(crate) enum GitChanges {
    /// The directory is not inside a git work tree.
    NotARepo,
    /// The repository could not be read.
    Failed(String),
    /// The changed files by path, and the ones that are not text (named, not drawn).
    Changes {
        files: HashMap<PathBuf, FileChange>,
        binary: Vec<String>,
    },
}

/// The changes of the work tree `dir` is in.
pub(crate) async fn get_git_changes(dir: &Path) -> GitChanges {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || changes_in(&dir))
        .await
        .unwrap_or_else(|error| GitChanges::Failed(error.to_string()))
}

fn changes_in(dir: &Path) -> GitChanges {
    let Some((repo, work_dir)) =
        GitRepo::discover(dir).and_then(|repo| repo.work_dir().map(|work_dir| (repo, work_dir)))
    else {
        return GitChanges::NotARepo;
    };
    let Some(versions) = repo.file_versions_since(&repo.pr_diff_base()) else {
        return GitChanges::Failed("the base of the diff is not a commit".to_owned());
    };
    let mut files = HashMap::new();
    let mut binary = Vec::new();
    // A side with a NUL byte in it, or that is not UTF-8, is not text.
    let text = |bytes: Option<Vec<u8>>| match bytes {
        Some(bytes) if bytes.contains(&0) => Err(()),
        Some(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| ()),
        None => Ok(None),
    };
    for file in versions {
        let change = match (text(file.old), text(file.new)) {
            (Ok(None), Ok(Some(content))) => FileChange::Add { content },
            (Ok(Some(content)), Ok(None)) => FileChange::Delete { content },
            (Ok(Some(old_text)), Ok(Some(new_text))) => FileChange::Update {
                old_text,
                new_text,
                first_line: 1,
                move_path: None,
            },
            (Ok(None), Ok(None)) => continue,
            (Err(()), _) | (_, Err(())) => {
                binary.push(file.path);
                continue;
            }
        };
        files.insert(work_dir.join(&file.path), change);
    }
    GitChanges::Changes { files, binary }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tempfile::TempDir;

    /// Create a temp git repo with an initial commit and return the TempDir.
    async fn create_temp_git_repo() -> TempDir {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path();

        // git init + initial commit
        run_in(path, &["git", "init"]).await;
        run_in(path, &["git", "config", "user.email", "test@test.com"]).await;
        run_in(path, &["git", "config", "user.name", "Test"]).await;
        std::fs::write(path.join("file.txt"), "initial\n").unwrap();
        run_in(path, &["git", "add", "."]).await;
        run_in(path, &["git", "commit", "-m", "init"]).await;

        dir
    }

    /// The changed files' names, relative to the repository.
    async fn changed_paths(dir: &Path) -> Vec<String> {
        let GitChanges::Changes { files, .. } = get_git_changes(dir).await else {
            panic!("expected changes");
        };
        let root = std::fs::canonicalize(dir).unwrap();
        let mut paths: Vec<String> = files
            .keys()
            .map(|path| {
                let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                path.strip_prefix(&root)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
            })
            .collect();
        paths.sort();
        paths
    }

    async fn run_in(dir: &Path, args: &[&str]) {
        let status = tokio::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "command failed: {args:?}");
    }

    #[tokio::test]
    async fn diff_with_explicit_dir_shows_changes() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();

        // Modify the tracked file
        std::fs::write(path.join("file.txt"), "modified\n").unwrap();

        assert_eq!(changed_paths(path).await, ["file.txt"]);
    }

    #[tokio::test]
    async fn diff_with_explicit_dir_includes_untracked_files() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();

        // Create an untracked file
        std::fs::write(path.join("new_file.txt"), "new content\n").unwrap();

        assert_eq!(changed_paths(path).await, ["new_file.txt"]);
    }

    #[tokio::test]
    async fn diff_with_non_git_dir_returns_not_git() {
        let dir = TempDir::new().expect("failed to create temp dir");
        assert!(matches!(
            get_git_changes(dir.path()).await,
            GitChanges::NotARepo
        ));
    }

    #[tokio::test]
    async fn diff_includes_branch_commits_not_just_uncommitted() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();

        // Create a "main" branch reference at current HEAD
        run_in(path, &["git", "branch", "-M", "main"]).await;

        // Create a feature branch and make a committed change
        run_in(path, &["git", "checkout", "-b", "feature"]).await;
        std::fs::write(path.join("file.txt"), "changed on feature\n").unwrap();
        run_in(path, &["git", "add", "."]).await;
        run_in(path, &["git", "commit", "-m", "feature change"]).await;

        // No uncommitted changes, but there's a committed diff vs main
        assert_eq!(changed_paths(path).await, ["file.txt"]);
    }

    #[tokio::test]
    async fn diff_on_main_with_no_changes_is_empty() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();
        run_in(path, &["git", "branch", "-M", "main"]).await;

        assert!(changed_paths(path).await.is_empty());
    }
}
