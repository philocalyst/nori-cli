//! The query language of the prompt: free-text terms plus structured filters.
//!
//! `auth @agent:claude @branch:feat/x %running #review`. `@key:value` filters the field `key`;
//! a bare `@value`, `%value` or `#value` filters the field the source bound to that sigil
//! (`@` agent, `%` status, `#` role for Nori's pickers). A `!` before the value negates it.
//! A token that names no known field stays ordinary text, so searching for an email address or
//! a `#hashtag` never silently matches nothing. A filter with no value yet (`@agent:` while
//! typing) constrains nothing.

use std::ops::Range;

/// A filterable field and the sigil that addresses it without a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterKey {
    pub key: &'static str,
    pub sigil: Option<char>,
}

impl FilterKey {
    pub const fn new(key: &'static str, sigil: Option<char>) -> Self {
        Self { key, sigil }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub key: &'static str,
    pub value: String,
    pub negated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Free-text terms as typed.
    pub terms: Vec<String>,
    pub filters: Vec<Filter>,
    /// Byte ranges of the input that are filters, for styling the prompt.
    pub filter_spans: Vec<Range<usize>>,
}

impl Query {
    pub fn parse(input: &str, keys: &[FilterKey]) -> Self {
        let mut query = Self::default();
        let mut start = None;
        for (offset, character) in input.char_indices() {
            match (character.is_whitespace(), start) {
                (false, None) => start = Some(offset),
                (true, Some(begin)) => {
                    query.push_token(begin..offset, input, keys);
                    start = None;
                }
                (true, None) | (false, Some(_)) => {}
            }
        }
        if let Some(begin) = start {
            query.push_token(begin..input.len(), input, keys);
        }
        query
    }

    fn push_token(&mut self, span: Range<usize>, input: &str, keys: &[FilterKey]) {
        let token = &input[span.clone()];
        match parse_filter(token, keys) {
            Some(parsed) => {
                self.filter_spans.push(span);
                if let Some(filter) = parsed {
                    self.filters.push(filter);
                }
            }
            None => self.terms.push(token.to_owned()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.filters.is_empty()
    }

    /// The text terms joined, which is what a full-text index is asked.
    pub fn text(&self) -> String {
        self.terms.join(" ")
    }
}

/// `Some(Some(filter))` for a complete filter, `Some(None)` for one with no value yet, `None`
/// when the token is not a filter at all.
fn parse_filter(token: &str, keys: &[FilterKey]) -> Option<Option<Filter>> {
    let mut characters = token.chars();
    let sigil = characters.next()?;
    let rest = characters.as_str();
    if !matches!(sigil, '@' | '%' | '#') && !keys.iter().any(|key| key.sigil == Some(sigil)) {
        return None;
    }
    let (key, value) = match rest.split_once(':') {
        Some((name, value)) => (
            keys.iter()
                .find(|key| key.key.eq_ignore_ascii_case(name))?
                .key,
            value,
        ),
        None => (keys.iter().find(|key| key.sigil == Some(sigil))?.key, rest),
    };
    let (negated, value) = match value.strip_prefix('!') {
        Some(value) => (true, value),
        None => (false, value),
    };
    Some((!value.is_empty()).then(|| Filter {
        key,
        value: value.to_owned(),
        negated,
    }))
}
