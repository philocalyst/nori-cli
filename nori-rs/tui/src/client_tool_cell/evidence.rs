//! The evidence behind a tool call: its input and output as a tree of properties, read files as
//! numbered code, command and script strings highlighted. Shown only when a cell is expanded.

use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

use super::explore::read_span;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::path_spans;
use crate::presentation::Artifact;
use crate::presentation::TextFormat;
use crate::presentation::ToolSnapshot;
use crate::render::highlight::highlight_code_to_lines;
use crate::render::highlight::highlight_code_to_lines_for_path;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

pub(super) fn append_evidence(
    chrome: &Chrome,
    lines: &mut Vec<Line<'static>>,
    snapshot: &ToolSnapshot,
    width: u16,
) {
    let source_json = (snapshot.kind == crate::presentation::ToolKind::Read
        || snapshot.title.eq_ignore_ascii_case("read"))
        && snapshot.raw_output.is_none()
        && snapshot
            .raw_input
            .as_ref()
            .and_then(|v| v.get("path").or_else(|| v.get("file_path")))
            .and_then(Value::as_str)
            .or_else(|| {
                snapshot
                    .locations
                    .first()
                    .and_then(|location| location.path.to_str())
            })
            .or_else(|| match &snapshot.invocation {
                Some(crate::presentation::Invocation::Read { path }) => path.to_str(),
                _ => None,
            })
            .is_some_and(|path| path.ends_with(".json"));
    let parsed_output = snapshot
        .artifacts
        .iter()
        .find_map(|artifact| match artifact {
            Artifact::Text { text, .. } if !source_json => {
                serde_json::from_str::<Value>(&clean(text)).ok()
            }
            Artifact::Text { .. } => None,
            Artifact::Diff(_) => None,
        });
    let parsed_raw = snapshot
        .raw_output
        .as_ref()
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(&clean(text)).ok())
        .filter(|value| value.is_object() || value.is_array());
    let output = parsed_raw
        .as_ref()
        .or(snapshot.raw_output.as_ref())
        .or(parsed_output.as_ref());
    let path = snapshot
        .raw_input
        .as_ref()
        .and_then(|value| value.get("path").or_else(|| value.get("file_path")))
        .and_then(Value::as_str)
        .or_else(|| {
            output
                .and_then(|value| value.get("path").or_else(|| value.get("file_path")))
                .and_then(Value::as_str)
        })
        .or_else(|| match &snapshot.invocation {
            Some(crate::presentation::Invocation::Read { path }) => path.to_str(),
            _ => None,
        })
        .or_else(|| {
            snapshot
                .locations
                .first()
                .and_then(|location| location.path.to_str())
        });
    let is_read = snapshot.kind == crate::presentation::ToolKind::Read
        || snapshot.title.eq_ignore_ascii_case("read");
    if snapshot.raw_input.is_none() && !is_read {
        match &snapshot.invocation {
            Some(crate::presentation::Invocation::Read { path }) => {
                property(chrome, lines, "Path", &path.display().to_string(), 0, width)
            }
            Some(crate::presentation::Invocation::Command { command }) => append_wrapped(
                chrome,
                lines,
                highlight_code_to_lines(&clean(command), "bash"),
                0,
                width,
            ),
            Some(invocation) => {
                if let Ok(mut value) = serde_json::to_value(invocation) {
                    if let Some(object) = value.as_object_mut() {
                        object.remove("invocation_type");
                    }
                    tree(chrome, lines, None, &value, 0, width, path);
                }
            }
            None => {}
        }
    }
    if is_read && path.is_some() {
        let path = path.unwrap_or_default();
        let line_range = read_line_range_label(snapshot);
        let mut hdr = vec![Span::styled("  ◇ ", chrome.faint())];
        hdr.extend(path_spans(&clean(path), chrome));
        if let Some(range) = &line_range {
            hdr.push(Span::styled(format!("  {range}"), chrome.muted()));
        }
        lines.push(Line::from(hdr));
        if let Some(input) = &snapshot.raw_input {
            let mut remaining = input.clone();
            if let Some(object) = remaining.as_object_mut() {
                object.remove("path");
                object.remove("file_path");
                object.remove("offset");
                object.remove("limit");
                object.remove("start_line");
                object.remove("end_line");
                object.remove("line");
            }
            if remaining
                .as_object()
                .is_none_or(|object| !object.is_empty())
            {
                tree(chrome, lines, None, &remaining, 0, width, Some(path));
            }
        }
        let start_line_num = read_start_line(snapshot).unwrap_or(1);
        if let Some(output) = output {
            let mut remaining = output.clone();
            if let Some(object) = remaining.as_object_mut() {
                let success = object.remove("success").and_then(|v| v.as_bool());
                let content = object.remove("lines");
                let count = content.as_ref().and_then(|v| {
                    v.as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| v.as_array().map(|a| a.len().to_string()))
                });
                if let Some(count) = count {
                    let outcome = if success == Some(false) {
                        "read failed"
                    } else {
                        "lines read"
                    };
                    property(chrome, lines, "", &format!("{count} {outcome}"), 0, width);
                } else if let Some(success) = success {
                    property(
                        chrome,
                        lines,
                        "",
                        if success {
                            "Read successfully"
                        } else {
                            "Read failed"
                        },
                        0,
                        width,
                    );
                }
                match content {
                    Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                        let text = items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n");
                        let highlighted = highlight_code_to_lines_for_path(
                            &clean(&text),
                            std::path::Path::new(path),
                        );
                        append_numbered_code(chrome, lines, highlighted, start_line_num, width);
                    }
                    Some(Value::Number(_)) | None => {}
                    Some(value) => tree(chrome, lines, Some("lines"), &value, 0, width, Some(path)),
                }
                if let Some(content) = object.remove("content") {
                    if let Some(text) = content.as_str() {
                        let highlighted = highlight_code_to_lines_for_path(
                            &clean(text),
                            std::path::Path::new(path),
                        );
                        append_numbered_code(chrome, lines, highlighted, start_line_num, width);
                    } else {
                        tree(
                            chrome,
                            lines,
                            Some("content"),
                            &content,
                            0,
                            width,
                            Some(path),
                        );
                    }
                }
                if object.get("path").and_then(Value::as_str) == Some(path) {
                    object.remove("path");
                }
                if object.get("file_path").and_then(Value::as_str) == Some(path) {
                    object.remove("file_path");
                }
            }
            if remaining
                .as_object()
                .is_none_or(|object| !object.is_empty())
            {
                tree(chrome, lines, None, &remaining, 0, width, Some(path));
            }
        }
    } else {
        if let Some(value) = &snapshot.raw_input {
            tree(chrome, lines, None, value, 0, width, path);
        }
        if let Some(value) = output {
            tree(chrome, lines, None, value, 0, width, path);
        }
    }
    for artifact in &snapshot.artifacts {
        match artifact {
            Artifact::Text { text, format } => {
                if source_json {
                    append_wrapped(
                        chrome,
                        lines,
                        highlight_code_to_lines(&clean(text), "json"),
                        0,
                        width,
                    );
                    continue;
                }
                if output.is_some_and(|value| {
                    value.as_str() == Some(text.as_str())
                        || serde_json::from_str::<Value>(&clean(text)).ok().as_ref() == Some(value)
                }) {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&clean(text)) {
                    tree(chrome, lines, None, &value, 0, width, path);
                } else {
                    append_text(chrome, lines, text, width, *format == TextFormat::Markdown);
                }
            }
            Artifact::Diff(_) => {} // The existing diff renderer owns the visual diff.
        }
    }
}

