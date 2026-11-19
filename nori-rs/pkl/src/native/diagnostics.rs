//! Point evaluation errors at the module's source.
//!
//! The evaluator reports what went wrong (`Expected value of type String, but got type Int ...
//! in property 'message'`) but not where. Syntax errors carry an offset; evaluation errors
//! name the property, the value or the identifier involved, which this module looks up in
//! the parsed main module to give `file:line` and the source line. When several places fit,
//! all of their lines are listed; when none does (the fault is in an imported module), the
//! message is returned as it is with the file that was being evaluated.

use pklr::ErrorFrame;

use super::source::Entry;
use super::source::Expr;
use super::source::parse_document;
/// A property of the module: its name, 1-based line and value span.
type PropertyLine = (String, usize, Option<(usize, usize)>);
// (name, byte offset of the declaration, value range)

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before.len(), |at| before.len() - at - 1)
        + 1;
    (line, column)
}

fn source_line(text: &str, line: usize) -> &str {
    text.lines().nth(line - 1).unwrap_or("")
}

/// One place in a module's source: the module name, its text and a byte range.
struct Site<'a> {
    module: &'a str,
    text: &'a str,
    start: usize,
    end: usize,
}

/// `  --> module:line:col`, the source line and a caret under the range.
fn excerpt(site: &Site<'_>) -> String {
    let (line, column) = line_col(site.text, site.start);
    let shown = source_line(site.text, line);
    let width = site
        .end
        .saturating_sub(site.start)
        .min(shown.len().saturating_sub(column - 1))
        .max(1);
    format!(
        "\n  --> {}:{line}:{column}\n   | {shown}\n   | {}{}",
        site.module,
        " ".repeat(column - 1),
        "^".repeat(width)
    )
}

fn frame_site(
    frame: &ErrorFrame,
    range: &std::ops::Range<usize>,
) -> (String, String, usize, usize) {
    (
        frame.module.clone(),
        frame.source.to_string(),
        range.start,
        range.end,
    )
}

/// The text between the first pair of `open`/`close` after `marker` in `message`.
fn quoted<'a>(message: &'a str, marker: &str, open: char, close: char) -> Option<&'a str> {
    let rest = &message[message.find(marker)? + marker.len()..];
    let start = rest.find(open)? + open.len_utf8();
    let end = start + rest[start..].find(close)?;
    Some(&rest[start..end])
}

/// Visit every object body in `entries`, passing the property names the body declares and the
/// line of its first property.
fn bodies<'a>(
    entries: &'a [Entry],
    root: bool,
    source: &str,
    properties: &[PropertyLine],
    visit: &mut dyn FnMut(&[&'a str], Option<usize>),
) {
    let names: Vec<&str> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Property(property) => Some(property.name.as_ref()),
            _ => None,
        })
        .collect();
    let first_line = entries.iter().find_map(|entry| match entry {
        Entry::Property(property) => properties
            .iter()
            .find(|(name, _, _)| name == property.name.as_ref())
            .map(|(_, line, _)| *line),
        _ => None,
    });
    if !root {
        visit(&names, first_line);
    }
    for entry in entries {
        match entry {
            Entry::Property(property) => {
                if let Some(body) = &property.body {
                    bodies(body, false, source, properties, visit);
                }
                if let Some(value) = &property.value {
                    expr_bodies(value, source, properties, visit);
                }
            }
            Entry::Elem(value) | Entry::Spread(value) => {
                expr_bodies(value, source, properties, visit);
            }
            Entry::DynProperty(_, value) => expr_bodies(value, source, properties, visit),
            Entry::WhenGenerator(generator) => {
                bodies(&generator.body, false, source, properties, visit);
            }
            Entry::ForGenerator(generator) => {
                bodies(&generator.body, false, source, properties, visit);
            }
            // Class definitions, aliases and predicates hold no program objects.
            _ => {}
        }
    }
}

