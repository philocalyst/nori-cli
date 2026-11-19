//! Caller-owned picker state: the items, the query, the ranked matches, the selection, and the
//! preview cache. Nothing here touches a terminal, and nothing runs per frame: searching
//! happens when the query or the items change, drawing only reads the result.

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use nucleo_matcher::pattern::Pattern;
use ratatui::layout::Rect;

use super::frame::FrameLayout;
use super::item::Field;
use super::item::HelixItem;
use super::item::Highlights;
use super::item::Hit;
use super::preview::Preview;
use super::preview::PreviewRequest;
use super::query::FilterKey;
use super::query::Query;
use super::score;
use super::score::Scorer;

/// Static description of one picker.
#[derive(Debug, Clone)]
pub struct HelixConfig {
    /// The word on the top edge: `Sessions`, `Actors`.
    pub title: String,
    /// Shown in the prompt while the query is empty.
    pub placeholder: String,
    /// What the list says when it is empty (no query).
    pub empty_text: String,
    /// What the list says when the query matches nothing.
    pub no_match_text: String,
    pub filter_keys: &'static [FilterKey],
    /// Whether the preview pane exists at all (and `Tab` toggles it).
    pub preview: bool,
    /// Rows carry a selection gutter; `Space` (with an empty query) toggles the selected one
    /// and Enter submits all that are toggled (`HelixEvent::SubmitMany`).
    pub multi: bool,
    /// Typing edits a query. When false the picker has no query: plain keys are row actions
    /// (`ItemAction::plain`), `j` and `k` move, and the prompt row shows the placeholder.
    pub searchable: bool,
    /// With `searchable`: the query starts closed. `/`, `f` or Ctrl-F opens it, Esc closes it
    /// (clearing it), and while it is closed `j` and `k` move and digits choose.
    pub on_demand: bool,
    /// While no query is being typed, a digit chooses the row of that number (1 is the first
    /// enabled row).
    pub numbered: bool,
    /// A line of context shown on the bottom edge when there is no other status.
    pub subtitle: Option<String>,
}

/// One row of [`HelixState::visible`].
pub struct VisibleRow<'a, I> {
    pub item: &'a I,
    pub selected: bool,
    /// 1-based position among the enabled rows; `None` for a disabled row.
    pub number: Option<usize>,
    /// The section this row is the first of, while the list is unfiltered.
    pub opens_section: Option<&'a str>,
}

/// An action the host binds to `Alt+<key>` on the selected item. With `compose` the prompt
/// first collects a line of text (`HelixEvent::Composed`) and `compose` is the verb shown
/// beside it: `message luna-wake ❯ …`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemAction {
    pub name: &'static str,
    pub key: char,
    pub compose: Option<&'static str>,
    /// Bound to the plain key (only in a picker that is not `searchable`) instead of `Alt+key`.
    pub plain: bool,
}

/// The prompt while it collects text for an action instead of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Compose<Id> {
    pub(super) action: &'static str,
    pub(super) id: Id,
    pub(super) verb: &'static str,
    pub(super) target: String,
    pub(super) text: String,
}

/// Asked of the host after the text of the query changed: search full text elsewhere (an index
/// on disk) and answer with `HelixState::set_hits` carrying the same `generation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub generation: i64,
    pub terms: Vec<String>,
    pub filters: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone)]
pub(super) struct Match {
    pub(super) index: usize,
    pub(super) score: i32,
    pub(super) highlights: Highlights,
    pub(super) hit: Option<Hit>,
}