fn tree(
    chrome: &Chrome,
    lines: &mut Vec<Line<'static>>,
    key: Option<&str>,
    value: &Value,
    depth: usize,
    width: u16,
    path: Option<&str>,
) {
    match value {
        Value::Object(map) => {
            if let Some(key) = key {
                property(chrome, lines, &humanize(key), "", depth, width);
            }
            let next = depth + usize::from(key.is_some());
            if map.is_empty() {
                property(chrome, lines, "Empty", "", next, width);
            }
            for (name, child) in map {
                if (name == "blob"
                    || (matches!(name.as_str(), "data" | "base64")
                        && (map
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| matches!(kind, "image" | "audio" | "resource"))
                            || map.get("mimeType").and_then(Value::as_str).is_some())))
                    && child.as_str().is_some()
                {
                    let bytes = child.as_str().map_or(0, str::len);
                    property(
                        chrome,
                        lines,
                        &humanize(name),
                        &format!("▧ attachment · {bytes} encoded characters"),
                        next,
                        width,
                    );
                } else {
                    tree(chrome, lines, Some(name), child, next, width, path);
                }
            }
        }
        Value::Array(items) => {
            if matches!(key, Some("lines")) && items.iter().all(Value::is_string) {
                property(
                    chrome,
                    lines,
                    "Lines",
                    &items.len().to_string(),
                    depth,
                    width,
                );
                let text = items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n");
                let rendered = path
                    .map(|p| {
                        highlight_code_to_lines_for_path(&clean(&text), std::path::Path::new(p))
                    })
                    .unwrap_or_else(|| {
                        text.lines()
                            .map(|line| Line::from(line.to_owned()))
                            .collect()
                    });
                append_wrapped(chrome, lines, rendered, depth, width);
            } else {
                if let Some(key) = key {
                    property(
                        chrome,
                        lines,
                        &humanize(key),
                        &format!("{} items", items.len()),
                        depth,
                        width,
                    );
                }
                for (index, item) in items.iter().enumerate() {
                    tree(
                        chrome,
                        lines,
                        Some(&format!("{}", index + 1)),
                        item,
                        depth + usize::from(key.is_some()),
                        width,
                        path,
                    );
                }
            }
        }
        Value::String(text) if depth == 0 && matches!(key, Some("code" | "command")) => {
            let language = if key == Some("code") {
                "javascript"
            } else {
                "bash"
            };
            append_wrapped(
                chrome,
                lines,
                highlight_code_to_lines(&clean(text), language),
                depth,
                width,
            );
        }
        Value::String(text) => {
            if !matches!(key, Some("content" | "source"))
                && let Ok(nested) = serde_json::from_str::<Value>(&clean(text))
                && (nested.is_object() || nested.is_array())
            {
                tree(chrome, lines, key, &nested, depth, width, path);
            } else if text.contains('\n') || text.contains("```") {
                if let Some(key) = key {
                    property(chrome, lines, &humanize(key), "", depth, width);
                }
                append_text(chrome, lines, text, width, text.contains("```"));
            } else {
                property(
                    chrome,
                    lines,
                    &key.map(humanize).unwrap_or_default(),
                    text,
                    depth,
                    width,
                );
            }
        }
        Value::Bool(value) => property(
            chrome,
            lines,
            &key.map(humanize).unwrap_or_default(),
            if *value { "Yes" } else { "No" },
            depth,
            width,
        ),
        Value::Null if key == Some("goal") => {
            property(chrome, lines, "", "No active goal", depth, width)
        }
        Value::Null => property(
            chrome,
            lines,
            &key.map(humanize).unwrap_or_default(),
            "None",
            depth,
            width,
        ),
        Value::Number(number) => property(
            chrome,
            lines,
            &key.map(humanize).unwrap_or_default(),
            &number.to_string(),
            depth,
            width,
        ),
    }
}

