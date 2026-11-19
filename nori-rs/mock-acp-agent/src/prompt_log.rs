//! JSONL fixture records must stay whole when independent mock providers write together.
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;

pub(crate) fn append(path: impl AsRef<Path>, value: &serde_json::Value) -> io::Result<()> {
    // Value's Display formatter writes many fragments. Serialize before opening
    // the shared append stream, then lock through the entire record write.
    let mut record = serde_json::to_vec(value).map_err(io::Error::other)?;
    record.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    file.lock()?;
    file.write_all(&record)?;
    file.unlock()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn concurrent_providers_preserve_complete_large_prompt_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("prompts.jsonl");
        let checkpoint = "historical context 🦀\\n".repeat(8192);
        std::thread::scope(|scope| {
            for provider in 0..4 {
                let path = &path;
                let checkpoint = &checkpoint;
                scope.spawn(move || {
                    for turn in 0..8 {
                        append(
                            path,
                            &json!({"provider": provider, "turn": turn, "checkpoint": checkpoint}),
                        )
                        .unwrap();
                    }
                });
            }
        });
        let mut records = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        records.sort_by_key(|record| {
            (
                record["provider"].as_i64().unwrap(),
                record["turn"].as_i64().unwrap(),
            )
        });
        let mut expected = Vec::new();
        for provider in 0..4 {
            for turn in 0..8 {
                expected
                    .push(json!({"provider": provider, "turn": turn, "checkpoint": checkpoint}));
            }
        }
        assert_eq!(records, expected);
    }
}
