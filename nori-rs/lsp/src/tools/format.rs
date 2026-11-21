//! Tool results as text for the model.

use std::fmt::Write as _;
use std::path::Path;

use crate::manager::Answer;
use crate::render::display_path;
use crate::types::CallItem;
use crate::types::EditPreview;
use crate::types::Hover;
use crate::types::Location;
use crate::types::Symbol;

const PREVIEW_FILES: usize = 20;
const PREVIEW_LINES_PER_FILE: usize = 15;

fn place(path: &Path, line: i32, column: i32, root: &Path) -> String {
    format!("{}:{line}:{column}", display_path(path, root))
}

fn header<T>(answer: &Answer<T>, root: &Path) -> String {
    let at = place(
        &answer.target.path,
        answer.target.line,
        answer.target.column,
        root,
    );
    if answer.target.word.is_empty() {
        format!("at {at}")
    } else {
        format!("`{}` at {at}", answer.target.word)
    }
}

pub(super) fn locations(
    answer: &Answer<Vec<Location>>,
    what: &str,
    limit: usize,
    root: &Path,
) -> String {
    let head = header(answer, root);
    if answer.value.is_empty() {
        return format!("No {what} found for {head}.");
    }
    let mut out = format!("{} {what}(s) for {head}:\n", answer.value.len());
    for found in answer.value.iter().take(limit) {
        let _ = writeln!(
            out,
            "{}  {}",
            place(&found.path, found.range.line, found.range.column, root),
            found.snippet
        );
    }
    if answer.value.len() > limit {
        let _ = writeln!(out, "... {} more not shown", answer.value.len() - limit);
    }
    out.trim_end().to_owned()
}

/// References grouped by file, in file order.
pub(super) fn references(answer: &Answer<Vec<Location>>, limit: usize, root: &Path) -> String {
    let head = header(answer, root);
    if answer.value.is_empty() {
        return format!("No references found for {head}.");
    }
    let mut sorted: Vec<&Location> = answer.value.iter().collect();
    sorted.sort_by(|a, b| {
        (&a.path, a.range.line, a.range.column).cmp(&(&b.path, b.range.line, b.range.column))
    });
    let mut out = format!("{} reference(s) to {head}:\n", sorted.len());
    let mut current: Option<&Path> = None;
    for found in sorted.iter().take(limit) {
        if current != Some(found.path.as_path()) {
            let _ = writeln!(out, "{}", display_path(&found.path, root));
            current = Some(found.path.as_path());
        }
        let _ = writeln!(
            out,
            "  {}:{}  {}",
            found.range.line, found.range.column, found.snippet
        );
    }
    if sorted.len() > limit {
        let _ = writeln!(out, "... {} more not shown", sorted.len() - limit);
    }
    out.trim_end().to_owned()
}

pub(super) fn hover(answer: &Answer<Option<Hover>>, root: &Path) -> String {
    let head = header(answer, root);
    match &answer.value {
        Some(hover) => format!("{head}\n\n{}", hover.text),
        None => format!("The language server has no information for {head}."),
    }
}

fn symbol_lines(
    out: &mut String,
    symbols: &[Symbol],
    depth: usize,
    left: &mut usize,
    root: &Path,
    with_path: bool,
) {
    for symbol in symbols {
        if *left == 0 {
            return;
        }
        *left -= 1;
        let indent = "  ".repeat(depth);
        let detail = symbol.detail.as_deref().filter(|d| !d.is_empty());
        let container = symbol.container.as_deref();
        let _ = write!(out, "{indent}{} {}", symbol.kind, symbol.name);
        if let Some(detail) = detail {
            let _ = write!(out, " {detail}");
        }
        if let Some(container) = container {
            let _ = write!(out, " (in {container})");
        }
        if with_path {
            let _ = writeln!(
                out,
                "  {}",
                place(&symbol.path, symbol.range.line, symbol.range.column, root)
            );
        } else {
            let _ = writeln!(out, "  {}:{}", symbol.range.line, symbol.range.column);
        }
        symbol_lines(out, &symbol.children, depth + 1, left, root, with_path);
    }
}

fn count(symbols: &[Symbol]) -> usize {
    symbols
        .iter()
        .map(|symbol| 1 + count(&symbol.children))
        .sum()
}

pub(super) fn symbols(
    found: &[Symbol],
    what: &str,
    with_path: bool,
    limit: usize,
    root: &Path,
) -> String {
    if found.is_empty() {
        return format!("No symbols found {what}.");
    }
    let total = count(found);
    let mut out = format!("{total} symbol(s) {what}:\n");
    let mut left = limit;
    symbol_lines(&mut out, found, 0, &mut left, root, with_path);
    if total > limit {
        let _ = writeln!(out, "... {} more not shown", total - limit);
    }
    out.trim_end().to_owned()
}

pub(super) fn rename(answer: &Answer<EditPreview>, new_name: &str, root: &Path) -> String {
    let head = header(answer, root);
    let preview = &answer.value;
    if preview.files.is_empty() && preview.resource_operations.is_empty() {
        return format!(
            "The language server returned no edits for renaming {head} to `{new_name}`."
        );
    }
    let mut out = format!(
        "Renaming {head} to `{new_name}` would make {} edit(s) in {} file(s). Nothing was changed; apply these with the edit tools:\n",
        preview.edit_count(),
        preview.files.len()
    );
    for file in preview.files.iter().take(PREVIEW_FILES) {
        let _ = writeln!(
            out,
            "\n{} ({} edit(s))",
            display_path(&file.path, root),
            file.edits.len()
        );
        for change in file.preview.iter().take(PREVIEW_LINES_PER_FILE) {
            let _ = writeln!(
                out,
                "  {}: {}\n  {}> {}",
                change.line,
                change.before,
                " ".repeat(change.line.to_string().len() - 1),
                change.after
            );
        }
        if file.preview.len() > PREVIEW_LINES_PER_FILE {
            let _ = writeln!(
                out,
                "  ... {} more lines",
                file.preview.len() - PREVIEW_LINES_PER_FILE
            );
        }
    }
    if preview.files.len() > PREVIEW_FILES {
        let _ = writeln!(
            out,
            "\n... {} more files",
            preview.files.len() - PREVIEW_FILES
        );
    }
    for operation in &preview.resource_operations {
        let _ = writeln!(out, "\nalso: {operation}");
    }
    out.trim_end().to_owned()
}

pub(super) fn calls(
    answer: &Answer<Vec<CallItem>>,
    incoming: bool,
    limit: usize,
    root: &Path,
) -> String {
    let head = header(answer, root);
    let word = if incoming {
        "callers of"
    } else {
        "calls made by"
    };
    if answer.value.is_empty() {
        return format!("No {word} {head}.");
    }
    let mut out = format!("{} {word} {head}:\n", answer.value.len());
    for item in answer.value.iter().take(limit) {
        let _ = writeln!(
            out,
            "{} {}  {}  {}",
            item.kind,
            item.name,
            place(&item.path, item.range.line, item.range.column, root),
            item.snippet
        );
        let sites: Vec<String> = item
            .call_sites
            .iter()
            .map(|site| format!("{}:{}", site.line, site.column))
            .collect();
        if !sites.is_empty() {
            let _ = writeln!(out, "  call sites: {}", sites.join(", "));
        }
    }
    if answer.value.len() > limit {
        let _ = writeln!(out, "... {} more not shown", answer.value.len() - limit);
    }
    out.trim_end().to_owned()
}
