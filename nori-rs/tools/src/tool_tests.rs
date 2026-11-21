//! End-to-end tests of the tools through `Workspace::call`, against real temp directories.

use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use crate::ChangeKind;
use crate::Workspace;

fn setup(files: &[(&str, &str)]) -> (TempDir, Workspace) {
    let dir = TempDir::new().expect("tempdir");
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, content).expect("write fixture");
    }
    let workspace = Workspace::local(dir.path());
    (dir, workspace)
}

fn disk(dir: &TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).expect("read back")
}

async fn read_all(ws: &Workspace, path: &str) {
    let result = ws.call("read", json!({"file_path": path})).await;
    assert!(!result.is_error, "{}", result.text);
}

// ---- read ----

#[tokio::test]
async fn read_numbers_lines_and_pages() {
    let content: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    let (_dir, ws) = setup(&[("a.txt", &content)]);
    let full = ws.call("read", json!({"file_path": "a.txt"})).await;
    assert!(
        full.text.starts_with("     1→line 1\n     2→line 2"),
        "{}",
        full.text
    );
    assert!(
        !full.text.contains('['),
        "a complete read has no footer: {}",
        full.text
    );

    let page = ws
        .call("Read", json!({"path": "a.txt", "offset": "3", "limit": 2}))
        .await;
    assert_eq!(
        page.text,
        "     3→line 3\n     4→line 4\n[Showing lines 3-4 of 10. Call `read` again with offset=5 to continue.]"
    );

    let tail = ws
        .call("read", json!({"file_path": "a.txt", "offset": 9}))
        .await;
    assert!(
        tail.text.ends_with("[End of file: 10 lines.]"),
        "{}",
        tail.text
    );
}

#[tokio::test]
async fn read_strips_crlf_and_truncates_long_lines() {
    let long = "x".repeat(2500);
    let (_dir, ws) = setup(&[("a.txt", &format!("a\r\n{long}\r\n"))]);
    let result = ws.call("read", json!({"file_path": "a.txt"})).await;
    assert!(
        result.text.starts_with("     1→a\n     2→xxx"),
        "{}",
        result.text
    );
    assert!(!result.text.contains('\r'));
    assert!(
        result.text.contains("[500 more characters]"),
        "{}",
        result.text
    );
    assert!(result.text.contains("shortened"), "{}", result.text);
}

#[tokio::test]
async fn read_errors_say_what_to_do() {
    let (dir, ws) = setup(&[("src/main.rs", "fn main() {}\n"), ("src/lib.rs", "")]);
    std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 3]).expect("write");

    let missing = ws.call("read", json!({"file_path": "src/main.rz"})).await;
    assert!(missing.is_error);
    assert!(missing.text.contains("File not found"), "{}", missing.text);
    assert!(missing.text.contains("src/main.rs"), "{}", missing.text);

    let missing_dir = ws.call("read", json!({"file_path": "scr/main.rs"})).await;
    assert!(
        missing_dir.text.contains("src/main.rs"),
        "{}",
        missing_dir.text
    );

    let binary = ws.call("read", json!({"file_path": "bin.dat"})).await;
    assert!(
        binary.is_error && binary.text.contains("binary"),
        "{}",
        binary.text
    );

    let past = ws
        .call("read", json!({"file_path": "src/main.rs", "offset": 9}))
        .await;
    assert!(
        past.is_error && past.text.contains("past the end"),
        "{}",
        past.text
    );

    let empty = ws.call("read", json!({"file_path": "src/lib.rs"})).await;
    assert!(
        !empty.is_error && empty.text.contains("is empty"),
        "{}",
        empty.text
    );
}

#[tokio::test]
async fn read_lists_directories_and_attaches_images() {
    let (dir, ws) = setup(&[("d/a.txt", "x"), ("d/sub/b.txt", "y")]);
    std::fs::write(dir.path().join("pic.png"), [137u8, 80, 78, 71]).expect("write");
    let listing = ws.call("read", json!({"file_path": "d"})).await;
    assert!(listing.text.contains("a.txt\nsub/"), "{}", listing.text);
    let image = ws.call("read", json!({"file_path": "pic.png"})).await;
    assert_eq!(image.images.len(), 1);
    assert_eq!(image.images[0].mime_type, "image/png");
}

// ---- the read ledger ----

