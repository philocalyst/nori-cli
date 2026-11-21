use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use pretty_assertions::assert_eq;

use super::ChangeKind;
use super::Limits;
use super::ShadowTree;
use super::SnapshotError;
use super::TreeChange;
use crate::undo::trash::UndoTrash;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A project that is a git work tree (`.gitignore` hides `ignored.txt` and `build/`), a Nori
/// home for the shadow repository and the trash, all temporary.
struct Project {
    _root: tempfile::TempDir,
    dir: PathBuf,
    home: PathBuf,
}

impl Project {
    fn new() -> Option<Self> {
        if !git_available() {
            eprintln!("git is not installed; skipping");
            return None;
        }
        let root = tempfile::tempdir().unwrap();
        // The temporary directory may be reached through a symlink (macOS `/var`); git reports real
        // paths.
        let real_root = root.path().canonicalize().unwrap();
        let dir = real_root.join("project");
        std::fs::create_dir_all(&dir).unwrap();
        let home = real_root.join("home");
        let init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(init.status.success());
        std::fs::write(dir.join(".gitignore"), "ignored.txt\nbuild/\n").unwrap();
        Some(Self {
            _root: root,
            dir,
            home,
        })
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(relative)).ok()
    }

    async fn tree(&self) -> ShadowTree {
        ShadowTree::open(&self.dir, &self.home, Limits::DEFAULT)
            .await
            .unwrap()
    }

    fn trash(&self) -> UndoTrash {
        UndoTrash::at(self.home.join("undo-trash"))
    }
}

#[tokio::test]
async fn capture_is_stable_until_a_file_changes() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "one");
    let tree = project.tree().await;
    let first = tree.capture().await.unwrap();
    assert_eq!(tree.capture().await.unwrap(), first);

    project.write("a.txt", "two");
    let second = tree.capture().await.unwrap();
    assert_eq!(
        tree.changes(&first, &second).await.unwrap(),
        vec![TreeChange {
            kind: ChangeKind::Modified,
            path: "a.txt".to_string(),
        }]
    );
}

#[tokio::test]
async fn shadow_repository_lives_under_nori_home_and_the_project_stays_clean() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "one");
    let tree = project.tree().await;
    tree.capture().await.unwrap();

    assert!(tree.git_dir.starts_with(&project.home));
    let in_project: Vec<String> = std::fs::read_dir(&project.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut in_project = in_project;
    in_project.sort();
    assert_eq!(in_project, vec![".git", ".gitignore", "a.txt"]);
    // The project's own index was never written by a capture.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&project.dir)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "?? .gitignore\n?? a.txt\n"
    );
}

#[tokio::test]
async fn a_change_with_no_diff_report_is_reverted() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "keep me\nold line\n");
    project.write("src/b.txt", "delete me not");
    project.write("ignored.txt", "ignored before");
    let tree = project.tree().await;
    let before = tree.capture().await.unwrap();

    // What `sed -i`, `rm` and `touch` would do, with no diff reported anywhere.
    project.write("a.txt", "keep me\nnew line\n");
    std::fs::remove_file(project.dir.join("src/b.txt")).unwrap();
    project.write("made/c.txt", "created by a command");
    project.write("ignored.txt", "ignored after");
    project.write("build/out.bin", "artifact");

    let report = tree.restore(&before, &project.trash(), 7).await.unwrap();

    assert_eq!(
        project.read("a.txt").as_deref(),
        Some("keep me\nold line\n")
    );
    assert_eq!(project.read("src/b.txt").as_deref(), Some("delete me not"));
    assert_eq!(project.read("made/c.txt"), None);
    assert!(
        !project.dir.join("made").exists(),
        "the directory the turn created is gone with its file"
    );
    // Ignored files are not the snapshot's business.
    assert_eq!(
        project.read("ignored.txt").as_deref(),
        Some("ignored after")
    );
    assert_eq!(project.read("build/out.bin").as_deref(), Some("artifact"));
    let mut restored = report.restored.clone();
    restored.sort();
    assert_eq!(
        restored,
        vec![
            project.dir.join("a.txt"),
            project.dir.join("made/c.txt"),
            project.dir.join("src/b.txt"),
        ]
    );
    assert_eq!(report.skipped, Vec::new());
    // Restoring left the tree as it was captured.
    assert_eq!(tree.capture().await.unwrap(), before);
}

#[tokio::test]
async fn what_restoring_overwrites_or_removes_is_kept_in_the_trash() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "original");
    let tree = project.tree().await;
    let before = tree.capture().await.unwrap();
    project.write("a.txt", "changed by the turn");
    project.write("new.txt", "created by the turn");

    let report = tree.restore(&before, &project.trash(), 3).await.unwrap();

    let mut kept: Vec<(PathBuf, String)> = report
        .kept
        .iter()
        .map(|(path, saved)| (path.clone(), std::fs::read_to_string(saved).unwrap()))
        .collect();
    kept.sort();
    assert_eq!(
        kept,
        vec![
            (project.dir.join("a.txt"), "changed by the turn".to_string()),
            (
                project.dir.join("new.txt"),
                "created by the turn".to_string()
            ),
        ]
    );
    assert!(
        report
            .kept
            .iter()
            .all(|(_, saved)| saved.starts_with(project.home.join("undo-trash")))
    );
}

