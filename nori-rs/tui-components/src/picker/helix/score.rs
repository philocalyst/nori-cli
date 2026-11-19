//! Scoring: fuzzy matching of fields, smart-case substring search of bodies, and the one-line
//! snippets that show why a body matched.

use nucleo_matcher::Config;
use nucleo_matcher::Matcher;
use nucleo_matcher::Utf32Str;
use nucleo_matcher::pattern::AtomKind;
use nucleo_matcher::pattern::CaseMatching;
use nucleo_matcher::pattern::Normalization;
use nucleo_matcher::pattern::Pattern;

/// What a hit in a body is worth before the item's own penalty: a body match always ranks
/// behind a clean title match.
pub(super) const BODY_BASE_SCORE: i32 = 20;
/// Each further occurrence adds this much, up to [`BODY_COUNT_CAP`] occurrences.
const BODY_COUNT_BONUS: i32 = 2;
const BODY_COUNT_CAP: usize = 10;
/// A fuzzy match may span this many times the term's length, plus the slack.
const MAX_SPREAD: usize = 2;
const MAX_SLACK: usize = 3;

pub(super) struct Scorer {
    matcher: Matcher,
    haystack: Vec<char>,
    indices: Vec<u32>,
}

impl Default for Scorer {
    fn default() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            haystack: Vec::new(),
            indices: Vec::new(),
        }
    }
}

pub(super) fn pattern(term: &str) -> Pattern {
    Pattern::new(
        term,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    )
}

impl Scorer {
    /// The fuzzy score of `text` against `pattern` (a term of `term_chars` characters), with
    /// the matched character positions. Matches spread thinly across a long text (the letters
    /// of `cancel` found a few words apart) are noise, not matches, and are rejected.
    pub(super) fn fuzzy(
        &mut self,
        pattern: &Pattern,
        term_chars: usize,
        text: &str,
    ) -> Option<(i32, Vec<usize>)> {
        self.haystack.clear();
        self.indices.clear();
        let haystack = Utf32Str::new(text, &mut self.haystack);
        let score = pattern.indices(haystack, &mut self.matcher, &mut self.indices)?;
        self.indices.sort_unstable();
        self.indices.dedup();
        let span = match (self.indices.first(), self.indices.last()) {
            (Some(first), Some(last)) => (last - first + 1) as usize,
            _ => 0,
        };
        if span > term_chars * MAX_SPREAD + MAX_SLACK {
            return None;
        }
        Some((
            i32::try_from(score).unwrap_or(i32::MAX),
            self.indices.iter().map(|index| *index as usize).collect(),
        ))
    }
}

/// Smart case: a term with no uppercase letter matches regardless of case.
fn fold(term: &str) -> Option<String> {
    (!term.chars().any(char::is_uppercase)).then(|| term.to_lowercase())
}

pub(super) fn contains(text: &str, term: &str) -> bool {
    match fold(term) {
        Some(lower) => text.to_lowercase().contains(&lower),
        None => text.contains(term),
    }
}

/// The score, count and context of `term` inside `body`, if it occurs.
pub(super) fn body_hit(body: &str, term: &str) -> Option<(i32, usize, String)> {
    let (haystack, needle) = match fold(term) {
        Some(lower) => (body.to_lowercase(), lower),
        None => (body.to_owned(), term.to_owned()),
    };
    let first = haystack.find(&needle)?;
    let count = haystack.matches(&needle).take(BODY_COUNT_CAP).count();
    let bonus = i32::try_from(count).unwrap_or(0) * BODY_COUNT_BONUS;
    // Lowercasing can change byte offsets; map through the character index instead.
    let at = haystack[..first].chars().count();
    Some((
        BODY_BASE_SCORE + bonus,
        count,
        snippet(body, at, needle.chars().count()),
    ))
}

const SNIPPET_BEFORE: usize = 28;
const SNIPPET_AFTER: usize = 44;

/// One line of context around the `length` characters at character index `at`.
pub fn snippet(text: &str, at: usize, length: usize) -> String {
    let characters: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(SNIPPET_BEFORE);
    let end = (at + length + SNIPPET_AFTER).min(characters.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(
        characters[start.min(characters.len())..end]
            .iter()
            .map(|character| {
                if character.is_whitespace() {
                    ' '
                } else {
                    *character
                }
            }),
    );
    if end < characters.len() {
        out.push('…');
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `snippet` cut to `width` characters around the first occurrence of any of `terms`, with an
/// ellipsis on each cut side, so that a narrow row still shows the match.
pub fn fit_snippet(snippet: &str, terms: &[String], width: usize) -> String {
    let characters: Vec<char> = snippet.chars().collect();
    if characters.len() <= width || width < 4 {
        return snippet.chars().take(width).collect();
    }
    let folded = snippet.to_lowercase();
    let (at, length) = terms
        .iter()
        .filter(|term| !term.is_empty())
        .filter_map(|term| {
            let term = term.to_lowercase();
            folded
                .find(&term)
                .map(|byte| (folded[..byte].chars().count(), term.chars().count()))
        })
        .min()
        .unwrap_or((0, 0));
    // A third of what room the match leaves goes before it, the rest after.
    let start = at
        .saturating_sub(width.saturating_sub(length) / 3)
        .min(characters.len() - width);
    let end = start + width;
    let mut window: Vec<char> = characters[start..end].to_vec();
    if start > 0 {
        window[0] = '…';
    }
    if end < characters.len() {
        window[width - 1] = '…';
    }
    window.into_iter().collect()
}
