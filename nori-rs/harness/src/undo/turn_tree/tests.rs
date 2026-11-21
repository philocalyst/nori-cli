use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::UndoEvent;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use crate::backend::BackendEvent;
use crate::normalized::Artifact;
use crate::normalized::FileChange;
use crate::normalized::ToolKind;
use crate::normalized::ToolPhase;
use crate::normalized::ToolSnapshot;
use crate::undo::EditJournal;
use crate::undo::UndoSnapshot;
use crate::undo::handle_undo;
use crate::undo::handle_undo_to;

struct Project {
    _root: tempfile::TempDir,
    dir: PathBuf,
    home: PathBuf,
}

impl Project {
    fn new() -> Option<Self> {
        let git = Command::new("git").arg("--version").output();
        if !git.is_ok_and(|output| output.status.success()) {
            eprintln!("git is not installed; skipping");
            return None;
        }
        let root = tempfile::tempdir().unwrap();
        let real_root = root.path().canonicalize().unwrap();
        let dir = real_root.join("project");
        std::fs::create_dir_all(&dir).unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&dir)
            .output()
            .unwrap();
        Some(Self {
            home: real_root.join("home"),
            _root: root,
            dir,
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
}

fn snapshot(kind: ToolKind, artifacts: Vec<Artifact>) -> ToolSnapshot {
    ToolSnapshot {
        call_id: "call".to_string(),
        title: "tool".to_string(),
        kind,
        phase: ToolPhase::Completed,
        locations: Vec::new(),
        invocation: None,
        artifacts,
        raw_input: None,
        raw_output: None,
        owner_request_id: None,
    }
}

fn command() -> ToolSnapshot {
    snapshot(ToolKind::Execute, Vec::new())
}

fn edit(path: &Path, old: &str, new: &str) -> ToolSnapshot {
    snapshot(
        ToolKind::Edit,
        vec![Artifact::Diff(FileChange {
            path: path.to_path_buf(),
            old_text: Some(old.to_string()),
            new_text: new.to_string(),
        })],
    )
}

/// The outcome of an undo: whether it succeeded and what it told the user.
async fn undo_result(events: &mut mpsc::Receiver<BackendEvent>) -> (bool, String) {
    while let Some(event) = events.recv().await {
        if let BackendEvent::Public(SessionEvent::Nori(NoriEvent::Undo(UndoEvent::Completed {
            success,
            message,
            ..
        }))) = event
        {
            return (success, message.unwrap_or_default());
        }
    }
    panic!("undo never completed");
}

#[tokio::test]
async fn a_turn_that_ran_rm_and_sed_through_a_command_is_fully_reverted() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("notes.txt", "hello old world\n");
    project.write("old/data.txt", "data");
    let (tx, mut events) = mpsc::channel(32);
    let journal = Arc::new(EditJournal::for_cwd(
        &project.dir,
        &project.home,
        tx.clone(),
    ));

    journal.begin_turn("tidy up".to_string());
    let turn = journal.latest_turn_id().unwrap();
    journal.capture_turn_start(turn).await;
    journal.record(&command(), &project.dir, None);
    // `sed -i 's/old/new/' notes.txt && rm -r old && touch made.txt`: no diff is reported.
    project.write("notes.txt", "hello new world\n");
    std::fs::remove_dir_all(project.dir.join("old")).unwrap();
    project.write("made.txt", "");
    journal.capture_turn_end().await.unwrap();

    assert_eq!(
        journal.list(),
        vec![UndoSnapshot {
            index: 0,
            short_id: "3 files".to_string(),
            label: "tidy up".to_string(),
        }]
    );
    handle_undo(&tx, "undo-1", &journal).await;
    let (success, message) = undo_result(&mut events).await;

    assert!(success, "{message}");
    assert!(message.contains("3 from the turn snapshot"), "{message}");
    assert!(!message.contains("not undone"), "{message}");
    assert_eq!(
        project.read("notes.txt").as_deref(),
        Some("hello old world\n")
    );
    assert_eq!(project.read("old/data.txt").as_deref(), Some("data"));
    assert_eq!(project.read("made.txt"), None);
    assert_eq!(journal.list(), Vec::new());
}

#[tokio::test]
async fn a_turn_that_changed_nothing_is_not_offered() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "one");
    let (tx, _events) = mpsc::channel(32);
    let journal = Arc::new(EditJournal::for_cwd(&project.dir, &project.home, tx));

    journal.begin_turn("just talking".to_string());
    let turn = journal.latest_turn_id().unwrap();
    journal.capture_turn_start(turn).await;
    // The agent ran a command that left the project as it was.
    journal.record(&command(), &project.dir, None);
    journal.capture_turn_end().await.unwrap();

    assert_eq!(journal.list(), Vec::new());
}

#[tokio::test]
async fn turns_without_a_snapshot_fall_back_to_the_diff_journal() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write("a.txt", "v0");
    project.write("b.txt", "b0");
    let (tx, mut events) = mpsc::channel(32);
    let journal = Arc::new(EditJournal::for_cwd(
        &project.dir,
        &project.home,
        tx.clone(),
    ));

    // Turn one: an edit reported as a diff; its snapshot "failed", so it was never captured.
    journal.begin_turn("edit a".to_string());
    journal.record(
        &edit(&project.dir.join("a.txt"), "v0", "v1"),
        &project.dir,
        None,
    );
    project.write("a.txt", "v1");
    // Turn two: a command, snapshotted.
    journal.begin_turn("shell b".to_string());
    let turn = journal.latest_turn_id().unwrap();
    journal.capture_turn_start(turn).await;
    journal.record(&command(), &project.dir, None);
    project.write("b.txt", "b1");
    journal.capture_turn_end().await.unwrap();

    handle_undo_to(&tx, "undo-1", &journal, 1).await;
    let (success, message) = undo_result(&mut events).await;

    assert!(success, "{message}");
    assert!(
        message.contains("1 from the turn snapshot, 1 from the edit journal"),
        "{message}"
    );
    assert_eq!(project.read("a.txt").as_deref(), Some("v0"));
    assert_eq!(project.read("b.txt").as_deref(), Some("b0"));
}

#[tokio::test]
async fn a_directory_that_is_not_a_git_work_tree_keeps_the_diff_journal_only() {
    let root = tempfile::tempdir().unwrap();
    let (tx, _events) = mpsc::channel(32);
    let journal = Arc::new(EditJournal::for_cwd(
        root.path(),
        &root.path().join("home"),
        tx,
    ));

    journal.begin_turn("run".to_string());
    let turn = journal.latest_turn_id().unwrap();
    journal.capture_turn_start(turn).await;
    journal.record(&command(), root.path(), None);
    journal.capture_turn_end().await.unwrap();

    // No snapshot, so the old rule: a turn that ran commands is listed.
    assert_eq!(
        journal.list(),
        vec![UndoSnapshot {
            index: 0,
            short_id: "commands only".to_string(),
            label: "run".to_string(),
        }]
    );
}
