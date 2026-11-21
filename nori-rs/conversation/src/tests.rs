use crate::ConversationLog;
use crate::Event;
use crate::SliceRef;
use pretty_assertions::assert_eq;
use std::fs;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

/// Every record of `log`, hidden branches included, read.
fn all(log: &ConversationLog) -> Vec<crate::Record> {
    log.all()
        .map(|record| record.map(Arc::unwrap_or_clone))
        .collect::<io::Result<_>>()
        .unwrap()
}

/// The visible records of `log`, read.
fn visible(log: &ConversationLog) -> Vec<crate::Record> {
    log.visible()
        .map(|record| record.map(Arc::unwrap_or_clone))
        .collect::<io::Result<_>>()
        .unwrap()
}

fn message(content: &str) -> Event {
    Event::Message {
        role: "user".into(),
        content: content.into(),
    }
}

#[test]
fn literal_journal_parser_handles_structural_formatting_without_evaluating_fields() {
    let expected = message("café 🙂 \\(read(\"env:NEVER_EVALUATED\"))");
    let record = crate::Record {
        id: 1,
        command_id: None,
        event: expected,
    };
    let encoded = crate::codec::encode(&record).unwrap();
    let reformatted = encoded
        .replace(" = new {", "=new Dynamic {/* retained comment */")
        .replace(
            "; data =",
            "; ignored = throw(\"never evaluate replay\");data=",
        );
    assert_eq!(
        crate::codec::decode(reformatted.trim_end()).unwrap(),
        record
    );
}

#[test]
fn journal_replay_rejects_computed_known_payloads_and_preserves_future_syntax() {
    let computed =
        "event_1 = new { kind = \"message\"; data = read(\"file:/nori-no-evaluation\") }";
    assert!(crate::codec::decode(computed).is_err());
    let future = "event_2 = new { kind = \"future-kind\"; data = throw(\"never execute\") }";
    assert_eq!(
        crate::codec::decode(future).unwrap(),
        crate::Record {
            id: 2,
            command_id: None,
            event: Event::Unknown {
                form: future.to_owned()
            },
        }
    );
}

#[test]
fn rewind_keeps_history_and_replay_reconstructs_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.pkl");
    let mut log = ConversationLog::open(&path).unwrap();
    log.append(message("first")).unwrap();
    log.append(message("hidden")).unwrap();
    log.append(Event::Rewind { head: Some(1) }).unwrap();
    log.append(message("replacement\n\"quoted\"")).unwrap();
    let expected = log.projection().clone();
    assert_eq!(expected.visible().to_vec(), vec![1, 4]);
    let replay = ConversationLog::open(&path).unwrap();
    assert_eq!(replay.projection(), &expected);
    assert_eq!(replay.len(), 4);
    assert!(fs::read_to_string(path).unwrap().contains("hidden"));
}

#[test]
fn slices_are_inclusive_and_require_visible_ordered_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = ConversationLog::open(dir.path().join("s.pkl")).unwrap();
    for text in ["one", "two", "three"] {
        log.append(message(text)).unwrap();
    }
    let reference = SliceRef { start: 1, end: 2 };
    assert_eq!(
        log.slice(reference)
            .unwrap()
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(log.slice(SliceRef { start: 2, end: 1 }).unwrap().is_none());
    log.append(Event::Fork { source: reference }).unwrap();
    log.append(Event::Rewind { head: Some(1) }).unwrap();
    assert!(log.slice(reference).unwrap().is_none());
    assert_eq!(
        log.slice_at(3, reference)
            .unwrap()
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(log.append(Event::Fork { source: reference }).is_err());
    assert_eq!(log.len(), 5);
}

#[test]
fn partial_tail_and_unknown_events_are_safe_to_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.pkl");
    fs::write(&path, b"event_1 = new { kind = \"future-op\"; data = \"{}\" }\nevent_2 = new { kind = \"message\"").unwrap();
    let mut log = ConversationLog::open(&path).unwrap();
    assert_eq!(log.len(), 1);
    log.append(message("recovered")).unwrap();
    let replay = ConversationLog::open(&path).unwrap();
    assert_eq!(all(&replay), all(&log));
    assert_eq!(replay.projection(), log.projection());
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "bad record").unwrap();
    assert!(ConversationLog::open(path).is_err());
}

#[test]
fn replay_ignores_unknown_fields_in_known_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.pkl");
    let payload = serde_json::json!({"role": "user", "content": "hello", "future": {"nested": 42}})
        .to_string();
    let encoded = serde_json::to_string(&payload).unwrap();
    fs::write(
        &path,
        format!("event_1 = new {{ kind = \"message\"; data = {encoded} }}\n"),
    )
    .unwrap();
    let log = ConversationLog::open(path).unwrap();
    assert_eq!(visible(&log)[0].event, message("hello"));
}

