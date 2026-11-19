//! The preview pane's content: supplied by the host for the selected item, cached by the engine.
//!
//! The engine never builds a preview. It says what it lacks ([`PreviewRequest`], from
//! `HelixState::preview_request`), the host builds it from wherever the content lives (memory,
//! an index on disk, a live conversation), and hands it back ([`Preview`]). A frame in which
//! nothing changed asks for nothing.

use std::time::Duration;
use std::time::Instant;

use ratatui::text::Line;

/// A running item's preview is rebuilt at least this often.
pub const LIVE_REFRESH: Duration = Duration::from_millis(250);

/// Which line of the preview is first in view before the user scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewAnchor {
    Top,
    /// The end: what a viewer of a live conversation follows.
    Tail,
    /// The first line that contains a query term, or the top when none does.
    FirstMatch,
    Line(usize),
}

/// What the engine lacks to draw the selected item's preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest<Id> {
    pub id: Id,
    /// Columns the body will be drawn in; lay lines out for this width.
    pub width: u16,
    /// Rows of the pane plus the rows scrolled back from the tail: enough for a host that
    /// builds only the end of a long conversation.
    pub rows: usize,
    /// The lowercase text terms of the query, for choosing what to show.
    pub terms: Vec<String>,
    pub(super) terms_key: u64,
}

impl<Id: Copy> PreviewRequest<Id> {
    /// The answer to this request.
    pub fn answer(
        &self,
        revision: i64,
        title: Line<'static>,
        lines: Vec<Line<'static>>,
        anchor: PreviewAnchor,
    ) -> Preview<Id> {
        Preview {
            id: self.id,
            revision,
            width: self.width,
            terms_key: self.terms_key,
            title,
            lines,
            anchor,
            built_at: Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Preview<Id> {
    pub id: Id,
    /// The item's `revision` this was built from.
    pub revision: i64,
    pub width: u16,
    terms_key: u64,
    /// One line shown beside the prompt: who the item is.
    pub title: Line<'static>,
    pub lines: Vec<Line<'static>>,
    pub anchor: PreviewAnchor,
    pub built_at: Instant,
}

impl<Id: Copy + Eq> Preview<Id> {
    pub(super) fn is_current(
        &self,
        id: Id,
        revision: i64,
        width: u16,
        terms_key: u64,
        live: bool,
        now: Instant,
    ) -> bool {
        self.id == id
            && self.revision == revision
            && self.width == width
            && self.terms_key == terms_key
            && (!live || now.saturating_duration_since(self.built_at) < LIVE_REFRESH)
    }
}