#[tokio::test]
async fn editing_an_unread_file_is_refused_and_leaves_it_alone() {
    let (dir, ws) = setup(&[("a.txt", "hello\n")]);
    let result = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "hello", "new_string": "bye"}),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.text.contains("have not read") && result.text.contains("`read`"),
        "{}",
        result.text
    );
    assert_eq!(disk(&dir, "a.txt"), "hello\n");
}

#[tokio::test]
async fn editing_after_a_read_works_and_chains_without_rereading() {
    let (dir, ws) = setup(&[("a.txt", "one\ntwo\nthree\n")]);
    read_all(&ws, "a.txt").await;
    for (old, new) in [("one", "1"), ("two", "2")] {
        let result = ws
            .call(
                "edit",
                json!({"file_path": "a.txt", "old_string": old, "new_string": new}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text);
    }
    assert_eq!(disk(&dir, "a.txt"), "1\n2\nthree\n");
}

#[tokio::test]
async fn a_file_changed_on_disk_since_the_read_is_refused() {
    let (dir, ws) = setup(&[("a.txt", "one\n")]);
    read_all(&ws, "a.txt").await;
    std::fs::write(dir.path().join("a.txt"), "one changed by the user\n").expect("write");
    let result = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "one", "new_string": "1"}),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.text.contains("changed on disk") && result.text.contains("read"),
        "{}",
        result.text
    );
    assert_eq!(disk(&dir, "a.txt"), "one changed by the user\n");

    read_all(&ws, "a.txt").await;
    let retry = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "one", "new_string": "1"}),
        )
        .await;
    assert!(!retry.is_error, "{}", retry.text);
}

#[tokio::test]
async fn touching_a_file_without_changing_it_does_not_invalidate_the_read() {
    let (dir, ws) = setup(&[("a.txt", "one\n")]);
    read_all(&ws, "a.txt").await;
    std::fs::write(dir.path().join("a.txt"), "one\n").expect("rewrite identical");
    let result = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "one", "new_string": "1"}),
        )
        .await;
    assert!(!result.is_error, "{}", result.text);
}

#[tokio::test]
async fn write_to_an_existing_file_needs_a_read_but_a_new_file_does_not() {
    let (dir, ws) = setup(&[("a.txt", "old\n")]);
    let refused = ws
        .call("write", json!({"file_path": "a.txt", "content": "new\n"}))
        .await;
    assert!(
        refused.is_error && refused.text.contains("have not read"),
        "{}",
        refused.text
    );
    assert_eq!(disk(&dir, "a.txt"), "old\n");

    let created = ws
        .call(
            "write",
            json!({"file_path": "deep/dir/b.txt", "content": "fresh\n"}),
        )
        .await;
    assert!(!created.is_error, "{}", created.text);
    assert_eq!(disk(&dir, "deep/dir/b.txt"), "fresh\n");
    assert_eq!(created.changes[0].kind, ChangeKind::Created);

    read_all(&ws, "a.txt").await;
    let written = ws
        .call("Write", json!({"path": "a.txt", "contents": "new\n"}))
        .await;
    assert!(!written.is_error, "{}", written.text);
    assert_eq!(disk(&dir, "a.txt"), "new\n");
}

#[tokio::test]
async fn the_gate_can_be_turned_off_for_agents_that_read_through_the_shell() {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), "x\n").expect("write");
    let config = crate::WorkspaceConfig {
        require_read_before_edit: false,
        ..crate::WorkspaceConfig::default()
    };
    let ws = Workspace::new(dir.path(), std::sync::Arc::new(crate::LocalFs), config);
    let result = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "x", "new_string": "y"}),
        )
        .await;
    assert!(!result.is_error, "{}", result.text);
}

// ---- edit ----

#[tokio::test]
async fn edit_preserves_crlf_on_disk_and_reports_the_tier() {
    let (dir, ws) = setup(&[("a.txt", "fn a() {\r\n    1\r\n}\r\n")]);
    read_all(&ws, "a.txt").await;
    let result = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "fn a() {\n    1\n}", "new_string": "fn a() {\n    2\n}"}),
        )
        .await;
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(disk(&dir, "a.txt"), "fn a() {\r\n    2\r\n}\r\n");
    let summary = result.summary.clone().expect("summary");
    assert_eq!(
        (summary.lines_added, summary.lines_removed, summary.changed),
        (1, 1, true)
    );
    assert!(
        result
            .text
            .starts_with("Edited a.txt: 1 replacement, +1 -1 lines."),
        "{}",
        result.text
    );
    assert!(
        result
            .diff()
            .is_some_and(|diff| diff.contains("-    1") && diff.contains("+    2"))
    );
}

