//! Protocol results into plain Nori types: URIs become paths, code-unit offsets become 1-based
//! character columns, and every location gets the source line it points at.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use helix_lsp::OffsetEncoding;
use helix_lsp::lsp;

use crate::position::apply_edits;
use crate::position::from_lsp_range;
use crate::position::split_lines;
use crate::types::CallItem;
use crate::types::CodeAction;
use crate::types::EditPreview;
use crate::types::FileEdit;
use crate::types::Hover;
use crate::types::LineChange;
use crate::types::Location;
use crate::types::Range;
use crate::types::Symbol;
use crate::types::TextEdit;

/// File contents for the duration of one request: what the server saw if the caller supplied it,
/// else the file on disk. Each file is read at most once.
#[derive(Default)]
pub(crate) struct Sources {
    cache: HashMap<PathBuf, Option<Arc<str>>>,
}

impl Sources {
    pub fn with(mut self, path: &Path, text: &Arc<str>) -> Self {
        self.cache
            .insert(path.to_path_buf(), Some(Arc::clone(text)));
        self
    }

    pub async fn text(&mut self, path: &Path) -> Option<Arc<str>> {
        if let Some(known) = self.cache.get(path) {
            return known.clone();
        }
        let read = tokio::fs::read_to_string(path).await.ok().map(Arc::from);
        self.cache.insert(path.to_path_buf(), read.clone());
        read
    }

    /// The range and trimmed first source line of `range` in `path`.
    async fn range(
        &mut self,
        path: &Path,
        range: lsp::Range,
        encoding: OffsetEncoding,
    ) -> (Range, String) {
        match self.text(path).await {
            Some(text) => {
                let lines = split_lines(&text);
                let plain = from_lsp_range(&lines, range, encoding);
                let index = usize::try_from(plain.line).unwrap_or(1).saturating_sub(1);
                let snippet = lines.get(index).map_or("", |line| line.trim()).to_owned();
                (plain, snippet)
            }
            None => (raw_range(range), String::new()),
        }
    }
}

fn raw_range(range: lsp::Range) -> Range {
    let one_based = |value: u32| i32::try_from(value).unwrap_or(i32::MAX).saturating_add(1);
    Range {
        line: one_based(range.start.line),
        column: one_based(range.start.character),
        end_line: one_based(range.end.line),
        end_column: one_based(range.end.character),
    }
}

pub(crate) async fn location(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    uri: &lsp::Url,
    range: lsp::Range,
) -> Option<Location> {
    let path = uri.to_file_path().ok()?;
    let (range, snippet) = sources.range(&path, range, encoding).await;
    Some(Location {
        path,
        range,
        snippet,
    })
}

pub(crate) async fn goto_locations(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    response: lsp::GotoDefinitionResponse,
) -> Vec<Location> {
    let mut out = Vec::new();
    match response {
        lsp::GotoDefinitionResponse::Scalar(found) => {
            out.extend(location(sources, encoding, &found.uri, found.range).await);
        }
        lsp::GotoDefinitionResponse::Array(found) => {
            for item in found {
                out.extend(location(sources, encoding, &item.uri, item.range).await);
            }
        }
        lsp::GotoDefinitionResponse::Link(links) => {
            for link in links {
                let range = link.target_selection_range;
                out.extend(location(sources, encoding, &link.target_uri, range).await);
            }
        }
    }
    out
}

pub(crate) async fn locations(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    found: Vec<lsp::Location>,
) -> Vec<Location> {
    let mut out = Vec::new();
    for item in found {
        out.extend(location(sources, encoding, &item.uri, item.range).await);
    }
    out
}

fn marked(string: lsp::MarkedString) -> String {
    match string {
        lsp::MarkedString::String(text) => text,
        lsp::MarkedString::LanguageString(code) => {
            format!("```{}\n{}\n```", code.language, code.value)
        }
    }
}