#[test]
fn replay_preserves_operations_as_data_without_running_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("operations.pkl");
    let effect = dir.path().join("must-not-exist");
    let event = Event::Operation {
        name: "shell".into(),
        args: format!("touch {}", effect.display()),
    };
    let mut log = ConversationLog::open(&path).unwrap();
    log.append(event.clone()).unwrap();

    let reopened = ConversationLog::open(path).unwrap();
    assert_eq!(visible(&reopened)[0].event, event);
    assert!(!effect.exists());
}

#[test]
fn shared_transport_checks_head_and_deduplicates_commands_across_reloads() {
    let store = crate::MemoryStore::default();
    let mut first = ConversationLog::with_store(store.clone()).unwrap();
    let mut second = ConversationLog::with_store(store.clone()).unwrap();
    let original = first
        .append_with_id(None, "client-1", message("one"))
        .unwrap();
    assert_eq!(
        second
            .append_with_id(None, "client-1", message("one"))
            .unwrap(),
        original
    );
    assert_eq!(
        second
            .append_with_id(None, "client-2", message("two"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    let second_record = second
        .append_with_id(Some(1), "client-2", message("two"))
        .unwrap();
    assert_eq!(second_record.id, 2);
    assert!(
        first
            .append_with_id(Some(2), "client-2", message("different"))
            .is_err()
    );
    let reopened = ConversationLog::with_store(store).unwrap();
    assert_eq!(reopened.len(), 2);
    assert_eq!(reopened.projection().head(), Some(2));
}

#[test]
fn file_store_atomically_compares_and_appends_across_instances() {
    use crate::ConversationStore;
    use std::sync::Arc;
    use std::sync::Barrier;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.pkl");
    let barrier = Arc::new(Barrier::new(2));
    let handles = ["first", "second"].map(|content| {
        let path = path.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let mut store = crate::FileStore::new(path);
            let record = crate::Record {
                id: 1,
                command_id: None,
                event: message(content),
            };
            let encoded = crate::codec::encode(&record).unwrap();
            barrier.wait();
            store.append(0, encoded.as_bytes())
        })
    });
    let results = handles.map(|handle| handle.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .map(std::io::Error::kind)
            .collect::<Vec<_>>(),
        vec![std::io::ErrorKind::WouldBlock]
    );
    let replay = ConversationLog::open(path).unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay.projection().head(), Some(1));
}

#[derive(Clone)]
struct RacingStore(Arc<RacingStoreState>);

struct RacingStoreState {
    bytes: Mutex<Vec<u8>>,
    first_appends: AtomicUsize,
    barrier: Barrier,
}

impl RacingStore {
    fn pair() -> (Self, Self) {
        let state = Arc::new(RacingStoreState {
            bytes: Mutex::new(Vec::new()),
            first_appends: AtomicUsize::new(0),
            barrier: Barrier::new(2),
        });
        (Self(state.clone()), Self(state))
    }
}

impl crate::ConversationStore for RacingStore {
    fn read(&self) -> io::Result<Vec<u8>> {
        Ok(self
            .0
            .bytes
            .lock()
            .map_err(|_| io::Error::other("test store lock poisoned"))?
            .clone())
    }

    fn append(&mut self, expected_version: i64, bytes: &[u8]) -> io::Result<()> {
        if self
            .0
            .first_appends
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            < 2
        {
            self.0.barrier.wait();
        }
        let mut current = self
            .0
            .bytes
            .lock()
            .map_err(|_| io::Error::other("test store lock poisoned"))?;
        if i64::try_from(current.len()).map_err(io::Error::other)? != expected_version {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "journal changed"));
        }
        current.extend_from_slice(bytes);
        Ok(())
    }
}

#[test]
fn log_can_refresh_and_retry_after_a_concurrent_compare_and_append_conflict() {
    let (first_store, second_store) = RacingStore::pair();
    let first = ConversationLog::with_store(first_store).unwrap();
    let second = ConversationLog::with_store(second_store).unwrap();
    let handles = [first, second].map(|mut log| {
        std::thread::spawn(move || {
            let result = log.append(message("race"));
            (log, result)
        })
    });
    let mut results = handles.map(|handle| handle.join().unwrap()).into_iter();
    let (first_log, first_result) = results.next().unwrap();
    let (second_log, second_result) = results.next().unwrap();
    let (mut loser, conflict) = match (first_result, second_result) {
        (Ok(_), Err(error)) => (second_log, error),
        (Err(error), Ok(_)) => (first_log, error),
        _ => panic!("exactly one concurrent append must win"),
    };
    assert_eq!(conflict.kind(), io::ErrorKind::WouldBlock);

    let retried = loser
        .append_with_id(Some(1), "retry-after-race", message("retry"))
        .unwrap();
    assert_eq!(retried.id, 2);
    assert_eq!(loser.projection().head(), Some(2));
}

