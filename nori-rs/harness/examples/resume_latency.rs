//! Times each step of resuming a recorded session: reading and decoding the journal, building
//! the transcript, and turning it into the client's replay.
//!
//! `resume_latency <nori-home> <project-id> <session-id>`

use std::time::Instant;

use nori_harness::transcript::TranscriptLoader;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(home), Some(project), Some(session)) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: resume_latency <nori-home> <project-id> <session-id>");
        return Ok(());
    };
    let home = std::path::PathBuf::from(home);
    let loader = TranscriptLoader::new(home.clone());
    let journal = loader
        .session_path(&project, &session)
        .with_extension("pkl");
    let size = std::fs::metadata(&journal)?.len();
    println!("journal {:.1} MB", size as f64 / 1e6);

    let started = Instant::now();
    let bytes = std::fs::read(&journal)?;
    println!("read: {:?}", started.elapsed());
    drop(bytes);

    let started = Instant::now();
    let log = nori_conversation::ConversationLog::open(&journal)?;
    println!(
        "open log: {:?} ({} records, {} visible)",
        started.elapsed(),
        log.len(),
        log.visible_ids().len()
    );
    let started = Instant::now();
    let entries = log.visible_named(nori_conversation::Kind::Operation, "transcript-entry");
    let tail = log
        .records(entries.iter().rev().take(2000).copied())
        .filter(Result::is_ok)
        .count();
    println!("read the newest {tail} entries: {:?}", started.elapsed());
    let started = Instant::now();
    let decoded = log.visible().filter(Result::is_ok).count();
    println!(
        "read all {decoded} visible records: {:?}",
        started.elapsed()
    );
    drop(log);

    let started = Instant::now();
    let transcript = loader.load_transcript(&project, &session).await?;
    println!("load transcript: {:?}", started.elapsed());
    drop(transcript);

    let started = Instant::now();
    let tail = loader
        .load_transcript_tail(
            &project,
            &session,
            nori_harness::transcript::RESUME_TAIL_BYTES,
        )
        .await?;
    println!(
        "load the resume tail: {:?} ({} entries left on disk)",
        started.elapsed(),
        tail.left_out()
    );
    Ok(())
}