pub(crate) fn hover(lines: &[&str], encoding: OffsetEncoding, hover: lsp::Hover) -> Option<Hover> {
    let text = match hover.contents {
        lsp::HoverContents::Scalar(one) => marked(one),
        lsp::HoverContents::Array(many) => many
            .into_iter()
            .map(marked)
            .collect::<Vec<_>>()
            .join("\n\n"),
        lsp::HoverContents::Markup(content) => content.value,
    };
    let text = text.trim().to_owned();
    (!text.is_empty()).then(|| Hover {
        text,
        range: hover
            .range
            .map(|range| from_lsp_range(lines, range, encoding)),
    })
}

const SYMBOL_KINDS: [&str; 26] = [
    "file",
    "module",
    "namespace",
    "package",
    "class",
    "method",
    "property",
    "field",
    "constructor",
    "enum",
    "interface",
    "function",
    "variable",
    "constant",
    "string",
    "number",
    "boolean",
    "array",
    "object",
    "key",
    "null",
    "enum member",
    "struct",
    "event",
    "operator",
    "type parameter",
];

pub(crate) fn kind_name(kind: lsp::SymbolKind) -> String {
    let number = serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    usize::try_from(number - 1)
        .ok()
        .and_then(|index| SYMBOL_KINDS.get(index))
        .map_or_else(|| "symbol".to_owned(), ToString::to_string)
}

fn nested(
    path: &Path,
    lines: &[&str],
    encoding: OffsetEncoding,
    items: Vec<lsp::DocumentSymbol>,
) -> Vec<Symbol> {
    items
        .into_iter()
        .map(|item| Symbol {
            name: item.name,
            kind: kind_name(item.kind),
            detail: item.detail,
            container: None,
            path: path.to_path_buf(),
            range: from_lsp_range(lines, item.selection_range, encoding),
            children: nested(path, lines, encoding, item.children.unwrap_or_default()),
        })
        .collect()
}

pub(crate) async fn document_symbols(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    path: &Path,
    response: lsp::DocumentSymbolResponse,
) -> Vec<Symbol> {
    match response {
        lsp::DocumentSymbolResponse::Nested(items) => {
            let text = sources.text(path).await.unwrap_or_else(|| Arc::from(""));
            nested(path, &split_lines(&text), encoding, items)
        }
        lsp::DocumentSymbolResponse::Flat(items) => flat_symbols(sources, encoding, items).await,
    }
}

async fn flat_symbols(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    items: Vec<lsp::SymbolInformation>,
) -> Vec<Symbol> {
    let mut out = Vec::new();
    for item in items {
        if let Some(place) =
            location(sources, encoding, &item.location.uri, item.location.range).await
        {
            out.push(Symbol {
                name: item.name,
                kind: kind_name(item.kind),
                detail: None,
                container: item.container_name.filter(|name| !name.is_empty()),
                path: place.path,
                range: place.range,
                children: Vec::new(),
            });
        }
    }
    out
}

pub(crate) async fn workspace_symbols(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    response: lsp::WorkspaceSymbolResponse,
) -> Vec<Symbol> {
    match response {
        lsp::WorkspaceSymbolResponse::Flat(items) => flat_symbols(sources, encoding, items).await,
        lsp::WorkspaceSymbolResponse::Nested(items) => {
            let mut out = Vec::new();
            for item in items {
                let (path, range) = match item.location {
                    lsp::OneOf::Left(place) => {
                        let Some(path) = place.uri.to_file_path().ok() else {
                            continue;
                        };
                        let (range, _) = sources.range(&path, place.range, encoding).await;
                        (path, range)
                    }
                    lsp::OneOf::Right(place) => {
                        let Some(path) = place.uri.to_file_path().ok() else {
                            continue;
                        };
                        // The server gave no range; the file is the best we can point at.
                        let top = Range {
                            line: 1,
                            column: 1,
                            end_line: 1,
                            end_column: 1,
                        };
                        (path, top)
                    }
                };
                out.push(Symbol {
                    name: item.name,
                    kind: kind_name(item.kind),
                    detail: None,
                    container: item.container_name.filter(|name| !name.is_empty()),
                    path,
                    range,
                    children: Vec::new(),
                });
            }
            out
        }
    }
}

