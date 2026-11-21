//! Small helpers shared by the tools.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::error::ToolError;

/// Work above this many bytes is moved off the async task onto the blocking pool.
const OFFLOAD_BYTES: usize = 128 * 1024;

/// Run CPU-bound `work` over about `size` bytes: inline when small, on the blocking pool when
/// large, so a tool call never stalls the runtime's workers.
pub(crate) async fn cpu<T, F>(size: usize, work: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if size < OFFLOAD_BYTES {
        return Ok(work());
    }
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::Internal(format!("worker failed: {error}")))
}

/// Number of lines in `text`: a final line without a newline still counts.
pub(crate) fn line_count(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.bytes().filter(|byte| *byte == b'\n').count();
    let extra = usize::from(!text.ends_with('\n'));
    i64::try_from(newlines + extra).unwrap_or(i64::MAX)
}

/// Resolve `.` and `..` lexically; the path need not exist (it may be on another machine).
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    out
}

/// A path as the model should see it: relative to `root` when inside it.
pub(crate) fn display_path(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        Ok(_) | Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn normalizes_lexically() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn counts_lines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
    }
}
