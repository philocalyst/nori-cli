//! Tests of cancellation, dry-run previews and external-directory reporting.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::ChangeKind;
use crate::DirEntry;
use crate::FileStat;
use crate::LocalFs;
use crate::PermissionAction;
use crate::ToolFs;
use crate::Workspace;
use crate::WorkspaceConfig;

/// A filesystem that can stall in `read` or `write` until released.
#[derive(Debug, Default)]
struct StallFs {
    stall_read: bool,
    stall_write: bool,
    fail_remove: bool,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl ToolFs for StallFs {
    async fn stat(&self, path: &Path) -> io::Result<FileStat> {
        LocalFs.stat(path).await
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if self.stall_read {
            self.entered.notify_one();
            self.release.notified().await;
        }
        LocalFs.read(path).await
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        if self.stall_write {
            self.entered.notify_one();
            self.release.notified().await;
        }
        LocalFs.write(path, contents).await
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        if self.fail_remove {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot remove",
            ));
        }
        LocalFs.remove_file(path).await
    }

    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        LocalFs.read_dir(path).await
    }
}

fn fixture(content: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), content).expect("write");
    dir
}

fn edit_args() -> serde_json::Value {
    json!({"file_path": "a.txt", "old_string": "one", "new_string": "1"})
}

// ---- cancellation ----

#[tokio::test]
async fn a_token_cancelled_up_front_does_nothing() {
    let dir = fixture("one\n");
    let ws = Workspace::local(dir.path());
    let token = CancellationToken::new();
    token.cancel();
    let result = ws
        .call_with_cancel("read", json!({"file_path": "a.txt"}), token)
        .await;
    assert!(result.is_error);
    assert_eq!(result.text, "cancelled by the user");
    assert!(ws.ledger().snapshot().entries.is_empty());
}

#[tokio::test]
async fn cancelling_before_the_write_leaves_the_file_untouched() {
    let dir = fixture("one\n");
    let fs = Arc::new(StallFs {
        stall_read: true,
        ..StallFs::default()
    });
    let ws = Workspace::new(
        dir.path(),
        fs.clone(),
        WorkspaceConfig {
            require_read_before_edit: false,
            ..WorkspaceConfig::default()
        },
    );
    let token = CancellationToken::new();
    let call = tokio::spawn({
        let (ws, token) = (ws.clone(), token.clone());
        async move { ws.call_with_cancel("edit", edit_args(), token).await }
    });
    fs.entered.notified().await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("cancel is prompt")
        .expect("join");
    assert!(result.is_error);
    assert_eq!(result.text, "cancelled by the user");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
        "one\n"
    );
    // Nothing leaked into the directory either.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .expect("dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(names.len(), 1);
}

#[tokio::test]
async fn a_cancel_during_the_write_lets_it_finish_atomically() {
    let dir = fixture("one\n");
    let fs = Arc::new(StallFs {
        stall_write: true,
        ..StallFs::default()
    });
    let ws = Workspace::new(
        dir.path(),
        fs.clone(),
        WorkspaceConfig {
            require_read_before_edit: false,
            ..WorkspaceConfig::default()
        },
    );
    let token = CancellationToken::new();
    let call = tokio::spawn({
        let (ws, token) = (ws.clone(), token.clone());
        async move { ws.call_with_cancel("edit", edit_args(), token).await }
    });
    fs.entered.notified().await;
    token.cancel();
    // The write is in flight: release it and the call completes with its real result.
    fs.release.notify_one();
    let result = call.await.expect("join");
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
        "1\n"
    );
    let record = ws
        .ledger()
        .record(&dir.path().join("a.txt"))
        .expect("ledger updated");
    assert!(record.written_by_tool);
}

#[tokio::test]
async fn a_never_cancelled_token_behaves_like_call() {
    let dir = fixture("one\n");
    let ws = Workspace::local(dir.path());
    let read = ws
        .call_with_cancel(
            "read",
            json!({"file_path": "a.txt"}),
            CancellationToken::new(),
        )
        .await;
    assert!(!read.is_error);
    let edit = ws
        .call_with_cancel("edit", edit_args(), CancellationToken::new())
        .await;
    assert!(!edit.is_error, "{}", edit.text);
}

// ---- preview ----