pub(super) fn clean(text: &str) -> String {
    codex_ansi_escape::ansi_escape(text)
        .lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn humanize(key: &str) -> String {
    let mut text = key.replace(['_', '-'], " ");
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text
}

fn property(
    chrome: &Chrome,
    lines: &mut Vec<Line<'static>>,
    name: &str,
    value: &str,
    depth: usize,
    width: u16,
) {
    let indent = format!("  {}", "  ".repeat(depth));
    let rendered = Line::from(vec![
        indent.into(),
        Span::styled(clean(name), chrome.cyan()),
        if name.is_empty() || value.is_empty() {
            "".into()
        } else {
            "  ".into()
        },
        Span::styled(clean(value), chrome.secondary()),
    ]);
    lines.extend(word_wrap_lines(
        [rendered],
        RtOptions::new(usize::from(width).max(1)),
    ));
}

fn append_text(
    chrome: &Chrome,
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: u16,
    markdown: bool,
) {
    let cleaned = clean(text);
    let text = cleaned.as_str();
    let rendered = if markdown {
        crate::markdown_render::render_markdown_text_with_width(
            text,
            Some(usize::from(width.saturating_sub(4)).max(1)),
        )
        .lines
    } else {
        text.split('\n')
            .map(|line| Line::from(line.to_owned()))
            .collect()
    };
    append_wrapped(chrome, lines, rendered, 0, width);
}

fn read_start_line(snapshot: &ToolSnapshot) -> Option<usize> {
    read_span(snapshot).map(|span| span.start)
}

fn read_line_range_label(snapshot: &ToolSnapshot) -> Option<String> {
    read_span(snapshot).map(|span| format!("L{}", span.label()))
}

fn append_numbered_code(
    chrome: &Chrome,
    lines: &mut Vec<Line<'static>>,
    rendered: Vec<Line<'static>>,
    start_line: usize,
    width: u16,
) {
    let max_num = start_line.saturating_add(rendered.len());
    let num_w = max_num.to_string().len().max(3);
    for (idx, code_line) in rendered.into_iter().enumerate() {
        let ln = start_line.saturating_add(idx);
        let prefix = format!("  {ln:>num_w$} │ ");
        let wrap_budget = usize::from(width)
            .saturating_sub(prefix.chars().count())
            .max(1);
        let wrapped = word_wrap_lines([code_line], RtOptions::new(wrap_budget));
        for (sub_idx, wline) in wrapped.into_iter().enumerate() {
            let gutter = if sub_idx == 0 {
                prefix.clone()
            } else {
                format!("  {:>num_w$} │ ", "")
            };
            let mut spans = vec![Span::styled(gutter, chrome.faint())];
            spans.extend(wline.spans);
            lines.push(Line::from(spans));
        }
    }
}

fn append_wrapped(
    chrome: &Chrome,
    lines: &mut Vec<Line<'static>>,
    rendered: Vec<Line<'static>>,
    depth: usize,
    width: u16,
) {
    let prefix = format!("  {}│ ", "  ".repeat(depth));
    for line in word_wrap_lines(
        &rendered,
        RtOptions::new(
            usize::from(width)
                .saturating_sub(depth.saturating_mul(2) + 4)
                .max(1),
        ),
    ) {
        let mut spans = vec![Span::styled(prefix.clone(), chrome.faint())];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
}