pub struct HelixState<I: HelixItem> {
    pub(super) config: HelixConfig,
    pub(super) items: Vec<I>,
    pub(super) query: String,
    pub(super) parsed: Query,
    patterns: Vec<Pattern>,
    pub(super) terms: Vec<String>,
    pub(super) matches: Vec<Match>,
    pub(super) selected: Option<I::Id>,
    pub(super) scroll_top: usize,
    /// Rows of the list at the last frame, for paging.
    pub(super) page: usize,
    dirty: bool,
    generation: i64,
    requested: i64,
    hits: HashMap<I::Id, Hit>,
    pub(super) status: Option<String>,
    pub(super) search_time: Option<Duration>,
    pub(super) preview: Option<Preview<I::Id>>,
    pub(super) preview_scroll: isize,
    /// The match in the preview the user jumped to, if they did.
    pub(super) preview_hit: Option<usize>,
    /// Preview lines containing a query term, cached per `(query, preview build)`.
    pub(super) match_lines: Vec<usize>,
    pub(super) match_for: Option<(i64, Instant)>,
    pub(super) preview_hidden: bool,
    pub(super) actions: Vec<ItemAction>,
    /// A query is being typed in an `on_demand` picker.
    pub(super) searching: bool,
    pub(super) compose: Option<Compose<I::Id>>,
    pub(super) checked: Vec<I::Id>,
    pub(super) layout: Option<FrameLayout>,
    /// `(match index, first row, height)` of every row drawn at the last frame.
    pub(super) drawn: Vec<(usize, u16, u16)>,
    scorer: Scorer,
}

impl<I: HelixItem> HelixState<I> {
    pub fn new(config: HelixConfig) -> Self {
        Self {
            config,
            items: Vec::new(),
            query: String::new(),
            parsed: Query::default(),
            patterns: Vec::new(),
            terms: Vec::new(),
            matches: Vec::new(),
            selected: None,
            scroll_top: 0,
            page: 8,
            dirty: false,
            generation: 0,
            requested: 0,
            hits: HashMap::new(),
            status: None,
            search_time: None,
            preview: None,
            preview_scroll: 0,
            preview_hit: None,
            match_lines: Vec::new(),
            match_for: None,
            preview_hidden: false,
            actions: Vec::new(),
            searching: false,
            compose: None,
            checked: Vec::new(),
            layout: None,
            drawn: Vec::new(),
            scorer: Scorer::default(),
        }
    }

    pub fn actions(mut self, actions: impl IntoIterator<Item = ItemAction>) -> Self {
        self.actions = actions.into_iter().collect();
        self
    }

    pub fn items(&self) -> &[I] {
        &self.items
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Number of items passing the query.
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub fn selected(&self) -> Option<&I> {
        let id = self.selected?;
        self.items.iter().find(|item| item.id() == id)
    }

    pub fn selected_position(&self) -> Option<usize> {
        let id = self.selected?;
        self.matches
            .iter()
            .position(|found| self.items[found.index].id() == id)
    }

    pub(super) fn generation_key(&self) -> i64 {
        self.generation
    }

    /// Close the query of an `on_demand` picker (the caller clears it).
    pub fn stop_searching(&mut self) {
        self.searching = false;
    }

    /// Whether an `on_demand` picker has its query open.
    pub fn searching(&self) -> bool {
        self.searching
    }

    /// The rows as the list shows them: the matches in order, each with its number among the
    /// enabled rows and the section it opens, while the list is unfiltered. For a host that
    /// paints rows itself. Call `refresh` after changing items.
    pub fn visible(&self) -> Vec<VisibleRow<'_, I>> {
        let mut number = 0;
        let mut previous: Option<&str> = None;
        let selected = self.selected;
        self.matches
            .iter()
            .map(|found| {
                let item = &self.items[found.index];
                if item.enabled() {
                    number += 1;
                }
                let section = item.section();
                let opens = if self.parsed.is_empty() && section != previous {
                    section
                } else {
                    None
                };
                previous = section;
                VisibleRow {
                    item,
                    selected: selected == Some(item.id()),
                    number: item.enabled().then_some(number),
                    opens_section: opens,
                }
            })
            .collect()
    }

    /// The lowercase text terms of the query.
    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// Words shown on the bottom edge, left: `indexing 12 / 98`.
    pub fn set_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    /// The items toggled in a multi-select picker, in the order they were toggled.
    pub fn checked(&self) -> &[I::Id] {
        &self.checked
    }

    pub fn set_checked(&mut self, checked: Vec<I::Id>) {
        self.checked = checked;
    }