#[tokio::test]
async fn edit_reports_a_forgiving_match_and_reindents() {
    let (dir, ws) = setup(&[("a.py", "class A:\n    def f(self):\n        return 1\n")]);
    read_all(&ws, "a.py").await;
    let result = ws
        .call(
            "str_replace",
            json!({"path": "a.py", "old_str": "def f(self):\n    return 1", "new_str": "def f(self):\n    return 2"}),
        )
        .await;
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(
        disk(&dir, "a.py"),
        "class A:\n    def f(self):\n        return 2\n"
    );
    assert!(
        result
            .text
            .contains("matched ignoring indentation; re-indented (+4 spaces)"),
        "{}",
        result.text
    );
}

#[tokio::test]
async fn ambiguous_and_missing_matches_do_not_touch_the_file() {
    let (dir, ws) = setup(&[("a.txt", "x = 1\ny = 2\nx = 1\n")]);
    read_all(&ws, "a.txt").await;
    let ambiguous = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "x = 1", "new_string": "x = 2"}),
        )
        .await;
    assert!(
        ambiguous.is_error
            && ambiguous.text.contains("line 1")
            && ambiguous.text.contains("line 3"),
        "{}",
        ambiguous.text
    );
    let missing = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "z = 9", "new_string": "z = 0"}),
        )
        .await;
    assert!(
        missing.is_error && missing.text.contains("not found"),
        "{}",
        missing.text
    );
    assert_eq!(disk(&dir, "a.txt"), "x = 1\ny = 2\nx = 1\n");

    let all = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "x = 1", "new_string": "x = 2", "replace_all": true}),
        )
        .await;
    assert!(!all.is_error, "{}", all.text);
    assert_eq!(disk(&dir, "a.txt"), "x = 2\ny = 2\nx = 2\n");
}

#[tokio::test]
async fn an_edit_that_changes_nothing_writes_nothing_and_says_so() {
    let (dir, ws) = setup(&[("a.txt", "say \u{201C}hi\u{201D}\n")]);
    read_all(&ws, "a.txt").await;
    let before = std::fs::metadata(dir.path().join("a.txt"))
        .expect("meta")
        .modified()
        .expect("mtime");
    let result = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "say \"hi\"", "new_string": "say \u{201C}hi\u{201D}"}),
        )
        .await;
    assert!(!result.is_error);
    assert!(
        result.text.starts_with("No changes made"),
        "{}",
        result.text
    );
    assert!(result.changes.is_empty());
    let after = std::fs::metadata(dir.path().join("a.txt"))
        .expect("meta")
        .modified()
        .expect("mtime");
    assert_eq!(before, after);
}

#[tokio::test]
async fn an_empty_old_string_creates_a_missing_file_only() {
    let (dir, ws) = setup(&[("a.txt", "x\n")]);
    let created = ws
        .call(
            "edit",
            json!({"file_path": "n/new.txt", "old_string": "", "new_string": "hi\n"}),
        )
        .await;
    assert!(!created.is_error, "{}", created.text);
    assert_eq!(disk(&dir, "n/new.txt"), "hi\n");
    read_all(&ws, "a.txt").await;
    let refused = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "", "new_string": "hi\n"}),
        )
        .await;
    assert!(
        refused.is_error && refused.text.contains("old_string is empty"),
        "{}",
        refused.text
    );
    let missing = ws
        .call(
            "edit",
            json!({"file_path": "nope.txt", "old_string": "a", "new_string": "b"}),
        )
        .await;
    assert!(
        missing.is_error && missing.text.contains("File not found"),
        "{}",
        missing.text
    );
}

#[tokio::test]
async fn multi_edit_rolls_back_when_a_later_edit_fails() {
    let (dir, ws) = setup(&[("a.txt", "one\ntwo\nthree\n")]);
    read_all(&ws, "a.txt").await;
    let failed = ws
        .call(
            "multi_edit",
            json!({"file_path": "a.txt", "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "nonexistent", "new_string": "x"},
            ]}),
        )
        .await;
    assert!(
        failed.is_error && failed.text.contains("Edit 2 of 2"),
        "{}",
        failed.text
    );
    assert_eq!(disk(&dir, "a.txt"), "one\ntwo\nthree\n");

    let ok = ws
        .call(
            "MultiEdit",
            json!({"file_path": "a.txt", "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "1\ntwo", "new_string": "1\n2"},
            ]}),
        )
        .await;
    assert!(!ok.is_error, "{}", ok.text);
    assert_eq!(disk(&dir, "a.txt"), "1\n2\nthree\n");
    assert_eq!(ok.summary.map(|summary| summary.replacements), Some(2));
}

