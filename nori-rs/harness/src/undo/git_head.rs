//! What git's HEAD says about a path.
//!
//! A diff that reports no previous content means a new file, but an agent that omits it for an
//! overwrite would make undo delete a file that was there all along. A file git tracks in HEAD was
//! not new.

use std::path::Path;
use std::process::Command;
use std::process::Stdio;

/// What HEAD holds at a path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HeadState {
    /// Not in a repository, not tracked, or git is not available: nothing says the file existed.
    Untracked,
    /// HEAD has the file, with this text.
    Tracked(String),
    /// HEAD has the file but not as text, so it cannot be put back as one.
    TrackedBinary,
}

fn git(directory: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
}

/// What the repository containing `path` holds for it in HEAD.
pub(crate) fn head_state(path: &Path) -> HeadState {
    let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
        return HeadState::Untracked;
    };
    let spec = format!("HEAD:./{}", name.to_string_lossy());
    if git(directory, &["cat-file", "-e", &spec]).is_none() {
        return HeadState::Untracked;
    }
    match git(directory, &["show", &spec]).map(|output| String::from_utf8(output.stdout)) {
        Some(Ok(text)) => HeadState::Tracked(text),
        Some(Err(_)) | None => HeadState::TrackedBinary,
    }
}