    /// Ask for a line of text for `action` on `id`, as if its key had been pressed. `label` is
    /// what the prompt names (beside the verb) and `initial` the text it starts with.
    pub fn begin_compose(
        &mut self,
        action: &'static str,
        verb: &'static str,
        label: String,
        initial: String,
        id: I::Id,
    ) {
        self.compose = Some(Compose {
            action,
            id,
            verb,
            target: label,
            text: initial,
        });
    }

    /// Whether the prompt is collecting text for an action.
    pub fn composing(&self) -> bool {
        self.compose.is_some()
    }

    pub fn select(&mut self, id: I::Id) {
        if self.items.iter().any(|item| item.id() == id) {
            self.set_selected(Some(id));
        }
    }

    /// Replace every item. The selection stays on its item when that survives. When nothing a
    /// search looks at changed, the ranking is kept and only the rows are refreshed.
    pub fn set_items(&mut self, items: Vec<I>) {
        let unchanged = items.len() == self.items.len()
            && items
                .iter()
                .zip(&self.items)
                .all(|(new, old)| new.id() == old.id() && new.search_identity_eq(old));
        self.items = items;
        if !unchanged {
            self.matches.clear();
            self.dirty = true;
        }
    }

    /// Replace the item with the same id, or add `item` at the end.
    pub fn upsert(&mut self, item: I) {
        match self.items.iter().position(|old| old.id() == item.id()) {
            Some(position) => self.items[position] = item,
            None => self.items.push(item),
        }
        self.dirty = true;
    }

    pub fn remove(&mut self, id: I::Id) {
        self.items.retain(|item| item.id() != id);
        self.hits.remove(&id);
        // Matches index into the items, which just shrank.
        self.matches.clear();
        self.dirty = true;
    }

