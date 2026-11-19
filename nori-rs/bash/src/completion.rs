//! Programmable and fallback Bash completion powered by `brush-core`.

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::session::BashSession;

const MAX_VAR_DETAIL_CHARS: usize = 40;

/// Semantic kind of a shell completion candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompletionKind {
    Function,
    Builtin,
    Command,
    Variable,
    File,
    Directory,
    Flag,
}

/// A single completion candidate with replacement range and metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionCandidate {
    pub text: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
    pub replace_start: i64,
    pub replace_len: i64,
}

impl BashSession {
    /// Generates completion candidates at `cursor_byte` in `line`.
    ///
    /// Uses `brush-core`'s programmable completion engine (`complete` / `compgen` /
    /// `compopt` specs) and falls back to command, function, builtin, alias,
    /// variable, and path completions.
    pub async fn complete(&mut self, line: &str, cursor_byte: i64) -> Vec<CompletionCandidate> {
        let mut cursor = usize::try_from(cursor_byte.max(0))
            .unwrap_or(0)
            .min(line.len());
        while cursor > 0 && !line.is_char_boundary(cursor) {
            cursor -= 1;
        }

        let Ok(completions) = self.shell_mut().complete(line, cursor).await else {
            return Vec::new();
        };

        let insertion_index = completions.insertion_index.min(cursor);
        let replace_start = i64::try_from(insertion_index).unwrap_or(0);
        let replace_len = i64::try_from(completions.delete_count).unwrap_or(0);
        let prefix = line.get(insertion_index..cursor).unwrap_or("");
        let in_command_pos = is_command_position(line, insertion_index);
        let has_spec = has_active_completion_spec(self.shell(), line, insertion_index);

        let mut raw_candidates = completions.candidates;

        // `brush-core` only completes PATH/builtins/functions when `cword == 0` and prefix is
        // non-empty. Supplement command positions after `;`, `|`, `&&`, `||`, `(`, `{` (and
        // empty prefixes for session-defined functions/aliases/builtins).
        if in_command_pos && !prefix.starts_with('$') && !has_path_sep(prefix) {
            supplement_command_candidates(self.shell(), prefix, &mut raw_candidates);
        }

        let mut seen = HashSet::with_capacity(raw_candidates.len());
        let mut results = Vec::with_capacity(raw_candidates.len());

        for text in raw_candidates {
            if !seen.insert(text.clone()) {
                continue;
            }
            let (kind, detail) = classify_candidate(self.shell(), &text, in_command_pos, has_spec);
            results.push(CompletionCandidate {
                text,
                kind,
                detail,
                replace_start,
                replace_len,
            });
        }

        results
    }
}

fn supplement_command_candidates(
    shell: &crate::tracked_tasks::SessionShell,
    prefix: &str,
    out: &mut Vec<String>,
) {
    let mut extra = Vec::new();
    for (name, _) in shell.funcs().iter() {
        if name.starts_with(prefix) {
            extra.push(name.to_owned());
        }
    }
    for name in shell.aliases().keys() {
        if name.starts_with(prefix) {
            extra.push(name.clone());
        }
    }
    for (name, reg) in shell.builtins().iter() {
        if reg.is_enabled() && name.starts_with(prefix) {
            extra.push(name.to_owned());
        }
    }
    if !prefix.is_empty() {
        for path in shell.find_executables_in_path_with_prefix(
            prefix,
            shell.options().case_insensitive_pathname_expansion,
        ) {
            if let Some(file_name) = path.file_name() {
                extra.push(file_name.to_string_lossy().into_owned());
            }
        }
    }
    extra.sort();
    extra.dedup();
    out.extend(extra);
}

fn classify_candidate(
    shell: &crate::tracked_tasks::SessionShell,
    text: &str,
    in_command_pos: bool,
    has_spec: bool,
) -> (CompletionKind, Option<String>) {
    if text.starts_with('-') {
        return (CompletionKind::Flag, Some("flag".to_owned()));
    }
    if text.starts_with('$') {
        let detail = variable_detail(shell, text);
        return (CompletionKind::Variable, detail);
    }

    if in_command_pos {
        if shell.funcs().get(text).is_some() {
            return (CompletionKind::Function, Some("function".to_owned()));
        }
        if shell
            .builtins()
            .get(text)
            .is_some_and(brush_core::builtins::Registration::is_enabled)
        {
            return (CompletionKind::Builtin, Some("builtin".to_owned()));
        }
        if let Some(alias_target) = shell.aliases().get(text) {
            return (
                CompletionKind::Command,
                Some(format!("alias -> {alias_target}")),
            );
        }
        if text.ends_with('/') || is_existing_dir(shell, text) {
            return (CompletionKind::Directory, Some("directory".to_owned()));
        }
        if is_existing_file(shell, text) {
            return (CompletionKind::File, Some("file".to_owned()));
        }
        return (CompletionKind::Command, Some("command".to_owned()));
    }

    if text.ends_with('/') || is_existing_dir(shell, text) {
        return (CompletionKind::Directory, Some("directory".to_owned()));
    }
    if is_existing_file(shell, text) {
        return (CompletionKind::File, Some("file".to_owned()));
    }
    if has_spec {
        return (CompletionKind::Command, Some("completion".to_owned()));
    }
    (CompletionKind::File, None)
}

fn variable_detail(shell: &crate::tracked_tasks::SessionShell, candidate: &str) -> Option<String> {
    let name = candidate
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .or_else(|| candidate.strip_prefix('$'))?;
    let val = shell.env_str(name)?;
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return Some("var".to_owned());
    }
    let truncated: String = trimmed.chars().take(MAX_VAR_DETAIL_CHARS).collect();
    if trimmed.chars().count() > MAX_VAR_DETAIL_CHARS {
        Some(format!("{truncated}..."))
    } else {
        Some(truncated)
    }
}

fn is_existing_dir(shell: &crate::tracked_tasks::SessionShell, candidate: &str) -> bool {
    let trimmed = candidate.trim_end_matches('/');
    if trimmed.is_empty() {
        return candidate == "/";
    }
    shell.absolute_path(Path::new(trimmed)).is_dir()
}

fn is_existing_file(shell: &crate::tracked_tasks::SessionShell, candidate: &str) -> bool {
    let path = shell.absolute_path(Path::new(candidate));
    path.is_file()
}

fn has_path_sep(s: &str) -> bool {
    s.contains('/') || (cfg!(windows) && s.contains('\\'))
}

fn is_command_position(line: &str, insertion_index: usize) -> bool {
    let before = line.get(..insertion_index).unwrap_or("").trim_end();
    if before.is_empty() {
        return true;
    }
    before.ends_with(';')
        || before.ends_with('|')
        || before.ends_with("&&")
        || before.ends_with("||")
        || before.ends_with('(')
        || before.ends_with('{')
        || before.ends_with('\n')
}

fn has_active_completion_spec(
    shell: &crate::tracked_tasks::SessionShell,
    line: &str,
    insertion_index: usize,
) -> bool {
    if is_command_position(line, insertion_index) {
        return false;
    }
    let before = line.get(..insertion_index).unwrap_or("").trim_start();
    let Some(cmd) = before.split_whitespace().next() else {
        return false;
    };
    let config = shell.completion_config();
    config
        .get(brush_core::completion::SpecName::Command(cmd))
        .is_some()
        || config
            .get(brush_core::completion::SpecName::Special(
                brush_core::completion::SpecialSpec::Default,
            ))
            .is_some()
}
