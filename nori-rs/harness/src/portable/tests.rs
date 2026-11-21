//! A conversation, moved from one Nori home to another, continues there: the bundle carries
//! it byte for byte, the new home files it under its own project, and it records where it came
//! from and where it went.

use std::path::Path;

use pretty_assertions::assert_eq;

use super::*;
use crate::transcript::TranscriptLoader;
use crate::transcript::TranscriptRecorder;

async fn conversation(home: &Path, cwd: &Path) -> (String, String) {
    let recorder =
        TranscriptRecorder::new(home, cwd, Some("claude-code".to_owned()), "0.1.0", None)
            .await
            .unwrap_or_else(|error| unreachable!("recorder: {error}"));
    recorder
        .record_user_message("m1", "fix the flaky login test", vec![])
        .await
        .unwrap_or_else(|error| unreachable!("record: {error}"));
    recorder
        .shutdown()
        .await
        .unwrap_or_else(|error| unreachable!("shutdown: {error}"));
    (
        recorder.session_id().to_owned(),
        recorder.project_id().to_owned(),
    )
}

#[tokio::test]
async fn a_moved_conversation_is_filed_and_resumable_in_its_new_home() {
    let dirs = tempfile::tempdir().unwrap_or_else(|error| unreachable!("{error}"));
    let (here, there) = (dirs.path().join("here"), dirs.path().join("there"));
    let (here_cwd, there_cwd) = (
        dirs.path().join("proj-here"),
        dirs.path().join("proj-there"),
    );
    for dir in [&here, &there, &here_cwd, &there_cwd] {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| unreachable!("{error}"));
    }
    let (id, here_project) = conversation(&here, &here_cwd).await;

    let bundle = export(&here, &id)
        .await
        .unwrap_or_else(|error| unreachable!("export: {error}"));
    let bytes = bundle.encode();
    let bundle = SessionBundle::decode(&bytes).unwrap_or_else(|error| unreachable!("{error}"));
    let imported = import(&there, &bundle, &there_cwd)
        .await
        .unwrap_or_else(|error| unreachable!("import: {error}"));
    assert_eq!(imported.conversation_id, id);
    assert!(
        imported.project_id != here_project,
        "a different directory is a different project"
    );

    // Loaded there, it belongs to the project it is filed under, so a resume continues its
    // transcript instead of starting another.
    let loader = TranscriptLoader::new(there.clone());
    let transcript = loader
        .load_transcript(&imported.project_id, &id)
        .await
        .unwrap_or_else(|error| unreachable!("load: {error}"));
    assert_eq!(transcript.meta.project_id, imported.project_id);
    assert_eq!(
        loader.session_path(&transcript.meta.project_id, &id),
        imported.transcript
    );
    let records: Vec<String> = transcript
        .records()
        .filter_map(|record| match record {
            crate::transcript::TranscriptRecord::User { content } => Some(content.to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(records, vec!["fix the flaky login test".to_owned()]);

    // The home it left says where it went; one it comes back to forgets that.
    let moved = MovedTo {
        machine: "there".to_owned(),
        ssh: "me@there".to_owned(),
        cwd: there_cwd.display().to_string(),
        launcher: None,
        at: "now".to_owned(),
    };
    record_moved_out(&here, &id, &moved)
        .await
        .unwrap_or_else(|error| unreachable!("record: {error}"));
    assert_eq!(moved_to(&here, &here_project, &id), Some(moved));
    let back = export(&there, &id)
        .await
        .unwrap_or_else(|error| unreachable!("export back: {error}"));
    import(&here, &back, &here_cwd)
        .await
        .unwrap_or_else(|error| unreachable!("import back: {error}"));
    assert_eq!(moved_to(&here, &here_project, &id), None);
}

#[tokio::test]
async fn import_rejects_a_manifest_session_id_that_escapes_its_directory() {
    let dirs = tempfile::tempdir().unwrap();
    let (id, _) = conversation(dirs.path(), dirs.path()).await;
    let mut bundle = export(dirs.path(), &id).await.unwrap();
    bundle.manifest.conversation_id = "../../outside".to_owned();
    assert!(import(dirs.path(), &bundle, dirs.path()).await.is_err());
}