fn expr_bodies<'a>(
    expr: &'a Expr,
    source: &str,
    properties: &[PropertyLine],
    visit: &mut dyn FnMut(&[&'a str], Option<usize>),
) {
    match expr {
        Expr::New(_, body, _) | Expr::ObjectBody(body) | Expr::InferredNew(_, body) => {
            bodies(body, false, source, properties, visit);
        }
        Expr::Call(_, args) => {
            for arg in args {
                expr_bodies(arg, source, properties, visit);
            }
        }
        _ => {}
    }
}

/// The first line of the declared property, for "declared at" notes.
fn declaration_note(frame: &ErrorFrame) -> String {
    let (line, column) = line_col(&frame.source, frame.declaration.start);
    let text = source_line(&frame.source, line).trim();
    format!("\n  declared at {}:{line}:{column}: {text}", frame.module)
}

/// Where in the module text the evaluation error `message` arose, as appended excerpts.
fn locate_eval(evaluator: &pklr::Evaluator, message: &str, source: &str, file: &str) -> String {
    let declaration = evaluator
        .error_declaration()
        .filter(|(seen, _)| seen == message)
        .map(|(_, frame)| frame);
    let mut sites: Vec<(String, String, usize, usize)> = Vec::new();

    // The evaluator knows which object literal was being built when the member failed and where
    // each of its properties was written: that is the span of the failure.
    let objects = evaluator.error_objects(message);
    let wanted = quoted(message, "in property ", '\'', '\'')
        .or_else(|| quoted(message, "Tried to read property ", '`', '`'));
    if let Some(object) = objects.iter().find(|object| {
        object
            .properties
            .iter()
            .any(|frame| frame.source.as_ref() == source)
    }) {
        match wanted.and_then(|name| {
            object
                .properties
                .iter()
                .find(|frame| frame.property == name)
        }) {
            Some(frame) => {
                sites.push(frame_site(
                    frame,
                    frame.value.as_ref().unwrap_or(&frame.declaration),
                ));
            }
            None => {
                // The object does not set the property: point at the object's first property.
                if let Some(frame) = object.properties.first() {
                    sites.push(frame_site(frame, &frame.declaration));
                }
            }
        }
    }

    if !sites.is_empty() {
        // Spans came from the evaluator; no lookup by name is needed.
    } else if let Some(name) = quoted(message, "in property ", '\'', '\'') {
        let value = message
            .lines()
            .find_map(|line| line.strip_prefix("Value: "))
            .map(str::trim);
        let candidates: Vec<ErrorFrame> = evaluator
            .properties_named(name)
            .into_iter()
            .filter(|frame| {
                declaration.as_ref().is_none_or(|decl| {
                    !(decl.module == frame.module && decl.declaration == frame.declaration)
                })
            })
            .collect();
        let in_main = |frame: &&ErrorFrame| frame.source.as_ref() == source;
        let exact: Vec<&ErrorFrame> = candidates
            .iter()
            .filter(in_main)
            .filter(|frame| {
                frame
                    .value
                    .as_ref()
                    .zip(value)
                    .is_some_and(|(span, value)| {
                        let text = frame.source[span.clone()].trim();
                        text == value || text.trim_matches('"') == value.trim_matches('"')
                    })
            })
            .collect();
        if exact.is_empty() {
            for frame in candidates.iter().filter(in_main) {
                sites.push(frame_site(frame, &frame.declaration));
            }
        } else {
            for frame in exact {
                sites.push(frame_site(
                    frame,
                    frame.value.as_ref().unwrap_or(&frame.declaration),
                ));
            }
        }
    } else if let Some(name) = quoted(message, "Tried to read property ", '`', '`')
        .filter(|_| message.contains("value is undefined"))
        && let Ok(document) = parse_document(source)
    {
        // An object that never sets `name`: report where such objects begin.
        let properties: Vec<PropertyLine> = document
            .properties()
            .iter()
            .map(|item| {
                (
                    item.property.name.to_string(),
                    item.declaration_span.start,
                    item.value_span.as_ref().map(|span| (span.start, span.end)),
                )
            })
            .collect();
        let mut found = Vec::new();
        bodies(
            &document.module().body,
            true,
            source,
            &properties,
            &mut |names, first| {
                if !names.contains(&name)
                    && !names.is_empty()
                    && let Some(offset) = first
                {
                    found.push(offset);
                }
            },
        );
        found.dedup();
        // Only objects shaped like others that do set it are suspect.
        if properties.iter().any(|(prop, _, _)| prop == name) || found.len() == 1 {
            for offset in found {
                sites.push((
                    file.to_owned(),
                    source.to_owned(),
                    offset,
                    offset + name.len(),
                ));
            }
        }
    } else if let Some(name) = quoted(message, "Cannot find type ", '`', '`')
        .or_else(|| quoted(message, "Cannot find property ", '`', '`'))
        .or_else(|| quoted(message, "Cannot find ", '`', '`'))
    {
        let mut offset = 0;
        for line in source.split_inclusive('\n') {
            if let Some(at) = line.find(name) {
                sites.push((
                    file.to_owned(),
                    source.to_owned(),
                    offset + at,
                    offset + at + name.len(),
                ));
            }
            offset += line.len();
        }
    } else if let Some(frame) = evaluator
        .error_frames()
        .into_iter()
        .rev()
        .find(|frame| frame.source.as_ref() == source)
    {
        sites.push(frame_site(&frame, &frame.declaration));
    }
    let mut out = String::new();
    sites.sort_by_key(|site| site.2);
    sites.dedup_by_key(|site| site.2);
    for (module, text, start, end) in sites.iter().take(3) {
        out.push_str(&excerpt(&Site {
            module,
            text,
            start: *start,
            end: *end,
        }));
    }
    if sites.len() > 3 {
        out.push_str(&format!(
            "\n  ... and {} more place(s) that fit",
            sites.len() - 3
        ));
    }
    if let Some(frame) = &declaration {
        out.push_str(&declaration_note(frame));
    }
    out
}

/// A readable message for `error`, located in the module `file` whose text is `source`, and
/// the modules it passed through.
pub(super) fn describe(
    error: &pklr::Error,
    evaluator: &pklr::Evaluator,
    source: &str,
    file: &str,
) -> String {
    let mut text = match error {
        pklr::Error::Parse { src, message, .. } | pklr::Error::Lex { src, message, .. } => {
            let offset = error.source_offset().unwrap_or(0);
            let (name, text) = (src.name(), src.inner().as_str());
            format!(
                "{message}{}",
                excerpt(&Site {
                    module: name,
                    text,
                    start: offset,
                    end: offset + 1,
                })
            )
        }
        pklr::Error::Eval(message) => {
            format!("{message}{}", locate_eval(evaluator, message, source, file))
        }
        pklr::Error::ImportNotFound(uri) => {
            // The import, amends or extends clause that names it.
            let name = std::path::Path::new(uri)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(uri);
            let mut sites = String::new();
            let mut offset = 0;
            for line in source.split_inclusive('\n') {
                let trimmed = line.trim_start();
                if (trimmed.starts_with("import")
                    || trimmed.starts_with("amends")
                    || trimmed.starts_with("extends"))
                    && let Some(at) = line.find(name)
                {
                    sites.push_str(&excerpt(&Site {
                        module: file,
                        text: source,
                        start: offset + at,
                        end: offset + at + name.len(),
                    }));
                }
                offset += line.len();
            }
            format!("Cannot find module `{uri}`.{sites}")
        }
        other => other.to_string(),
    };
    for step in evaluator.error_module_chain() {
        text.push_str(&format!("\n  {step}"));
    }
    text
}
