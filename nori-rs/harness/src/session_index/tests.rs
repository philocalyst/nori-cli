use pretty_assertions::assert_eq;

use super::*;
use crate::transcript::TranscriptRecorder;

/// The user turns of a session's digest.
fn asked(index: &SessionIndex, recorder: &TranscriptRecorder) -> Vec<String> {
    index
        .digest(recorder.project_id(), recorder.session_id())
        .unwrap()
        .turns
        .into_iter()
        .filter(|turn| turn.role == TurnRole::User)
        .map(|turn| turn.text)
        .collect()
}

#[tokio::test]
async fn a_grown_session_is_digested_from_where_its_last_digest_stopped() {
    let home = tempfile::tempdir().unwrap();
    let recorder = TranscriptRecorder::new(home.path(), home.path(), None, "0.1.0", None)
        .await
        .unwrap();
    recorder
        .record_user_message("m1", "first question", vec![])
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    let index = SessionIndex::open(home.path());
    assert!(
        index
            .sync_session(recorder.project_id(), recorder.session_id())
            .await
            .unwrap()
    );
    assert_eq!(asked(&index, &recorder), vec!["first question".to_owned()]);
    // Nothing appended: nothing is read.
    assert!(
        !index
            .sync_session(recorder.project_id(), recorder.session_id())
            .await
            .unwrap()
    );

    // Mark the saved reading position's state: a digest that continues from it keeps the mark,
    // one that read the session again from its start would lose it.
    let cursor_path = digest::cursor_path(
        &index.inner.root,
        recorder.project_id(),
        recorder.session_id(),
    );
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
    saved["extractor"]["tail"][0]["text"] = "first question (from the cursor)".into();
    std::fs::write(&cursor_path, serde_json::to_vec(&saved).unwrap()).unwrap();

    recorder
        .record_user_message("m2", "second question", vec![])
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    assert!(
        index
            .sync_session(recorder.project_id(), recorder.session_id())
            .await
            .unwrap()
    );
    assert_eq!(
        asked(&index, &recorder),
        vec!["first question (from the cursor)\nsecond question".to_owned()]
    );
}

#[tokio::test]
async fn a_rewind_digests_the_visible_conversation_again() {
    let home = tempfile::tempdir().unwrap();
    let recorder = TranscriptRecorder::new(home.path(), home.path(), None, "0.1.0", None)
        .await
        .unwrap();
    recorder
        .record_user_message("m1", "kept", vec![])
        .await
        .unwrap();
    let head = recorder.conversation_head().await;
    recorder
        .record_backend_bound(serde_json::json!({"agent": "codex"}).to_string())
        .await
        .unwrap();
    recorder
        .record_user_message("m2", "rewound away", vec![])
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    let index = SessionIndex::open(home.path());
    index
        .sync_session(recorder.project_id(), recorder.session_id())
        .await
        .unwrap();
    assert_eq!(
        asked(&index, &recorder),
        vec!["kept\nrewound away".to_owned()]
    );

    recorder.rewind_head(head).await.unwrap();
    recorder
        .record_user_message("m3", "instead", vec![])
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    index
        .sync_session(recorder.project_id(), recorder.session_id())
        .await
        .unwrap();
    assert_eq!(asked(&index, &recorder), vec!["kept\ninstead".to_owned()]);
    let header = index
        .headers()
        .into_iter()
        .find(|header| header.session_id == recorder.session_id())
        .unwrap();
    // The binding was rewound away with the turn after it.
    assert_eq!(header.agent, None);
}

/// Text of `bytes` bytes made of words that are different from each other, so that it has many
/// distinct trigrams, and that begins with `mark`.
fn varied(mark: &str, bytes: usize, seed: u32) -> String {
    let mut state = seed;
    let mut text = format!("{mark} ");
    while text.len() < bytes {
        for _ in 0..6 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            text.push(char::from(b'a' + u8::try_from((state >> 24) % 26).unwrap()));
        }
        text.push(' ');
    }
    text
}

fn files(index: &SessionIndex, recorder: &TranscriptRecorder) -> (Vec<u8>, Vec<u8>) {
    let root = &index.inner.root;
    let (project, session) = (recorder.project_id(), recorder.session_id());
    (
        std::fs::read(digest::digest_path(root, project, session)).unwrap(),
        std::fs::read(digest::bloom_path(root, project, session)).unwrap(),
    )
}