#[test]
fn streamed_appends_do_not_reload_the_journal() {
    struct CountingStore {
        store: crate::MemoryStore,
        reads: Arc<AtomicUsize>,
    }
    impl crate::ConversationStore for CountingStore {
        fn read(&self) -> io::Result<Vec<u8>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.store.read()
        }
        fn append(&mut self, expected_version: i64, bytes: &[u8]) -> io::Result<()> {
            self.store.append(expected_version, bytes)
        }
    }
    let reads = Arc::new(AtomicUsize::new(0));
    let store = crate::MemoryStore::default();
    let mut log = ConversationLog::with_store(CountingStore {
        store: store.clone(),
        reads: reads.clone(),
    })
    .unwrap();
    for index in 0..512 {
        log.append(message(&format!("stream chunk {index}")))
            .unwrap();
    }
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    let replay = ConversationLog::with_store(store).unwrap();
    assert_eq!(all(&replay), all(&log));
    assert_eq!(replay.projection(), log.projection());
}

#[test]
fn contention_with_a_rewound_head_refreshes_ids_before_append() {
    let store = crate::MemoryStore::default();
    let mut first = ConversationLog::with_store(store.clone()).unwrap();
    first.append(message("root")).unwrap();
    let mut stale = ConversationLog::with_store(store.clone()).unwrap();
    first.append(message("hidden")).unwrap();
    first.append(Event::Rewind { head: Some(1) }).unwrap();
    let record = stale
        .append_with_id(Some(1), "after-rewind", message("replacement"))
        .unwrap();
    assert_eq!(record.id, 4);
    let replay = ConversationLog::with_store(store).unwrap();
    assert_eq!(all(&stale), all(&replay));
    assert_eq!(stale.projection(), replay.projection());
}

#[test]
fn invalid_projection_change_does_not_write_or_mutate_the_branch() {
    let store = crate::MemoryStore::default();
    let mut log = ConversationLog::with_store(store.clone()).unwrap();
    log.append(message("root")).unwrap();
    let expected = log.projection().clone();
    assert_eq!(
        log.append(Event::Rewind { head: Some(99) })
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(log.projection(), &expected);
    let replay = ConversationLog::with_store(store).unwrap();
    assert_eq!(all(&log), all(&replay));
    assert_eq!(log.append(message("next")).unwrap().id, 2);
}

#[test]
fn incomplete_append_requires_recovery_without_advancing_projection() {
    struct InterruptedStore {
        store: crate::MemoryStore,
        interrupt: bool,
    }
    impl crate::ConversationStore for InterruptedStore {
        fn read(&self) -> io::Result<Vec<u8>> {
            self.store.read()
        }
        fn append(&mut self, version: i64, bytes: &[u8]) -> io::Result<()> {
            if self.interrupt {
                self.interrupt = false;
                self.store.append(version, &bytes[..bytes.len() / 2])?;
                return Err(io::Error::other("interrupted durable append"));
            }
            self.store.append(version, bytes)
        }
    }
    let store = crate::MemoryStore::default();
    let mut log = ConversationLog::with_store(InterruptedStore {
        store: store.clone(),
        interrupt: true,
    })
    .unwrap();
    assert!(log.append(message("partial")).is_err());
    assert!(log.is_empty());
    assert_eq!(log.projection().head(), None);
    assert!(log.append(message("must wait for recovery")).is_err());
    log.refresh().unwrap();
    assert_eq!(log.append(message("recovered")).unwrap().id, 1);
    let replay = ConversationLog::with_store(store).unwrap();
    assert_eq!(all(&replay), all(&log));
    assert_eq!(replay.projection(), log.projection());
}

#[test]
fn the_visible_branch_shares_the_records_of_the_log() {
    let mut log = ConversationLog::with_store(crate::MemoryStore::default()).unwrap();
    let appended = log.append(message("held once")).unwrap();
    // One allocation: what the log appended is what it reads back.
    assert!(Arc::ptr_eq(&appended, &log.record(1).unwrap()));
    assert!(Arc::ptr_eq(
        &appended,
        &log.visible().next().unwrap().unwrap()
    ));
}

#[test]
fn a_reopened_log_knows_kinds_and_names_without_reading_and_reads_records_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.pkl");
    let mut log = ConversationLog::open(&path).unwrap();
    log.append(message("one")).unwrap();
    log.append(Event::Command {
        name: "backend-bound".into(),
        payload: "{\"agent\":\"codex\"}".into(),
    })
    .unwrap();
    log.append_with_id(
        Some(2),
        "turn-1",
        Event::Operation {
            name: "transcript-entry".into(),
            args: "{\"ts\":1,\"nested\":\"a \\\"quoted\\\" word\"}".into(),
        },
    )
    .unwrap();
    log.append(Event::Rewind { head: Some(2) }).unwrap();
    log.append(message("after the rewind ☾")).unwrap();

    let reopened = ConversationLog::open(&path).unwrap();
    assert_eq!(
        (
            reopened.len(),
            reopened.visible_ids().to_vec(),
            reopened.kind(2),
            reopened.name(2),
            reopened.name(3),
            reopened.visible_named(crate::Kind::Command, "backend-bound"),
            reopened.visible_named(crate::Kind::Operation, "transcript-entry"),
        ),
        (
            5,
            vec![1, 2, 5],
            Some(crate::Kind::Command),
            Some("backend-bound"),
            Some("transcript-entry"),
            vec![2],
            Vec::new(),
        )
    );
    assert_eq!(all(&reopened), all(&log));
    // Read backwards too, as a resume reads a conversation's last turns.
    let newest_first = reopened
        .records(reopened.visible_ids().iter().rev().copied())
        .map(|record| record.unwrap().id)
        .collect::<Vec<_>>();
    assert_eq!(newest_first, vec![5, 2, 1]);
    // The command id is known again, so a retry returns the recorded record.
    let mut reopened = reopened;
    assert_eq!(
        reopened
            .append_with_id(
                Some(5),
                "turn-1",
                Event::Operation {
                    name: "transcript-entry".into(),
                    args: "{\"ts\":1,\"nested\":\"a \\\"quoted\\\" word\"}".into(),
                },
            )
            .unwrap()
            .id,
        3
    );
}

