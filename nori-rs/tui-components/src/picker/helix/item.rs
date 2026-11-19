//! What a source of rows must tell the engine: identity, searchable fields, and how to draw.

use std::fmt::Debug;
use std::hash::Hash;

use ratatui::text::Line;

use crate::Theme;

/// One searchable piece of an item. `key` is what `@key:value` filters address; `penalty` is
/// subtracted from the fuzzy score, so a match in a secondary field ranks behind one in the
/// title (whose penalty is zero).
#[derive(Debug, Clone, Copy)]
pub struct Field<'a> {
    pub key: &'static str,
    pub text: &'a str,
    pub penalty: i32,
}

impl<'a> Field<'a> {
    pub fn new(key: &'static str, text: &'a str, penalty: i32) -> Self {
        Self { key, text, penalty }
    }
}

/// The character positions the query matched, per field key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Highlights {
    fields: Vec<(&'static str, Vec<usize>)>,
}

impl Highlights {
    /// Matched character positions in `key`, ascending and distinct.
    pub fn field(&self, key: &str) -> &[usize] {
        self.fields
            .iter()
            .find(|(name, _)| *name == key)
            .map_or(&[], |(_, indices)| indices.as_slice())
    }

    pub(super) fn merge(&mut self, key: &'static str, indices: &[usize]) {
        let position = match self.fields.iter().position(|(name, _)| *name == key) {
            Some(position) => position,
            None => {
                self.fields.push((key, Vec::new()));
                self.fields.len() - 1
            }
        };
        let slot = &mut self.fields[position].1;
        slot.extend_from_slice(indices);
        slot.sort_unstable();
        slot.dedup();
    }
}

/// Where a full-text query matched inside an item that its fields did not explain: how often,
/// and one line of context around the first occurrence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hit {
    pub score: i32,
    pub count: usize,
    pub snippet: Option<String>,
}

/// Everything an item needs to draw its rows.
pub struct RowContext<'a> {
    /// Columns available to each row (the engine has already taken the selection marker).
    pub width: u16,
    pub selected: bool,
    /// A query is active, so rows are ranked rather than in natural order. Tree glyphs and
    /// other position-dependent decoration belong to the unfiltered view only.
    pub filtered: bool,
    pub theme: &'a Theme,
    pub highlights: &'a Highlights,
    pub hit: Option<&'a Hit>,
    /// The lowercase text terms of the query, for highlighting free text.
    pub terms: &'a [String],
    /// Animation phase, in steps of the caller's choosing.
    pub tick: i64,
}

/// A row source. Items are plain data the host keeps current; the engine owns filtering,
/// ordering, selection, scrolling and drawing.
pub trait HelixItem {
    type Id: Copy + Eq + Hash + Debug;

    fn id(&self) -> Self::Id;

    /// Append the searchable fields, the title first.
    fn fields<'a>(&'a self, out: &mut Vec<Field<'a>>);

    /// The item's name where a sentence needs one (`message luna-wake`): its title field.
    fn title_text(&self) -> String {
        let mut fields = Vec::new();
        self.fields(&mut fields);
        fields
            .first()
            .map(|field| field.text.to_owned())
            .unwrap_or_default()
    }

    /// Full text held in memory, searched by substring after the fields. Items whose text lives
    /// on disk leave this empty and have hits delivered with `HelixState::set_hits`.
    fn body(&self) -> Option<&str> {
        None
    }

    /// The section this item belongs to. While the list is unfiltered, a heading is drawn
    /// above each run of items of one section.
    fn section(&self) -> Option<&str> {
        None
    }

    /// Choosing this item first asks for a line of text (the verb beside the prompt) instead
    /// of submitting it; see `HelixEvent::Composed`.
    fn asks(&self) -> Option<&'static str> {
        None
    }

    /// Whether choosing this item does anything. A disabled item is listed (with whatever
    /// reason its row gives) and can be highlighted, but Enter leaves it alone.
    fn enabled(&self) -> bool {
        true
    }

    /// Terminal rows this item takes in the list.
    fn row_height(&self) -> u16 {
        1
    }

    /// Exactly [`Self::row_height`] lines.
    fn rows(&self, cx: &RowContext<'_>) -> Vec<Line<'static>>;

    /// Changes whenever what the preview shows changes.
    fn revision(&self) -> i64 {
        0
    }

    /// The preview of this item changes without a new revision (a running agent's output).
    fn is_live(&self) -> bool {
        false
    }

    /// Whether everything the search looks at is the same as in `other`, so that a refreshed
    /// item list needs no new search, only fresher times and counts.
    fn search_identity_eq(&self, _other: &Self) -> bool {
        false
    }
}