#[tokio::test]
async fn the_real_repositorys_private_excludes_apply() {
    let Some(project) = Project::new() else {
        return;
    };
    let exclude = project.dir.join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(exclude, "private/\n").unwrap();
    project.write("a.txt", "one");
    let tree = project.tree().await;
    let first = tree.capture().await.unwrap();

    project.write("private/notes.txt", "scratch");
    assert_eq!(tree.capture().await.unwrap(), first);
}

#[tokio::test]
async fn a_directory_outside_any_git_work_tree_has_no_snapshots() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let result = ShadowTree::open(root.path(), &root.path().join("home"), Limits::DEFAULT).await;
    assert!(matches!(result, Err(SnapshotError::NoGit)), "{result:?}");
}

#[tokio::test]
async fn a_tree_with_too_many_files_turns_snapshots_off() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "1");
    project.write("b.txt", "2");
    let tree = ShadowTree::open(
        &project.dir,
        &project.home,
        Limits {
            max_entries: 1,
            ..Limits::DEFAULT
        },
    )
    .await
    .unwrap();

    assert!(matches!(
        tree.capture().await,
        Err(SnapshotError::TooLarge { .. })
    ));
    // Off for the session: later captures do not run git.
    assert!(matches!(
        tree.capture().await,
        Err(SnapshotError::TooLarge { .. })
    ));
}

#[tokio::test]
async fn a_slow_first_capture_turns_snapshots_off() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "1");
    let tree = ShadowTree::open(
        &project.dir,
        &project.home,
        Limits {
            first_capture: std::time::Duration::ZERO,
            ..Limits::DEFAULT
        },
    )
    .await
    .unwrap();

    assert!(matches!(
        tree.capture().await,
        Err(SnapshotError::TooLarge { .. })
    ));
}

#[tokio::test]
async fn maintenance_runs_once_a_day() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "1");
    let tree = project.tree().await;
    tree.capture().await.unwrap();

    tree.maintain().await;
    let marker = tree.git_dir.join("nori-last-gc");
    let first = std::fs::metadata(&marker).unwrap().modified().unwrap();
    assert!(Path::new(&marker).exists());
    tree.maintain().await;
    assert_eq!(
        std::fs::metadata(&marker).unwrap().modified().unwrap(),
        first
    );
}

/// `NORI_SNAPSHOT_MEASURE=<dir> cargo test -p nori-harness --lib measure_capture -- --ignored
/// --nocapture` times the first (cold) and later (warm) captures of a real project; the shadow
/// repository goes in a temporary directory and the project is only read.
#[tokio::test]
#[ignore = "measures a real project"]
async fn measure_capture_on_a_real_project() {
    let Some(dir) = std::env::var_os("NORI_SNAPSHOT_MEASURE") else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let limits = Limits {
        first_capture: std::time::Duration::from_secs(600),
        max_entries: i64::MAX,
    };
    let tree = ShadowTree::open(Path::new(&dir), home.path(), limits)
        .await
        .unwrap();
    for label in ["cold", "warm 1", "warm 2", "warm 3"] {
        let started = std::time::Instant::now();
        tree.capture().await.unwrap();
        eprintln!("{label}: {:?}", started.elapsed());
    }
    let listing = tree
        .run("ls-files", &["ls-files", "-z"], &tree.index, None)
        .await
        .unwrap();
    let mut by_top: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for path in String::from_utf8_lossy(&listing)
        .split('\0')
        .filter(|p| !p.is_empty())
    {
        let top = path.split('/').next().unwrap_or_default().to_string();
        *by_top.entry(top).or_default() += 1;
    }
    let mut sizes: Vec<(u64, String)> = String::from_utf8_lossy(&listing)
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter_map(|path| {
            let size = std::fs::symlink_metadata(Path::new(&dir).join(path))
                .ok()?
                .len();
            Some((size, path.to_string()))
        })
        .collect();
    sizes.sort();
    eprintln!(
        "bytes: {}; largest: {:?}",
        sizes.iter().map(|(size, _)| size).sum::<u64>(),
        &sizes[sizes.len().saturating_sub(5)..]
    );
    let total: i64 = by_top.values().sum();
    let mut top: Vec<_> = by_top.into_iter().collect();
    top.sort_by_key(|(_, count)| -count);
    eprintln!(
        "entries: {total}; largest top-level entries: {:?}",
        &top[..top.len().min(6)]
    );
}
