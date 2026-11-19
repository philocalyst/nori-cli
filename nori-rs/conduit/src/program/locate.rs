//! Best-effort source lines for program problems.
//!
//! The evaluator hands the runtime data, not spans, so a problem found after evaluation is
//! placed by looking up the literal property that caused it (`to = "reviewr"`) in the parsed
//! module. Steps built by expressions or imports have no line; their problems carry the step
//! path alone.

use nori_pkl::native::Expr;
use nori_pkl::native::parse_document;

pub struct Locator {
    file: String,
    /// `(property name, literal string value, 1-based line)` of every literal property.
    properties: Vec<(String, Option<String>, i64)>,
}

impl Locator {
    pub fn new(file: &str, source: &str) -> Self {
        let properties = parse_document(source)
            .map(|document| {
                document
                    .properties()
                    .iter()
                    .map(|item| {
                        let line = source[..item.declaration_span.start.min(source.len())]
                            .bytes()
                            .filter(|byte| *byte == b'\n')
                            .count();
                        let literal = match item.property.value.as_ref() {
                            Some(Expr::String(text)) => Some(text.to_string()),
                            _ => None,
                        };
                        (
                            item.property.name.to_string(),
                            literal,
                            i64::try_from(line + 1).unwrap_or(0),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            file: file.to_owned(),
            properties,
        }
    }

    pub fn file(&self) -> &str {
        &self.file
    }

    /// `file:line (path)` when `hint` (a property name and its literal value) is found in the
    /// source, otherwise `file (path)`.
    pub fn at(&self, path: &str, hint: Option<(&str, &str)>) -> String {
        let line = hint.and_then(|(name, value)| {
            self.properties
                .iter()
                .find(|(prop, literal, _)| prop == name && literal.as_deref() == Some(value))
                .map(|(_, _, line)| *line)
        });
        match line {
            Some(line) => format!("{}:{line} ({path})", self.file),
            None => format!("{} ({path})", self.file),
        }
    }
}
