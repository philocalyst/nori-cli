//! Indexes the transcripts of one `NORI_HOME` into a scratch directory and reports what the
//! index holds and how searches fare. Read-only on the transcripts.
//!
//! `index_probe <home-with-transcripts> <scratch-dir> [query...]`

use std::time::Instant;

use nori_harness::session_index::SessionIndex;
use nori_harness::session_index::SessionSearch;
use nori_harness::session_index::TurnRole;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(source), Some(scratch)) = (args.next(), args.next()) else {
        eprintln!("usage: index_probe <home-with-transcripts> <scratch-dir> [query...]");
        return Ok(());
    };
    let (source, scratch) = (
        std::path::PathBuf::from(source),
        std::path::PathBuf::from(scratch),
    );
    std::fs::create_dir_all(&scratch)?;
    let link = scratch.join("transcripts");
    if !link.exists() {
        std::os::unix::fs::symlink(source.join("transcripts"), &link)?;
    }
    let index = SessionIndex::open(&scratch);
    let started = Instant::now();
    index.sync_all(&[], |_| {}).await?;
    println!("sync: {:?}", started.elapsed());

    let headers = index.headers();
    let mut turns = [0i64; 3];
    let mut with_output = 0;
    let mut with_command = 0;
    for header in &headers {
        if let Some(digest) = index.digest(&header.project_id, &header.session_id) {
            for turn in &digest.turns {
                turns[match turn.role {
                    TurnRole::User => 0,
                    TurnRole::Assistant => 1,
                    TurnRole::Tool => 2,
                }] += 1;
                with_output += i32::from(turn.output.is_some());
                with_command += i32::from(turn.command.is_some());
            }
        }
    }
    println!(
        "sessions {} | with model {} | with branch {} | with title {} | turns user {} assistant {} tool {} | tool commands {} | tool outputs {}",
        headers.len(),
        headers.iter().filter(|h| h.model.is_some()).count(),
        headers.iter().filter(|h| h.branch.is_some()).count(),
        headers.iter().filter(|h| h.title.is_some()).count(),
        turns[0],
        turns[1],
        turns[2],
        with_command,
        with_output,
    );
    let bytes: u64 = walk(&scratch.join("index"));
    println!("index size: {} KiB", bytes / 1024);
    for query in args {
        let started = Instant::now();
        let hits = index.search(&SessionSearch {
            terms: query.split(' ').map(str::to_owned).collect(),
            scope: None,
        });
        let elapsed = started.elapsed();
        let best = hits.iter().max_by_key(|hit| hit.score);
        println!(
            "query {query:?}: {} sessions in {elapsed:?}; best score {:?} turns {:?} snippet {:?}",
            hits.len(),
            best.map(|hit| hit.score),
            best.map(|hit| hit.turns_matched),
            best.map(|hit| hit.snippet.chars().take(70).collect::<String>()),
        );
    }
    Ok(())
}

fn walk(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                walk(&path)
            } else {
                entry.metadata().map_or(0, |meta| meta.len())
            }
        })
        .sum()
}
