//! Times what a picker and a settings change cost on the event loop, on a large synthetic session
//! in a scratch Nori home: the session index's digest of a session from its first byte (what a
//! picker paid for the session someone is in, whenever it had grown), bringing the digest up to
//! date after a turn's worth of records (which reads and writes only what was appended: the
//! digest's turns are appended to, the filter patched, a small cursor and header replaced),
//! listing a project's sessions (exact metadata, which reads
//! every record for the bindings, against each session's first record), and a config write.
//!
//! `index_latency <scratch-dir> [records] [record-bytes]`

use std::time::Duration;
use std::time::Instant;

use nori_harness::session_index::SessionIndex;
use nori_harness::transcript::TranscriptLoader;
use nori_harness::transcript::TranscriptRecorder;

fn ms(elapsed: Duration) -> String {
    format!("{:.2} ms", elapsed.as_secs_f64() * 1000.0)
}

/// The sizes of a session's index files: turns, filter, cursor, header.
fn index_files(root: &std::path::Path, session: &str) -> String {
    ["nidx", "tri", "nst", "nhd"]
        .iter()
        .map(|extension| {
            let len = std::fs::metadata(root.join(format!("{session}.{extension}")))
                .map_or(0, |metadata| metadata.len());
            format!("{extension} {len}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(home) = args.next().map(std::path::PathBuf::from) else {
        eprintln!("usage: index_latency <scratch-dir> [records] [record-bytes]");
        return Ok(());
    };
    let records: i64 = args.next().and_then(|n| n.parse().ok()).unwrap_or(20_000);
    let record_bytes: i64 = args.next().and_then(|n| n.parse().ok()).unwrap_or(2_000);
    std::fs::create_dir_all(&home)?;
    let filler =
        "lorem ipsum dolor sit amet ".repeat(usize::try_from(record_bytes / 27 + 1).unwrap_or(1));

    let recorder =
        TranscriptRecorder::new(&home, &home, Some("codex".to_owned()), "0.0.0", None).await?;
    let started = Instant::now();
    for i in 0..records {
        recorder
            .record_user_message(&format!("m{i}"), &format!("{i} {filler}"), Vec::new())
            .await?;
    }
    recorder.flush().await?;
    let journal = recorder.transcript_path().with_extension("pkl");
    let size = std::fs::metadata(&journal)?.len();
    println!(
        "session: {records} records, journal {:.1} MB, recorded in {}",
        size as f64 / 1e6,
        ms(started.elapsed())
    );
    let (project, session) = (
        recorder.project_id().to_owned(),
        recorder.session_id().to_owned(),
    );
    let index = SessionIndex::open(&home);
    let root = home.join("index").join("v1").join(&project);

    let started = Instant::now();
    index.sync_session(&project, &session).await?;
    println!(
        "digest from the first byte (every picker open of a grown session, before): {}",
        ms(started.elapsed())
    );

    let started = Instant::now();
    index.sync_session(&project, &session).await?;
    println!(
        "digest already current (nothing appended):                            {}",
        ms(started.elapsed())
    );

    for i in 0..50 {
        recorder
            .record_user_message(&format!("t{i}"), &format!("turn {i} {filler}"), Vec::new())
            .await?;
    }
    recorder.flush().await?;
    let before_files = index_files(&root, &session);
    let started = Instant::now();
    index.sync_session(&project, &session).await?;
    println!(
        "digest after 50 appended records (after: reads and writes only those): {}",
        ms(started.elapsed())
    );
    println!(
        "  index files before -> after: {before_files} -> {}",
        index_files(&root, &session)
    );
    recorder
        .record_user_message("t-one", &format!("one more {filler}"), Vec::new())
        .await?;
    recorder.flush().await?;
    let started = Instant::now();
    index.sync_session(&project, &session).await?;
    println!(
        "digest after 1 appended record:                                       {}",
        ms(started.elapsed())
    );
    // The cursor gone, the session is read from its start again: what it cost before cursors.
    std::fs::remove_file(root.join(format!("{session}.nst")))?;
    for i in 0..50 {
        recorder
            .record_user_message(&format!("u{i}"), &format!("turn {i} {filler}"), Vec::new())
            .await?;
    }
    recorder.flush().await?;
    let started = Instant::now();
    index.sync_session(&project, &session).await?;
    println!(
        "digest after 50 appended records without a cursor (before):           {}",
        ms(started.elapsed())
    );

    let loader = TranscriptLoader::new(home.clone());
    let started = Instant::now();
    let exact = loader.list_session_metadata(&project).await?;
    println!(
        "list sessions, exact metadata (picker and index enumeration, before):  {} ({} sessions)",
        ms(started.elapsed()),
        exact.len()
    );
    let started = Instant::now();
    let heads = loader.list_session_heads(Some(&home)).await?;
    println!(
        "list sessions, first records (after):                                 {} ({} sessions)",
        ms(started.elapsed()),
        heads.len()
    );

    let started = Instant::now();
    nori_config::NoriConfigEdits::new(&home)
        .set_path(&["tui", "vim_mode"], "insert")
        .apply_blocking()
        .map_err(std::io::Error::other)?;
    println!(
        "config.toml write on the caller's thread (settings change, before):   {}",
        ms(started.elapsed())
    );
    let started = Instant::now();
    nori_config::NoriConfigEdits::new(&home)
        .set_path(&["tui", "vim_mode"], "normal")
        .apply_in_background();
    println!(
        "config.toml write handed to the config writer (after):                {}",
        ms(started.elapsed())
    );
    let started = Instant::now();
    nori_config::NoriConfigEdits::new(&home)
        .set_path(&["tui", "vim_mode"], "insert")
        .apply()
        .await
        .map_err(std::io::Error::other)?;
    println!(
        "config.toml write awaited on the config writer (off the caller's thread): {}",
        ms(started.elapsed())
    );
    Ok(())
}
