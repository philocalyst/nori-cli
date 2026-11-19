//! Lines appended to a file, as `tail -F` sees them: a rotated or truncated file is followed from
//! its start, and a removed one is waited for.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncSeekExt as _;
use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::monitor::event::Ending;

const POLL: Duration = Duration::from_millis(250);
/// The most read in one pass.
const MAX_READ: i64 = 1024 * 1024;
/// An unfinished line longer than this is cut and sent.
const MAX_PENDING: usize = 64 * 1024;

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn identity(_metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

pub(super) async fn run(path: PathBuf, from_start: bool, tx: &Sender<Item>) -> Option<Ending> {
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) => {
            return Some(Ending::Failed {
                reason: format!("{} cannot be read: {error}", path.display()),
            });
        }
    };
    let mut position = if from_start {
        0
    } else {
        i64::try_from(metadata.len()).unwrap_or(i64::MAX)
    };
    let mut file_id = identity(&metadata);
    let mut pending: Vec<u8> = Vec::new();
    let mut missing = false;
    loop {
        tokio::time::sleep(POLL).await;
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(_) => {
                if !missing {
                    missing = true;
                    let note = format!(
                        "{} was removed; waiting for it to come back",
                        path.display()
                    );
                    if tx
                        .send(Item::Line {
                            text: note,
                            failure: false,
                        })
                        .await
                        .is_err()
                    {
                        return None;
                    }
                }
                continue;
            }
        };
        let length = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        let replaced = identity(&metadata) != file_id;
        if missing || replaced || length < position {
            // Rotated, recreated or truncated: what is there now is new.
            missing = false;
            file_id = identity(&metadata);
            position = 0;
            pending.clear();
        }
        if length <= position {
            continue;
        }
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(_) => continue,
        };
        let wanted = (length - position).min(MAX_READ);
        let mut buffer = vec![0; usize::try_from(wanted).unwrap_or(0)];
        let seek = file
            .seek(std::io::SeekFrom::Start(
                u64::try_from(position).unwrap_or(0),
            ))
            .await;
        if seek.is_err() {
            continue;
        }
        let Ok(read) = file.read(&mut buffer).await else {
            continue;
        };
        buffer.truncate(read);
        position += i64::try_from(read).unwrap_or(0);
        pending.extend_from_slice(&buffer);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1])
                .trim_end_matches('\r')
                .to_owned();
            if tx
                .send(Item::Line {
                    text,
                    failure: false,
                })
                .await
                .is_err()
            {
                return None;
            }
        }
        if pending.len() > MAX_PENDING {
            let text = String::from_utf8_lossy(&std::mem::take(&mut pending)).into_owned();
            if tx
                .send(Item::Line {
                    text,
                    failure: false,
                })
                .await
                .is_err()
            {
                return None;
            }
        }
    }
}