#[tokio::test]
async fn write_keeps_crlf_and_reports_no_change() {
    let (dir, ws) = setup(&[("a.txt", "a\r\nb\r\n")]);
    read_all(&ws, "a.txt").await;
    let same = ws
        .call("write", json!({"file_path": "a.txt", "content": "a\nb\n"}))
        .await;
    assert!(same.text.starts_with("No changes made"), "{}", same.text);
    let changed = ws
        .call("write", json!({"file_path": "a.txt", "content": "a\nc\n"}))
        .await;
    assert!(!changed.is_error, "{}", changed.text);
    assert_eq!(disk(&dir, "a.txt"), "a\r\nc\r\n");
}

#[tokio::test]
async fn unknown_tools_and_bad_arguments_explain_themselves() {
    let (_dir, ws) = setup(&[]);
    let unknown = ws.call("bash", json!({})).await;
    assert!(
        unknown.is_error && unknown.text.contains("Available tools"),
        "{}",
        unknown.text
    );
    let bad = ws.call("edit", json!({"file_path": "a"})).await;
    assert!(
        bad.is_error && bad.text.contains("Invalid arguments"),
        "{}",
        bad.text
    );
}

// ---- apply_patch ----

#[tokio::test]
async fn apply_patch_adds_updates_moves_and_deletes_in_one_go() {
    let (dir, ws) = setup(&[
        ("src/app.py", "def greet():\n    print(\"Hi\")\n"),
        ("old.txt", "bye\n"),
    ]);
    let patch = "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** Update File: src/app.py\n\
        *** Move to: src/main.py\n@@ def greet():\n-    print(\"Hi\")\n+    print(\"Hello, world!\")\n\
        *** Delete File: old.txt\n*** End Patch\n";
    let result = ws.call("apply_patch", json!(patch)).await;
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(disk(&dir, "hello.txt"), "Hello world\n");
    assert_eq!(
        disk(&dir, "src/main.py"),
        "def greet():\n    print(\"Hello, world!\")\n"
    );
    assert!(!dir.path().join("src/app.py").exists());
    assert!(!dir.path().join("old.txt").exists());
    assert!(
        result.text.contains("A hello.txt") && result.text.contains("D old.txt"),
        "{}",
        result.text
    );
    assert_eq!(result.changes.len(), 4);
}

#[tokio::test]
async fn apply_patch_is_all_or_nothing() {
    let (dir, ws) = setup(&[("a.txt", "alpha\nbeta\n"), ("b.txt", "one\n")]);
    let patch = "*** Begin Patch\n*** Update File: a.txt\n-alpha\n+ALPHA\n*** Update File: b.txt\n-missing line\n+x\n*** End Patch\n";
    let result = ws.call("apply_patch", json!({"input": patch})).await;
    assert!(result.is_error, "{}", result.text);
    assert!(
        result.text.contains("b.txt") && result.text.contains("no file was changed"),
        "{}",
        result.text
    );
    assert_eq!(disk(&dir, "a.txt"), "alpha\nbeta\n");
}

#[tokio::test]
async fn apply_patch_refuses_to_add_over_an_existing_file() {
    let (dir, ws) = setup(&[("a.txt", "keep\n")]);
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+replace\n*** End Patch\n";
    let result = ws.call("apply_patch", json!(patch)).await;
    assert!(
        result.is_error && result.text.contains("already exists"),
        "{}",
        result.text
    );
    assert_eq!(disk(&dir, "a.txt"), "keep\n");
}

#[tokio::test]
async fn patched_files_count_as_read_for_later_edits() {
    let (dir, ws) = setup(&[("a.txt", "x\n")]);
    let patch = "*** Begin Patch\n*** Update File: a.txt\n-x\n+y\n*** End Patch\n";
    assert!(!ws.call("apply_patch", json!(patch)).await.is_error);
    let result = ws
        .call(
            "edit",
            json!({"file_path": "a.txt", "old_string": "y", "new_string": "z"}),
        )
        .await;
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(disk(&dir, "a.txt"), "z\n");
}
