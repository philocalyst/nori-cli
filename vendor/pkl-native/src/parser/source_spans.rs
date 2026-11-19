//! Opt-in source locations for structural editing without evaluating a module.

use std::ops::Range;
use std::sync::Arc;

use super::Entry;
use super::Module;
use super::Parser;
use super::Property;
use super::Token;
use super::TokenKind;
use crate::error::Result;

#[derive(Clone, Debug)]
pub struct PropertySource {
    pub property: Arc<Property>,
    pub declaration_span: Range<usize>,
    pub value_span: Option<Range<usize>>,
    /// The source used `property { body }`, rather than `property = value`.
    pub body_only: bool,
}

#[derive(Clone, Debug)]
pub struct ParsedSource {
    pub module: Module,
    pub properties: Vec<PropertySource>,
}

/// Parse with the same grammar and checks as evaluation, retaining exact spans.
/// Ordinary evaluation does not allocate this source-location table.
pub fn parse_named_spanned(tokens: &[Token], source: &str, name: &str) -> Result<ParsedSource> {
    let mut parser = Parser::new(tokens, source, name);
    parser.property_sources = Some(Vec::new());
    let module = parser.parse_module()?;
    Ok(ParsedSource {
        module,
        properties: parser.property_sources.take().unwrap_or_default(),
    })
}

impl Parser<'_> {
    pub(super) fn property_value_start(&self) -> Option<usize> {
        match self.peek() {
            TokenKind::Equals => self.tokens[self.pos + 1..]
                .iter()
                .find(|token| !matches!(token.kind, TokenKind::Semicolon))
                .map(|token| token.offset),
            TokenKind::LBrace => Some(self.peek_tok().offset),
            _ => None,
        }
    }

    pub(super) fn property_entry(
        &mut self,
        property: Property,
        declaration_start: usize,
        value_start: Option<usize>,
        body_only: bool,
    ) -> Entry {
        let property = Arc::new(property);
        if let Some(sources) = &mut self.property_sources {
            sources.push(PropertySource {
                property: Arc::clone(&property),
                declaration_span: declaration_start..self.prev_end,
                value_span: value_start.map(|start| start..self.prev_end),
                body_only,
            });
        }
        if let Some(log) = &mut self.span_log {
            log.push(PropertySource {
                property: Arc::clone(&property),
                declaration_span: declaration_start..self.prev_end,
                value_span: value_start.map(|start| start..self.prev_end),
                body_only,
            });
        }
        Entry::Property(property)
    }
}