    /// Edit the item with `id` in place and search again.
    pub fn update(&mut self, id: I::Id, edit: impl FnOnce(&mut I)) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id() == id) {
            edit(item);
            self.dirty = true;
        }
    }

    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.query_changed();
    }

    pub(super) fn query_changed(&mut self) {
        self.parsed = Query::parse(&self.query, self.config.filter_keys);
        self.terms = self
            .parsed
            .terms
            .iter()
            .map(|term| term.to_lowercase())
            .collect();
        self.patterns = self
            .parsed
            .terms
            .iter()
            .map(|term| score::pattern(term))
            .collect();
        self.generation += 1;
        self.preview_scroll = 0;
        self.preview_hit = None;
        self.dirty = true;
        // Typing starts at the best match; clearing the query leaves the selection where it is.
        if !self.query.is_empty() {
            self.selected = None;
        }
    }

    /// The full-text search the host is asked to run, once per change of the query's text.
    pub fn take_search_request(&mut self) -> Option<SearchRequest> {
        if self.requested == self.generation {
            return None;
        }
        self.requested = self.generation;
        if self.parsed.terms.is_empty() {
            self.hits.clear();
            self.dirty = true;
            return None;
        }
        Some(SearchRequest {
            generation: self.generation,
            terms: self.parsed.terms.clone(),
            filters: self
                .parsed
                .filters
                .iter()
                .filter(|filter| !filter.negated)
                .map(|filter| (filter.key, filter.value.clone()))
                .collect(),
        })
    }

    /// Full-text matches the host found. Stale answers (for an older query) are dropped.
    pub fn set_hits(&mut self, generation: i64, hits: HashMap<I::Id, Hit>) {
        if generation != self.generation {
            return;
        }
        self.hits = hits;
        self.dirty = true;
    }

    /// Make the ranking current. Called before reading matches; cheap when nothing changed.
    pub fn refresh(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let started = Instant::now();
        let previous = self.selected_position();
        self.matches = if self.parsed.is_empty() {
            (0..self.items.len())
                .map(|index| Match {
                    index,
                    score: 0,
                    highlights: Highlights::default(),
                    hit: None,
                })
                .collect()
        } else {
            self.rank()
        };
        self.search_time = (!self.parsed.is_empty()).then(|| started.elapsed());
        let survives = self.selected.is_some_and(|id| {
            self.matches
                .iter()
                .any(|found| self.items[found.index].id() == id)
        });
        if !survives {
            let at = previous
                .unwrap_or(0)
                .min(self.matches.len().saturating_sub(1));
            let fallback = if self.parsed.is_empty() { at } else { 0 };
            self.selected = self
                .matches
                .get(fallback)
                .map(|found| self.items[found.index].id());
        }
    }

    fn rank(&mut self) -> Vec<Match> {
        let mut ranked: Vec<Match> = Vec::new();
        let mut fields: Vec<Field<'_>> = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            fields.clear();
            item.fields(&mut fields);
            let passes_filters = self.parsed.filters.iter().all(|filter| {
                let found = fields
                    .iter()
                    .filter(|field| field.key == filter.key)
                    .any(|field| score::contains(field.text, &filter.value));
                found != filter.negated
            });
            if !passes_filters {
                continue;
            }
            let external = self.hits.get(&item.id());
            let mut total = 0;
            let mut highlights = Highlights::default();
            let mut hit = None;
            let mut matched = true;
            for (term, pattern) in self.parsed.terms.iter().zip(&self.patterns) {
                let mut best: Option<i32> = None;
                for field in &fields {
                    if let Some((found, indices)) =
                        self.scorer.fuzzy(pattern, term.chars().count(), field.text)
                    {
                        highlights.merge(field.key, &indices);
                        best = best.max(Some(found - field.penalty));
                    }
                }
                if best.is_none()
                    && let Some(body) = item.body()
                    && let Some((found, count, snippet)) = score::body_hit(body, term)
                {
                    best = Some(found);
                    hit.get_or_insert(Hit {
                        score: found,
                        count,
                        snippet: Some(snippet),
                    });
                }
                if best.is_none()
                    && let Some(found) = external
                {
                    best = Some(found.score);
                    hit.get_or_insert_with(|| found.clone());
                }
                match best {
                    Some(value) => total += value,
                    None => {
                        matched = false;
                        break;
                    }
                }
            }
            if matched {
                ranked.push(Match {
                    index,
                    score: total,
                    highlights,
                    hit,
                });
            }
        }
        ranked.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
        ranked
    }

    pub(super) fn set_selected(&mut self, id: Option<I::Id>) {
        if self.selected != id {
            self.selected = id;
            self.preview_scroll = 0;
            self.preview_hit = None;
        }
    }

    /// Move the selection `delta` rows. A single step wraps around the ends; a page stops.
    pub(super) fn step(&mut self, delta: isize, wrap: bool) {
        self.refresh();
        let len = isize::try_from(self.matches.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        let current = isize::try_from(self.selected_position().unwrap_or(0)).unwrap_or(0);
        let next = if wrap {
            (current + delta).rem_euclid(len)
        } else {
            (current + delta).clamp(0, len - 1)
        };
        let id = self.items[self.matches[usize::try_from(next).unwrap_or(0)].index].id();
        self.set_selected(Some(id));
    }

    pub(super) fn select_edge(&mut self, last: bool) {
        self.refresh();
        let at = if last {
            self.matches.len().saturating_sub(1)
        } else {
            0
        };
        let id = self
            .matches
            .get(at)
            .map(|found| self.items[found.index].id());
        self.set_selected(id);
    }

    /// The preview the engine lacks for the selected item at `area`, if any.
    pub fn preview_request(&mut self, area: Rect, now: Instant) -> Option<PreviewRequest<I::Id>> {
        self.refresh();
        if self.preview_hidden {
            return None;
        }
        let pane = FrameLayout::new(area, true).preview?;
        let item = self.selected()?;
        let width = pane.width.saturating_sub(super::frame::PANE_PAD + 1);
        let terms_key = u64::try_from(self.generation).unwrap_or(0);
        if self.preview.as_ref().is_some_and(|preview| {
            preview.is_current(
                item.id(),
                item.revision(),
                width,
                terms_key,
                item.is_live(),
                now,
            )
        }) {
            return None;
        }
        let scrolled_back = self.preview_scroll.unsigned_abs();
        Some(PreviewRequest {
            id: item.id(),
            width,
            rows: usize::from(pane.height) + scrolled_back,
            terms: self.terms.clone(),
            terms_key,
        })
    }

    pub fn set_preview(&mut self, preview: Preview<I::Id>) {
        self.preview = Some(preview);
    }
}
