//! "Did you mean": nearby files for a path that does not exist.

use std::path::Path;
use std::path::PathBuf;

use similar::TextDiff;

use crate::fs::ToolFs;

const MAX_SUGGESTIONS: usize = 3;
const MIN_SCORE: f32 = 0.6;

fn score(wanted: &str, candidate: &str) -> f32 {
    let (wanted, candidate) = (wanted.to_lowercase(), candidate.to_lowercase());
    if wanted == candidate {
        return 1.0;
    }
    let stem = |name: &str| -> String {
        Path::new(name)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if !wanted.is_empty() && stem(&wanted) == stem(&candidate) {
        return 0.95;
    }
    if candidate.contains(&wanted) || wanted.contains(&candidate) {
        return 0.8;
    }
    TextDiff::from_chars(wanted.as_str(), candidate.as_str()).ratio()
}

/// Up to three existing paths resembling `missing`: other extensions of the same stem, names
/// containing it, close spellings, or (when a parent directory is missing too) the closest
/// spelling of the first missing directory.
pub(crate) async fn suggest(fs: &dyn ToolFs, missing: &Path) -> Vec<PathBuf> {
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = missing.to_path_buf();
    let entries = loop {
        let Some(name) = ancestor.file_name().map(ToOwned::to_owned) else {
            return Vec::new();
        };
        remainder.push(name);
        if !ancestor.pop() {
            return Vec::new();
        }
        if remainder.len() > 4 {
            return Vec::new();
        }
        if let Ok(entries) = fs.read_dir(&ancestor).await {
            break entries;
        }
    };
    // `remainder` is deepest first; the entry we compare against is the shallowest missing part.
    let Some(wanted) = remainder
        .last()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    let tail: PathBuf = remainder.iter().rev().skip(1).collect();

    let mut scored: Vec<(f32, PathBuf)> = entries
        .iter()
        .filter_map(|entry| {
            let value = score(&wanted, &entry.name);
            (value >= MIN_SCORE).then(|| (value, ancestor.join(&entry.name).join(&tail)))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, path)| path)
        .collect()
}