pub(crate) async fn call_item(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    item: lsp::CallHierarchyItem,
    call_sites: Vec<lsp::Range>,
    sites_in: Option<&Path>,
) -> Option<CallItem> {
    let path = item.uri.to_file_path().ok()?;
    let (range, snippet) = sources.range(&path, item.selection_range, encoding).await;
    let site_path = sites_in.unwrap_or(&path).to_path_buf();
    let mut sites = Vec::new();
    for site in call_sites {
        sites.push(sources.range(&site_path, site, encoding).await.0);
    }
    Some(CallItem {
        name: item.name,
        kind: kind_name(item.kind),
        detail: item.detail,
        path,
        range,
        snippet,
        call_sites: sites,
    })
}

fn plain_edit(lines: &[&str], encoding: OffsetEncoding, edit: &lsp::TextEdit) -> TextEdit {
    TextEdit {
        range: from_lsp_range(lines, edit.range, encoding),
        new_text: edit.new_text.clone(),
    }
}

/// The line `edits` change, before and after. Edits that span lines are shown against their first
/// line only.
fn line_change(lines: &[&str], line: i32, edits: &[&TextEdit]) -> LineChange {
    let index = usize::try_from(line).unwrap_or(1).saturating_sub(1);
    let before = lines.get(index).copied().unwrap_or("");
    let local: Vec<TextEdit> = edits
        .iter()
        .filter(|edit| edit.range.end_line == line)
        .map(|edit| TextEdit {
            range: Range {
                line: 1,
                column: edit.range.column,
                end_line: 1,
                end_column: edit.range.end_column,
            },
            new_text: edit.new_text.clone(),
        })
        .collect();
    let after = if local.len() == edits.len() {
        apply_edits(before, &local).unwrap_or_else(|_| before.to_owned())
    } else {
        let first = edits.first().map_or("", |edit| edit.new_text.as_str());
        format!(
            "{} ... {}",
            before.trim_end(),
            first.lines().next().unwrap_or("")
        )
    };
    LineChange {
        line,
        before: before.trim().to_owned(),
        after: after.trim().to_owned(),
    }
}

async fn file_edit(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    path: PathBuf,
    edits: Vec<lsp::TextEdit>,
) -> FileEdit {
    let text = sources.text(&path).await.unwrap_or_else(|| Arc::from(""));
    let lines = split_lines(&text);
    let mut plain: Vec<TextEdit> = edits
        .iter()
        .map(|edit| plain_edit(&lines, encoding, edit))
        .collect();
    plain.sort_by_key(|edit| (edit.range.line, edit.range.column));
    let mut preview = Vec::new();
    let mut start = 0;
    while start < plain.len() {
        let line = plain[start].range.line;
        let end = plain[start..]
            .iter()
            .position(|edit| edit.range.line != line)
            .map_or(plain.len(), |offset| start + offset);
        let group: Vec<&TextEdit> = plain[start..end].iter().collect();
        preview.push(line_change(&lines, line, &group));
        start = end;
    }
    FileEdit {
        path,
        edits: plain,
        preview,
    }
}

fn operation_text(operation: &lsp::ResourceOp) -> String {
    let show = |uri: &lsp::Url| {
        uri.to_file_path().map_or_else(
            |()| uri.as_str().to_owned(),
            |path| path.display().to_string(),
        )
    };
    match operation {
        lsp::ResourceOp::Create(create) => format!("create {}", show(&create.uri)),
        lsp::ResourceOp::Rename(rename) => {
            format!(
                "rename {} to {}",
                show(&rename.old_uri),
                show(&rename.new_uri)
            )
        }
        lsp::ResourceOp::Delete(delete) => format!("delete {}", show(&delete.uri)),
    }
}