#[tokio::test]
async fn preview_shows_the_exact_change_without_writing_or_touching_the_ledger() {
    let dir = fixture("fn a() {\r\n    1\r\n}\r\n");
    let ws = Workspace::local(dir.path());
    ws.call("read", json!({"file_path": "a.txt"})).await;
    let ledger_before = ws.ledger().snapshot();

    let args = json!({"file_path": "a.txt", "old_string": "fn a() {\n    1\n}", "new_string": "fn a() {\n    2\n}"});
    let preview = ws.preview("edit", args.clone()).await.expect("preview");
    let path = dir.path().join("a.txt");
    assert_eq!(preview.action, PermissionAction::Edit);
    assert_eq!(preview.paths.len(), 1);
    assert_eq!(preview.paths[0].path, path);
    assert!(!preview.paths[0].external);
    assert_eq!(preview.changes.len(), 1);
    assert_eq!(preview.changes[0].kind, ChangeKind::Modified);
    assert_eq!(
        preview.changes[0].old_text.as_deref(),
        Some("fn a() {\r\n    1\r\n}\r\n")
    );
    assert_eq!(preview.changes[0].new_text, "fn a() {\r\n    2\r\n}\r\n");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "fn a() {\r\n    1\r\n}\r\n"
    );
    assert_eq!(ws.ledger().snapshot(), ledger_before);

    // The real call produces exactly the previewed text.
    let result = ws.call("edit", args).await;
    assert_eq!(
        result.changes[0].new_text.as_deref(),
        Some(preview.changes[0].new_text.as_str())
    );
}

#[tokio::test]
async fn preview_of_a_failing_call_returns_the_same_error() {
    let dir = fixture("one\ntwo\n");
    let ws = Workspace::local(dir.path());

    // Unread file.
    let preview = ws.preview("edit", edit_args()).await.expect_err("fails");
    let real = ws.call("edit", edit_args()).await;
    assert_eq!(preview.to_string(), real.text);

    // No match, with the same closest-lines hint.
    ws.call("read", json!({"file_path": "a.txt"})).await;
    let args = json!({"file_path": "a.txt", "old_string": "tw0", "new_string": "x"});
    let preview = ws.preview("edit", args.clone()).await.expect_err("fails");
    assert_eq!(preview.to_string(), ws.call("edit", args).await.text);

    // Changed on disk.
    std::fs::write(dir.path().join("a.txt"), "other\n").expect("write");
    let preview = ws
        .preview("write", json!({"file_path": "a.txt", "content": "x"}))
        .await
        .expect_err("fails");
    assert!(preview.to_string().contains("changed on disk"), "{preview}");

    // Unknown tool and bad arguments.
    assert!(ws.preview("bash", json!({})).await.is_err());
    assert!(ws.preview("edit", json!({"file_path": "a"})).await.is_err());
}

#[tokio::test]
async fn preview_of_a_read_has_no_changes_and_a_missing_file_fails() {
    let dir = fixture("one\n");
    let ws = Workspace::local(dir.path());
    let preview = ws
        .preview("Read", json!({"path": "a.txt"}))
        .await
        .expect("preview");
    assert_eq!(preview.action, PermissionAction::Read);
    assert!(preview.changes.is_empty());
    assert!(ws.ledger().snapshot().entries.is_empty());
    let missing = ws
        .preview("read", json!({"file_path": "nope.txt"}))
        .await
        .expect_err("missing");
    assert!(missing.to_string().contains("File not found"), "{missing}");
}

#[tokio::test]
async fn preview_creates_and_no_ops_and_multi_edit() {
    let dir = fixture("one\ntwo\n");
    let ws = Workspace::local(dir.path());
    let created = ws
        .preview(
            "write",
            json!({"file_path": "new/b.txt", "content": "hi\n"}),
        )
        .await
        .expect("preview");
    assert_eq!(created.changes[0].kind, ChangeKind::Created);
    assert_eq!(created.changes[0].old_text, None);
    assert!(!dir.path().join("new").exists());

    ws.call("read", json!({"file_path": "a.txt"})).await;
    let same = ws
        .preview(
            "write",
            json!({"file_path": "a.txt", "content": "one\ntwo\n"}),
        )
        .await
        .expect("preview");
    assert!(same.changes.is_empty());

    let multi = ws
        .preview(
            "multi_edit",
            json!({"file_path": "a.txt", "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "two", "new_string": "2"},
            ]}),
        )
        .await
        .expect("preview");
    assert_eq!(multi.changes[0].new_text, "1\n2\n");
}