fn found(index: &SessionIndex, word: &str) -> usize {
    index
        .search(&SessionSearch {
            terms: vec![word.to_owned()],
            scope: None,
        })
        .len()
}

#[tokio::test]
async fn an_update_only_appends_to_the_turns_and_patches_the_filter() {
    let home = tempfile::tempdir().unwrap();
    let recorder = TranscriptRecorder::new(home.path(), home.path(), None, "0.1.0", None)
        .await
        .unwrap();
    let index = SessionIndex::open(home.path());
    // Long messages: the turns they merge into stay at most 64 KiB, so some are final.
    for (i, mark) in ["alphaword", "bravoword", "charlieword", "deltaword"]
        .iter()
        .enumerate()
    {
        recorder
            .record_user_message(
                &format!("m{i}"),
                &varied(mark, 40_000, 7 + i as u32),
                vec![],
            )
            .await
            .unwrap();
    }
    recorder.flush().await.unwrap();
    index
        .sync_session(recorder.project_id(), recorder.session_id())
        .await
        .unwrap();
    let (turns_before, filter_before) = files(&index, &recorder);
    assert!(!turns_before.is_empty());
    assert_eq!(found(&index, "bravoword"), 1);
    assert_eq!(found(&index, "echoword"), 0);

    recorder
        .record_user_message("m4", &varied("echoword", 100_000, 99), vec![])
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    assert!(
        index
            .sync_session(recorder.project_id(), recorder.session_id())
            .await
            .unwrap()
    );
    let (turns_after, filter_after) = files(&index, &recorder);
    // The turns file grew at its end; what was there is untouched.
    assert!(turns_after.len() > turns_before.len());
    assert_eq!(&turns_after[..turns_before.len()], &turns_before[..]);
    // The filter is the same size with only more bits set.
    assert_eq!(filter_after.len(), filter_before.len());
    assert!(
        filter_before
            .iter()
            .zip(&filter_after)
            .all(|(before, after)| before & after == *before)
    );
    assert_eq!(found(&index, "echoword"), 1);
    assert_eq!(found(&index, "alphaword"), 1);

    // Reading the session from its start gives the digest the updates made.
    let incremental = index
        .digest(recorder.project_id(), recorder.session_id())
        .unwrap();
    std::fs::remove_file(digest::cursor_path(
        &index.inner.root,
        recorder.project_id(),
        recorder.session_id(),
    ))
    .unwrap();
    recorder
        .record_user_message("m5", "and a last word", vec![])
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    index
        .sync_session(recorder.project_id(), recorder.session_id())
        .await
        .unwrap();
    let from_start = index
        .digest(recorder.project_id(), recorder.session_id())
        .unwrap();
    // The last turn was past the length turns merge up to, so the new message is a turn of its own.
    let mut expected = incremental.turns.clone();
    expected.push(DigestTurn {
        role: TurnRole::User,
        text: "and a last word".to_owned(),
        command: None,
        output: None,
    });
    assert_eq!(from_start.turns, expected);
    assert_eq!(
        from_start.header.user_turns,
        incremental.header.user_turns + 1
    );
}

#[tokio::test]
async fn a_filter_that_fills_up_is_built_again_larger() {
    let home = tempfile::tempdir().unwrap();
    let recorder = TranscriptRecorder::new(home.path(), home.path(), None, "0.1.0", None)
        .await
        .unwrap();
    let index = SessionIndex::open(home.path());
    recorder
        .record_user_message("m0", &varied("firstword", 200, 3), vec![])
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    index
        .sync_session(recorder.project_id(), recorder.session_id())
        .await
        .unwrap();
    let (_, small) = files(&index, &recorder);
    for i in 1..=5 {
        recorder
            .record_user_message(
                &format!("m{i}"),
                &varied(&format!("markword{i}"), 30_000, 11 * i as u32),
                vec![],
            )
            .await
            .unwrap();
        recorder.flush().await.unwrap();
        index
            .sync_session(recorder.project_id(), recorder.session_id())
            .await
            .unwrap();
    }
    let (_, large) = files(&index, &recorder);
    assert!(large.len() > small.len());
    // Nothing said earlier was lost on the way, in the loaded filter or in the file.
    for word in ["firstword", "markword1", "markword3", "markword5"] {
        assert_eq!(found(&index, word), 1, "{word}");
    }
    let reopened = Bloom::decode(&large).unwrap();
    assert!(reopened.may_contain("markword2"));
}
