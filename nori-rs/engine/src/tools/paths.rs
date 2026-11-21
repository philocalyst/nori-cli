//! Paths the model names, resolved against the session.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use super::Check;
use super::ToolContext;

/// `raw` resolved against the working directory, `.` and `..` folded without touching the disk.
pub(crate) fn resolve(raw: &str, context: &ToolContext) -> PathBuf {
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        context.cwd.join(path)
    };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normal.push(component.as_os_str());
            }
        }
    }
    normal
}

/// The checks touching `path` needs: `action` on it, and `external_directory` when it lies
/// outside every root.
pub(crate) fn checks(action: &str, path: &Path, context: &ToolContext) -> Vec<Check> {
    let mut checks = Vec::new();
    if !context.roots.iter().any(|root| path.starts_with(root)) {
        let directory = path.parent().unwrap_or(path);
        checks.push(Check::new(
            "external_directory",
            directory.to_string_lossy().into_owned(),
        ));
    }
    checks.push(Check::new(action, display(path, context)));
    checks
}

/// `path` as the person reads it: relative to the working directory when inside it.
pub(crate) fn display(path: &Path, context: &ToolContext) -> String {
    path.strip_prefix(&context.cwd)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}
