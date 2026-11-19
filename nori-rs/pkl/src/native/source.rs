//! Lossless source inspection through the same Rust parser used by evaluation.

use pklr::lexer;
use pklr::parser;

pub use parser::Entry;
pub use parser::Expr;
pub use parser::Module;
pub use parser::Property;
pub use parser::PropertySource;

#[derive(Clone, Debug)]
pub struct Document {
    pub(super) source: String,
    pub(super) module: Module,
    pub(super) properties: Vec<PropertySource>,
}

impl Document {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn module(&self) -> &Module {
        &self.module
    }

    pub fn properties(&self) -> &[PropertySource] {
        &self.properties
    }
}

/// Parse without evaluating expressions, resolving imports, or reading resources.
pub fn parse_document(source: &str) -> Result<Document, String> {
    let tokens = lexer::lex(source).map_err(|error| error.to_string())?;
    let parsed = parser::parse_named_spanned(&tokens, source, "<nori-source>")
        .map_err(|error| error.to_string())?;
    Ok(Document {
        source: source.to_owned(),
        module: parsed.module,
        properties: parsed.properties,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn complete_grammar_retains_literal_and_property_spans_without_evaluation() {
        let source = "// café\nclass Config { text: String }\nconfig = new Config { text = #\"raw \\(untouched)\"# }\nnever = read(\"file:/definitely-not-a-real-nori-resource\")\n";
        let parsed = parse_document(source).unwrap();
        assert_eq!(parsed.source, source);
        let property = parsed
            .properties
            .iter()
            .find(|item| item.property.name.as_ref() == "text" && item.value_span.is_some())
            .unwrap();
        assert_eq!(
            &source[property.value_span.clone().unwrap()],
            "#\"raw \\(untouched)\"#"
        );
        assert!(
            matches!(property.property.value.as_ref(), Some(Expr::String(value)) if value.as_ref() == "raw \\(untouched)")
        );
    }

    #[test]
    fn unicode_and_interpolation_are_distinct_syntax() {
        let parsed = parse_document(
            "literal = \"\\u{1f642}\"\ncomputed = \"hello \\(read(\"env:NORI_SOURCE_TEST\"))\"\n",
        )
        .unwrap();
        let entries = parsed.module.body.as_ref();
        assert!(
            matches!(&entries[0], Entry::Property(property) if matches!(property.value.as_ref(), Some(Expr::String(value)) if value.as_ref() == "🙂"))
        );
        assert!(
            matches!(&entries[1], Entry::Property(property) if matches!(property.value.as_ref(), Some(Expr::StringInterpolation(_))))
        );
        assert!(parse_document("value = \"\\q\"").is_err());
    }
}
