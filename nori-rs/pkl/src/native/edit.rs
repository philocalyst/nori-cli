//! Transactional edits through the native Pkl parser. Editing never evaluates a module.

use std::sync::Arc;

use super::parse_document;
use super::source::Document;
use super::source::Entry;
use super::source::Expr;
use super::source::PropertySource;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueEdit {
    pub path: Vec<String>,
    pub expression: String,
}

/// Replace static property values, retaining every byte outside their spans.
/// Paths can traverse class declarations, object bodies and literal list items.
/// Every path and replacement is checked before any change is returned.
pub fn replace_values(document: &Document, edits: &[ValueEdit]) -> Result<Document, String> {
    let mut replacements = Vec::with_capacity(edits.len());
    for edit in edits {
        let selected = member(document, &document.module().body, &edit.path)?;
        let range = selected
            .value_span
            .clone()
            .ok_or_else(|| "the selected property has no explicit value".to_owned())?;
        document
            .source()
            .get(range.clone())
            .ok_or_else(|| "property span does not identify valid UTF-8 source".to_owned())?;
        let tokens = pklr::lexer::lex(&edit.expression).map_err(|error| error.to_string())?;
        pklr::parser::parse_expr_tokens(&tokens, &edit.expression, "<nori-value-edit>")
            .map_err(|error| error.to_string())?;
        let mut value_tokens = tokens.iter().filter(|token| {
            !matches!(
                token.kind,
                pklr::lexer::TokenKind::Eof | pklr::lexer::TokenKind::DocComment
            )
        });
        let first = value_tokens
            .next()
            .ok_or_else(|| "a replacement requires an expression".to_owned())?;
        let end = value_tokens
            .next_back()
            .map_or(first.end, |token| token.end);
        let value = &edit.expression[first.offset..end];
        let replacement = if selected.body_only {
            format!("= {value}")
        } else {
            value.to_owned()
        };
        replacements.push((range, replacement));
    }
    replacements.sort_by_key(|(range, _)| range.start);
    if replacements
        .windows(2)
        .any(|pair| pair[0].0.end > pair[1].0.start)
    {
        return Err("value edits overlap; select either an ancestor or its descendant".to_owned());
    }
    let mut source = document.source().to_owned();
    for (range, replacement) in replacements.into_iter().rev() {
        source.replace_range(range, &replacement);
    }
    parse_document(&source)
}

fn member<'a>(
    document: &'a Document,
    body: &'a [Entry],
    path: &[String],
) -> Result<&'a PropertySource, String> {
    let Some((name, rest)) = path.split_first() else {
        return Err("a value edit requires a nonempty property path".to_owned());
    };
    let mut matches = body.iter().filter_map(|entry| match entry {
        Entry::Property(property) if property.name.as_ref() == name => Some(property),
        _ => None,
    });
    if let Some(property) = matches.next() {
        if matches.next().is_some() {
            return Err(format!("ambiguous property {name:?}"));
        }
        if rest.is_empty() {
            return document
                .properties()
                .iter()
                .find(|item| Arc::ptr_eq(&item.property, property))
                .ok_or_else(|| format!("property {name:?} has no editable source value"));
        }
        if let Some(body) = &property.body {
            return member(document, body, rest);
        }
        let value = property
            .value
            .as_ref()
            .ok_or_else(|| format!("property {name:?} has no explicit value"))?;
        return expression(document, value, rest);
    }
    for entry in body {
        if let Entry::ClassDef(class, _, _, body) = entry
            && class == name
            && !rest.is_empty()
        {
            return member(document, body, rest);
        }
    }
    Err(format!("unknown static property {name:?}"))
}