#[test]
fn names_are_found_in_either_key_order() {
    for (data, name) in [
        (r#"{"name":"first","args":"x"}"#, "first"),
        (r#"{"args":"x \"name\" y","name":"last"}"#, "last"),
    ] {
        let line = format!(
            "event_1 = new {{ kind = \"operation\"; data = {} }}",
            serde_json::to_string(data).unwrap()
        );
        assert_eq!(
            crate::codec::locate(&line).and_then(|located| located.name),
            Some(name)
        );
    }
}

#[test]
fn a_scan_from_where_the_last_one_stopped_reads_only_what_was_appended() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.pkl");
    let mut log = ConversationLog::open(&path).unwrap();
    log.append(message("one")).unwrap();
    log.append(message("two")).unwrap();
    let mut seen = Vec::new();
    let offset = ConversationLog::scan_from(&path, 0, |record| {
        seen.push(record.id);
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(
        (seen.clone(), offset),
        (
            vec![1, 2],
            i64::try_from(fs::metadata(&path).unwrap().len()).unwrap()
        )
    );

    log.append(message("three")).unwrap();
    // A record still being written is left for the next scan.
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"event_4 = new { kind")
        .unwrap();
    seen.clear();
    let next = ConversationLog::scan_from(&path, offset, |record| {
        seen.push(record.id);
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(seen, vec![3]);
    assert_eq!(
        ConversationLog::scan_from(&path, next, |_| std::ops::ControlFlow::Continue(())).unwrap(),
        next
    );
}

#[test]
fn a_saved_index_is_used_while_it_matches_the_journal_and_ignored_after() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.pkl");
    let big = "x".repeat(300 * 1024);
    let mut log = ConversationLog::open(&path).unwrap();
    for n in 0..4 {
        log.append(message(&format!("{n} {big}"))).unwrap();
    }
    log.append(Event::Rewind { head: Some(2) }).unwrap();
    log.append_with_id(Some(2), "after", message("after"))
        .unwrap();
    drop(log);

    // The first open indexes more than a megabyte itself and saves the index beside the log.
    let first = ConversationLog::open(&path).unwrap();
    let saved = dir.path().join("long.pkx");
    assert!(saved.exists());
    let mut reopened = ConversationLog::open(&path).unwrap();
    assert_eq!(
        (reopened.visible_ids().to_vec(), all(&reopened)),
        (first.visible_ids().to_vec(), all(&first))
    );
    // What is appended after the saved index is read on top of it.
    reopened.append(message("newer")).unwrap();
    let again = ConversationLog::open(&path).unwrap();
    assert_eq!(again.visible_ids().to_vec(), vec![1, 2, 6, 7]);
    assert_eq!(all(&again), all(&reopened));

    // A journal that no longer has the bytes the index covered is indexed whole.
    fs::write(
        &path,
        crate::codec::encode(&crate::Record {
            id: 1,
            command_id: None,
            event: message("other"),
        })
        .unwrap(),
    )
    .unwrap();
    let replaced = ConversationLog::open(&path).unwrap();
    assert_eq!(
        visible(&replaced),
        vec![crate::Record {
            id: 1,
            command_id: None,
            event: message("other")
        }]
    );
}
