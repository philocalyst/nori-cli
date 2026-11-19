//! Running a full-text query over the catalog: bloom filters prune, digests verify and score.

use std::collections::HashSet;

use super::Catalog;
use super::CatalogEntry;
use super::digest::DigestTurn;
use super::digest::TurnRole;
use super::digest::digest_path;
use super::digest::read_digest;

/// Words shorter than this are not searched on disk; the picker's in-memory fields cover them.
pub const MIN_TERM_CHARS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearch {
    /// Every term must occur somewhere in a session for it to match. A term without an
    /// uppercase letter matches regardless of case.
    pub terms: Vec<String>,
    /// Only these sessions (by id) are searched; `None` searches all.
    pub scope: Option<HashSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHit {
    pub session_id: String,
    pub project_id: String,
    pub score: i32,
    /// Turns that contain at least one term.
    pub turns_matched: i64,
    /// The turn that matches best, for the preview to open on.
    pub best_turn: i64,
    /// One line of context around the first match in the best turn.
    pub snippet: String,
}

/// Threads a search spreads its digest reads over, at most.
const MAX_WORKERS: usize = 8;

const BASE_SCORE: i32 = 40;
const PER_TURN: i32 = 3;
const TURN_CAP: i64 = 10;
const PER_TERM_IN_TURN: i32 = 5;
const USER_TURN_BONUS: i32 = 6;
/// All terms adjacent, in order, in one turn.
const PHRASE_BONUS: i32 = 20;

/// A search term with its folded form, prepared once per search.
struct Term<'a> {
    text: &'a str,
    folded: String,
    /// Smart case: a term with an uppercase letter is matched exactly.
    exact: bool,
    chars: usize,
}

impl<'a> Term<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            folded: text.to_lowercase(),
            exact: text.chars().any(char::is_uppercase),
            chars: text.chars().count(),
        }
    }

    /// The character position of the first occurrence in a turn, given the turn's text and its
    /// lowercase form.
    fn find(&self, text: &str, folded: &str) -> Option<usize> {
        let (haystack, needle) = if self.exact {
            (text, self.text)
        } else {
            (folded, self.folded.as_str())
        };
        haystack
            .find(needle)
            .map(|at| haystack[..at].chars().count())
    }
}

fn snippet(text: &str, at: usize, length: usize) -> String {
    const BEFORE: usize = 28;
    const AFTER: usize = 44;
    let characters: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(BEFORE);
    let end = (at + length + AFTER).min(characters.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(characters[start.min(end)..end].iter());
    if end < characters.len() {
        out.push('…');
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn turn_text(turn: &DigestTurn) -> String {
    [
        Some(&turn.text),
        turn.command.as_ref(),
        turn.output.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(String::as_str)
    .collect::<Vec<_>>()
    .join("\n")
}

pub(super) fn run(
    catalog: &Catalog,
    root: &std::path::Path,
    search: &SessionSearch,
) -> Vec<SessionHit> {
    let terms: Vec<&str> = search
        .terms
        .iter()
        .map(String::as_str)
        .filter(|term| term.chars().count() >= MIN_TERM_CHARS)
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let prepared: Vec<Term<'_>> = terms.iter().map(|term| Term::new(term)).collect();
    let candidates: Vec<&CatalogEntry> = catalog
        .entries()
        .map(AsRef::as_ref)
        .filter(|entry| {
            search
                .scope
                .as_ref()
                .is_none_or(|scope| scope.contains(&entry.header.session_id))
                && terms.iter().all(|term| entry.bloom.may_contain(term))
        })
        .collect();
    // Reading and scanning digests is the cost; spread it over the cores.
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(MAX_WORKERS);
    let chunk = candidates.len().div_ceil(workers).max(1);
    let mut hits = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for entries in candidates.chunks(chunk) {
            let prepared = &prepared;
            handles.push(scope.spawn(move || {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let header = &entry.header;
                        let path = digest_path(root, &header.project_id, &header.session_id);
                        let digest = read_digest(&path).ok()?;
                        score(
                            &digest.turns,
                            prepared,
                            &header.session_id,
                            &header.project_id,
                        )
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for handle in handles {
            hits.extend(handle.join().unwrap_or_default());
        }
    });
    hits
}

fn score(
    turns: &[DigestTurn],
    terms: &[Term<'_>],
    session_id: &str,
    project_id: &str,
) -> Option<SessionHit> {
    let mut found = vec![false; terms.len()];
    let mut matched: i64 = 0;
    let mut best: Option<(i32, usize, usize, usize)> = None;
    let phrase = (terms.len() > 1).then(|| {
        let words: Vec<&str> = terms.iter().map(|term| term.text).collect();
        words.join(" ")
    });
    let phrase = phrase.as_deref().map(Term::new);
    let phrase_chars = phrase.as_ref().map_or(0, |phrase| phrase.chars);
    for (index, turn) in turns.iter().enumerate() {
        let text = turn_text(turn);
        let folded = text.to_lowercase();
        let mut in_turn = 0;
        let mut first: Option<(usize, usize)> = None;
        for (slot, term) in terms.iter().enumerate() {
            if let Some(at) = term.find(&text, &folded) {
                found[slot] = true;
                in_turn += 1;
                if first.is_none_or(|(earliest, _)| at < earliest) {
                    first = Some((at, term.chars));
                }
            }
        }
        let Some((at, length)) = first else {
            continue;
        };
        matched += 1;
        // The words together, in order, in one turn: that is what was searched for.
        let phrase_at = phrase
            .as_ref()
            .and_then(|phrase| phrase.find(&text, &folded));
        let (at, length) = phrase_at.map_or((at, length), |at| (at, phrase_chars));
        let quality = in_turn * PER_TERM_IN_TURN
            + if phrase_at.is_some() { PHRASE_BONUS } else { 0 }
            + if turn.role == TurnRole::User {
                USER_TURN_BONUS
            } else {
                0
            };
        if best.is_none_or(|(top, ..)| quality > top) {
            best = Some((quality, index, at, length));
        }
    }
    if !found.iter().all(|present| *present) {
        return None;
    }
    let (quality, index, at, length) = best?;
    Some(SessionHit {
        session_id: session_id.to_owned(),
        project_id: project_id.to_owned(),
        score: BASE_SCORE + PER_TURN * i32::try_from(matched.min(TURN_CAP)).unwrap_or(0) + quality,
        turns_matched: matched,
        best_turn: i64::try_from(index).unwrap_or(0),
        snippet: snippet(&turn_text(&turns[index]), at, length),
    })
}