/// Computes what a `WorkspaceEdit` would do, without doing it.
pub(crate) async fn edit_preview(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    edit: lsp::WorkspaceEdit,
) -> EditPreview {
    let mut by_file: Vec<(PathBuf, Vec<lsp::TextEdit>)> = Vec::new();
    let mut add = |uri: &lsp::Url, edits: Vec<lsp::TextEdit>| {
        if let Ok(path) = uri.to_file_path() {
            match by_file.iter_mut().find(|(known, _)| *known == path) {
                Some((_, existing)) => existing.extend(edits),
                None => by_file.push((path, edits)),
            }
        }
    };
    let mut operations = Vec::new();
    let text_edits = |document: lsp::TextDocumentEdit| {
        let edits = document
            .edits
            .into_iter()
            .map(|edit| match edit {
                lsp::OneOf::Left(plain) => plain,
                lsp::OneOf::Right(annotated) => annotated.text_edit,
            })
            .collect();
        (document.text_document.uri, edits)
    };
    match edit.document_changes {
        Some(lsp::DocumentChanges::Edits(documents)) => {
            for document in documents {
                let (uri, edits) = text_edits(document);
                add(&uri, edits);
            }
        }
        Some(lsp::DocumentChanges::Operations(changes)) => {
            for change in changes {
                match change {
                    lsp::DocumentChangeOperation::Edit(document) => {
                        let (uri, edits) = text_edits(document);
                        add(&uri, edits);
                    }
                    lsp::DocumentChangeOperation::Op(operation) => {
                        operations.push(operation_text(&operation));
                    }
                }
            }
        }
        None => {
            for (uri, edits) in edit.changes.unwrap_or_default() {
                add(&uri, edits);
            }
        }
    }
    by_file.sort_by(|a, b| a.0.cmp(&b.0));
    let mut files = Vec::new();
    for (path, edits) in by_file {
        files.push(file_edit(sources, encoding, path, edits).await);
    }
    EditPreview {
        files,
        resource_operations: operations,
    }
}

pub(crate) async fn code_actions(
    sources: &mut Sources,
    encoding: OffsetEncoding,
    items: Vec<lsp::CodeActionOrCommand>,
) -> Vec<CodeAction> {
    let mut out = Vec::new();
    for item in items {
        out.push(match item {
            lsp::CodeActionOrCommand::Command(command) => CodeAction {
                title: command.title,
                kind: None,
                preferred: false,
                disabled: None,
                edit: None,
                command: Some(command.command),
            },
            lsp::CodeActionOrCommand::CodeAction(action) => {
                let edit = match action.edit {
                    Some(edit) => Some(edit_preview(sources, encoding, edit).await),
                    None => None,
                };
                CodeAction {
                    title: action.title,
                    kind: action.kind.map(|kind| kind.as_str().to_owned()),
                    preferred: action.is_preferred.unwrap_or(false),
                    disabled: action.disabled.map(|disabled| disabled.reason),
                    edit,
                    command: action.command.map(|command| command.command),
                }
            }
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn symbol_kind_names() {
        assert_eq!(kind_name(lsp::SymbolKind::FUNCTION), "function");
        assert_eq!(kind_name(lsp::SymbolKind::STRUCT), "struct");
        assert_eq!(kind_name(lsp::SymbolKind::FILE), "file");
    }

    #[tokio::test]
    async fn rename_preview_shows_each_changed_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn old() {}\nfn main() { old(); old(); }\n").unwrap();
        let uri = lsp::Url::from_file_path(&path).unwrap();
        let edit_at = |line: u32, from: u32, to: u32| lsp::TextEdit {
            range: lsp::Range::new(lsp::Position::new(line, from), lsp::Position::new(line, to)),
            new_text: "new".to_owned(),
        };
        let edit = lsp::WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri,
                vec![edit_at(0, 3, 6), edit_at(1, 12, 15), edit_at(1, 19, 22)],
            )])),
            ..Default::default()
        };
        let preview = edit_preview(&mut Sources::default(), OffsetEncoding::Utf16, edit).await;
        assert_eq!(preview.edit_count(), 3);
        assert_eq!(
            preview.files[0].preview,
            vec![
                LineChange {
                    line: 1,
                    before: "fn old() {}".to_owned(),
                    after: "fn new() {}".to_owned()
                },
                LineChange {
                    line: 2,
                    before: "fn main() { old(); old(); }".to_owned(),
                    after: "fn main() { new(); new(); }".to_owned()
                },
            ]
        );
    }
}
