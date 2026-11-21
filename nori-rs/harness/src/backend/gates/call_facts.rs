//! What a permission request says about the tool call it is for: the command, and the paths it
//! acts on.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

pub(in crate::backend) fn command_text(raw: Option<&Value>) -> Option<String> {
    let raw = raw?;
    match raw.get("command").or_else(|| raw.get("cmd"))? {
        Value::String(command) => {
            let mut text = command.clone();
            let args = raw.get("args").and_then(Value::as_array);
            for arg in args.into_iter().flatten().filter_map(Value::as_str) {
                text.push(' ');
                text.push_str(arg);
            }
            Some(text)
        }
        Value::Array(words) => {
            let words: Vec<String> = words
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            if words.is_empty() {
                return None;
            }
            Some(match crate::bash::extract_bash_command(&words) {
                Some((_, script)) => script.to_owned(),
                None => words.join(" "),
            })
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => None,
    }
}

pub(in crate::backend) fn raw_paths(raw: Option<&Value>) -> impl Iterator<Item = &str> {
    ["file_path", "filePath", "path", "notebook_path"]
        .into_iter()
        .filter_map(move |key| raw?.get(key)?.as_str())
}

/// `path` made absolute and free of `.` and `..`, then relative to `cwd` when inside it. `..` is
/// resolved by name, not on disk, so a path cannot step out of a pattern's directory unseen.
pub(in crate::backend) fn project_path(path: &Path, cwd: &Path) -> String {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                resolved.push(component);
            }
        }
    }
    match resolved.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().into_owned(),
        Ok(_) | Err(_) => resolved.to_string_lossy().into_owned(),
    }
}
