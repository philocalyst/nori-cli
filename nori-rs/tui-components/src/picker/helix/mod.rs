//! The Helix-style picker engine: one frame, one query language, one ranking, one set of keys
//! for every list-with-preview in Nori (resume, actors, and whatever comes next).
//!
//! A picker is a [`HelixState`] over a host-defined [`HelixItem`]. The host
//!
//! 1. keeps the items current (`set_items`, `upsert`, `remove`, `update`),
//! 2. feeds input (`on_key`, `on_paste`, `on_mouse`) and acts on the returned [`HelixEvent`],
//! 3. answers what the engine lacks: full-text hits from a slower index
//!    (`take_search_request` then `set_hits`) and previews for the selection
//!    (`preview_request` then `set_preview`), both asynchronously and never from the draw path,
//! 4. calls `render`.
//!
//! Filtering, fuzzy and full-text scoring, `@agent:x %status #role` filters, selection that
//! follows an item (not an index) through changes, scrolling, the 38/62 split and the box
//! drawn around it are the engine's. Content is the host's.

mod frame;
mod highlight;
mod input;
mod item;
mod preview;
mod query;
mod render;
mod score;
mod state;

pub use frame::FrameLayout;
pub use frame::LIST_SHARE_PERCENT;
pub use frame::PREVIEW_MIN_WIDTH;
pub use highlight::highlight_indices;
pub use highlight::highlight_terms;
pub use input::HelixEvent;
pub use item::Field;
pub use item::HelixItem;
pub use item::Highlights;
pub use item::Hit;
pub use item::RowContext;
pub use preview::LIVE_REFRESH;
pub use preview::Preview;
pub use preview::PreviewAnchor;
pub use preview::PreviewRequest;
pub use query::Filter;
pub use query::FilterKey;
pub use query::Query;
pub use score::fit_snippet;
pub use score::snippet;
pub use state::HelixConfig;
pub use state::HelixState;
pub use state::ItemAction;
pub use state::SearchRequest;
pub use state::VisibleRow;

#[cfg(test)]
mod tests;