fn expression<'a>(
    document: &'a Document,
    value: &'a Expr,
    path: &[String],
) -> Result<&'a PropertySource, String> {
    let Some((name, rest)) = path.split_first() else {
        return Err("select a property inside the list item to edit".to_owned());
    };
    match value {
        Expr::New(_, body, _) | Expr::ObjectBody(body) | Expr::InferredNew(_, body) => {
            if body.iter().any(|entry| matches!(entry, Entry::Elem(_))) {
                let index = name
                    .parse::<i64>()
                    .map_err(|_| format!("invalid list index {name:?}"))?;
                let index = usize::try_from(index)
                    .map_err(|_| "list indices cannot be negative".to_owned())?;
                let value = body
                    .iter()
                    .filter_map(|entry| match entry {
                        Entry::Elem(value) => Some(value),
                        _ => None,
                    })
                    .nth(index)
                    .ok_or_else(|| format!("list index {index} is out of bounds"))?;
                expression(document, value, rest)
            } else {
                member(document, body, path)
            }
        }
        Expr::Call(callee, args) if matches!(callee.as_ref(), Expr::Ident(name) if name == "List" || name == "Set") =>
        {
            let index = name
                .parse::<i64>()
                .map_err(|_| format!("invalid list index {name:?}"))?;
            let index =
                usize::try_from(index).map_err(|_| "list indices cannot be negative".to_owned())?;
            let value = args
                .get(index)
                .ok_or_else(|| format!("list index {index} is out of bounds"))?;
            expression(document, value, rest)
        }
        _ => Err(format!(
            "cannot edit {name:?} inside a computed expression without evaluating it"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn nested_edits_preserve_comments_unicode_spacing_and_unrelated_source() {
        let source = "// π stays byte-for-byte\nsettings = new Dynamic {\n  name = \"café\" // keep this\n  enabled = false\n}\nother = read(\"env:UNTOUCHED\")\n";
        let document = parse_document(source).unwrap();
        let edited = replace_values(
            &document,
            &[
                ValueEdit {
                    path: vec!["settings".into(), "name".into()],
                    expression: "\"茶\"".into(),
                },
                ValueEdit {
                    path: vec!["settings".into(), "enabled".into()],
                    expression: "true".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            edited.source,
            source
                .replace("\"café\"", "\"茶\"")
                .replace("false", "true")
        );
        assert_eq!(document.source, source);
    }

    #[test]
    fn invalid_or_overlapping_edits_leave_the_document_unchanged() {
        let document = parse_document("settings = new Dynamic { count = 1 }\n").unwrap();
        for edits in [
            vec![ValueEdit {
                path: vec!["missing".into()],
                expression: "2".into(),
            }],
            vec![ValueEdit {
                path: vec!["settings".into()],
                expression: "null\ninjected = 3".into(),
            }],
            vec![
                ValueEdit {
                    path: vec!["settings".into()],
                    expression: "null".into(),
                },
                ValueEdit {
                    path: vec!["settings".into(), "count".into()],
                    expression: "2".into(),
                },
            ],
        ] {
            assert!(replace_values(&document, &edits).is_err());
            assert_eq!(document.source, "settings = new Dynamic { count = 1 }\n");
        }
    }

    #[test]
    fn class_defaults_listing_commands_and_amendments_share_native_source_spans() {
        let source = "class Settings { title: String = \"default\" }\ncommands = new Listing { new Dynamic { op = \"send\"; message = \"before\" } }\nsettings { title = \"before\" }\n";
        let document = parse_document(source).unwrap();
        let edited = replace_values(
            &document,
            &[
                ValueEdit {
                    path: vec!["Settings".into(), "title".into()],
                    expression: "\"changed default\"".into(),
                },
                ValueEdit {
                    path: vec!["commands".into(), "0".into(), "message".into()],
                    expression: "\"changed message\"".into(),
                },
                ValueEdit {
                    path: vec!["settings".into(), "title".into()],
                    expression: "\"changed title\"".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            edited.source,
            "class Settings { title: String = \"changed default\" }\ncommands = new Listing { new Dynamic { op = \"send\"; message = \"changed message\" } }\nsettings { title = \"changed title\" }\n"
        );
        let replaced = replace_values(
            &document,
            &[ValueEdit {
                path: vec!["settings".into()],
                expression: "new Dynamic { title = \"replacement\" }".into(),
            }],
        )
        .unwrap();
        assert!(
            replaced
                .source
                .ends_with("settings = new Dynamic { title = \"replacement\" }\n")
        );
    }
}