#[tokio::test]
async fn preview_of_a_patch_lists_every_path_and_change() {
    let dir = fixture("one\n");
    std::fs::write(dir.path().join("gone.txt"), "bye\n").expect("write");
    let ws = Workspace::local(dir.path());
    let patch = "*** Begin Patch\n*** Add File: n.txt\n+new\n*** Update File: a.txt\n*** Move to: m.txt\n-one\n+uno\n*** Delete File: gone.txt\n*** End Patch\n";
    let preview = ws
        .preview("apply_patch", json!(patch))
        .await
        .expect("preview");
    assert_eq!(preview.action, PermissionAction::Edit);
    let names: Vec<_> = preview
        .paths
        .iter()
        .map(|path| {
            path.path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["a.txt", "gone.txt", "m.txt", "n.txt"]);
    let mut kinds: Vec<_> = preview
        .changes
        .iter()
        .map(|change| {
            (
                change
                    .path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
                change.kind,
            )
        })
        .collect();
    kinds.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kinds,
        vec![
            ("a.txt".to_owned(), ChangeKind::Deleted),
            ("gone.txt".to_owned(), ChangeKind::Deleted),
            ("m.txt".to_owned(), ChangeKind::Created),
            ("n.txt".to_owned(), ChangeKind::Created),
        ]
    );
    assert!(dir.path().join("a.txt").exists() && !dir.path().join("m.txt").exists());
}

// ---- external directories ----

#[tokio::test]
async fn paths_outside_the_roots_are_reported_not_refused() {
    let project = TempDir::new().expect("project");
    let extra = TempDir::new().expect("extra");
    let outside = TempDir::new().expect("outside");
    let config = WorkspaceConfig {
        roots: vec![extra.path().to_path_buf()],
        ..WorkspaceConfig::default()
    };
    let ws = Workspace::new(project.path(), Arc::new(LocalFs), config);

    assert!(!ws.is_external(&project.path().join("src/a.rs")));
    assert!(!ws.is_external(Path::new("src/a.rs")));
    assert!(!ws.is_external(&extra.path().join("b.rs")));
    assert!(ws.is_external(&outside.path().join("c.rs")));
    assert!(ws.is_external(&project.path().join("../escape.rs")));
    assert!(ws.is_external(Path::new("../escape.rs")));
    // A sibling whose name merely starts with the root's name is outside it.
    let sibling: PathBuf = format!("{}-sibling/x", project.path().display()).into();
    assert!(ws.is_external(&sibling));

    let target = outside.path().join("c.txt");
    let preview = ws
        .preview(
            "write",
            json!({"file_path": target.display().to_string(), "content": "x"}),
        )
        .await
        .expect("preview");
    assert!(preview.paths[0].external && preview.any_external());
    // The tools do not refuse it: that is the permission layer's call.
    let written = ws
        .call(
            "write",
            json!({"file_path": target.display().to_string(), "content": "x"}),
        )
        .await;
    assert!(!written.is_error, "{}", written.text);

    let inside = ws
        .preview(
            "write",
            json!({"file_path": extra.path().join("d.txt").display().to_string(), "content": "x"}),
        )
        .await
        .expect("preview");
    assert!(!inside.any_external());
}

#[tokio::test]
async fn failed_patch_deletion_preserves_read_ledger() {
    let dir = fixture("one\n");
    let fs = Arc::new(StallFs {
        fail_remove: true,
        ..StallFs::default()
    });
    let ws = Workspace::new(dir.path(), fs, WorkspaceConfig::default());
    assert!(
        !ws.call("read", json!({"file_path": "a.txt"}))
            .await
            .is_error
    );
    let before = ws.ledger().snapshot();
    let result = ws
        .call(
            "apply_patch",
            json!("*** Begin Patch\n*** Delete File: a.txt\n*** End Patch"),
        )
        .await;
    assert!(result.is_error);
    assert_eq!(ws.ledger().snapshot(), before);
}
