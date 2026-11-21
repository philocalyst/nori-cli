//! Lazy, per-cell layout for the visible tail of a focused conversation.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;

use ratatui::text::Line;

use crate::chatwidget::tool_projection::ToolProjection;
use crate::chatwidget::tool_projection::ToolSpan;
use crate::history_cell::HistoryCell;

/// Keep cached layouts bounded even after the user traverses a very long transcript.
const MAX_CACHED_CELLS: usize = 512;

pub(super) enum Version {
    Main(Arc<dyn HistoryCell>),
    Child(i64),
    Metadata(String),
    Output(i64),
}

impl Version {
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Main(a), Self::Main(b)) => Arc::ptr_eq(a, b),
            (Self::Child(a), Self::Child(b)) => a == b,
            (Self::Metadata(a), Self::Metadata(b)) => a == b,
            (Self::Output(a), Self::Output(b)) => a == b,
            (Self::Main(_), Self::Child(_) | Self::Metadata(_) | Self::Output(_))
            | (Self::Child(_), Self::Main(_) | Self::Metadata(_) | Self::Output(_))
            | (Self::Metadata(_), Self::Main(_) | Self::Child(_) | Self::Output(_))
            | (Self::Output(_), Self::Main(_) | Self::Child(_) | Self::Metadata(_)) => false,
        }
    }
}

struct CachedCell {
    version: Version,
    projection: ToolProjection,
    continuation: bool,
}

#[derive(Default)]
pub(super) struct TranscriptWindow {
    width: Option<u16>,
    expanded: HashSet<String>,
    cells: HashMap<usize, CachedCell>,
    order: VecDeque<usize>,
    source: Option<Version>,
    source_count: usize,
    /// Cumulative row counts, from the newest indexed cell toward the oldest.
    ends: Vec<usize>,
}

/// Which rows of a transcript of `count` cells to lay out at `width`: `rows` rows starting
/// `offset` rows above the newest line.
#[derive(Clone, Copy)]
pub(super) struct Viewport {
    pub(super) count: usize,
    pub(super) width: u16,
    pub(super) rows: usize,
    pub(super) offset: usize,
}

pub(super) struct Window {
    pub(super) projection: ToolProjection,
    /// Only known after scanning all the way to the start.
    pub(super) maximum_offset: Option<usize>,
}

impl TranscriptWindow {
    pub(super) fn invalidate(&mut self, index: usize) {
        self.cells.remove(&index);
        self.order.retain(|item| *item != index);
        self.ends.clear();
    }

    fn insert(&mut self, index: usize, cell: CachedCell) {
        self.order.retain(|item| *item != index);
        if !self.cells.contains_key(&index)
            && self.cells.len() >= MAX_CACHED_CELLS
            && let Some(oldest) = self.order.pop_front()
        {
            self.cells.remove(&oldest);
        }
        self.order.push_back(index);
        self.cells.insert(index, cell);
    }

    pub(super) fn render(
        &mut self,
        viewport: Viewport,
        source: Version,
        expanded: &HashSet<String>,
        mut version: impl FnMut(usize) -> Option<Version>,
        mut project: impl FnMut(usize) -> Option<(ToolProjection, bool)>,
    ) -> Window {
        let Viewport {
            count,
            width,
            rows,
            offset,
        } = viewport;
        if self.width != Some(width) {
            self.cells.clear();
            self.order.clear();
            self.ends.clear();
            self.width = Some(width);
            self.expanded.clone_from(expanded);
        } else if self.expanded != *expanded {
            let changed: HashSet<_> = self.expanded.symmetric_difference(expanded).collect();
            self.cells.retain(|_, cell| {
                !cell
                    .projection
                    .spans
                    .iter()
                    .any(|span| changed.iter().any(|id| id.as_str() == span.call_id))
            });
            let cells = &self.cells;
            self.order.retain(|index| cells.contains_key(index));
            self.ends.clear();
            self.expanded.clone_from(expanded);
        }
        if self.source_count != count
            || !self
                .source
                .as_ref()
                .is_some_and(|previous| previous.matches(&source))
        {
            if self.source_count != count && matches!(&source, Version::Metadata(_)) {
                // Child parts precede nested cells: an insertion can shift every later index.
                self.cells.clear();
                self.order.clear();
            } else if count < self.source_count {
                self.cells.retain(|index, _| *index < count);
                self.order.retain(|index| *index < count);
            }
            self.source_count = count;
            self.source = Some(source);
            self.ends.clear();
        }
        let mut fragments = Vec::new();
        let top = offset.saturating_add(rows);
        while self.ends.len() < count && self.ends.last().copied().unwrap_or(0) < top {
            let index = count - self.ends.len() - 1;
            let Some(current) = version(index) else {
                self.ends.push(self.ends.last().copied().unwrap_or(0));
                continue;
            };
            if !self
                .cells
                .get(&index)
                .is_some_and(|cell| cell.version.matches(&current))
            {
                let Some((projection, continuation)) = project(index) else {
                    self.ends.push(self.ends.last().copied().unwrap_or(0));
                    continue;
                };
                self.insert(
                    index,
                    CachedCell {
                        version: current,
                        projection,
                        continuation,
                    },
                );
            }
            let cell = &self.cells[&index];
            let separator = usize::from(index > 0 && !cell.continuation);
            let length = cell.projection.lines.len().saturating_add(separator);
            self.ends.push(
                self.ends
                    .last()
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(length),
            );
        }
        let start_index = self.ends.partition_point(|end| *end <= offset);
        let end_index = self
            .ends
            .partition_point(|end| *end < top)
            .saturating_add(1)
            .min(self.ends.len());
        for position in start_index..end_index {
            let index = count - position - 1;
            if !self.cells.contains_key(&index) {
                let Some(current) = version(index) else {
                    continue;
                };
                let Some((projection, continuation)) = project(index) else {
                    continue;
                };
                self.insert(
                    index,
                    CachedCell {
                        version: current,
                        projection,
                        continuation,
                    },
                );
            }
            let Some(cell) = self.cells.get(&index) else {
                continue;
            };
            let separator = usize::from(index > 0 && !cell.continuation);
            let length = cell.projection.lines.len().saturating_add(separator);
            let seen = position
                .checked_sub(1)
                .map_or(0, |previous| self.ends[previous]);
            let end = seen.saturating_add(length);
            if end > offset && seen < top {
                let start_in_cell = length.saturating_sub(top.saturating_sub(seen).min(length));
                let end_in_cell = length.saturating_sub(offset.saturating_sub(seen).min(length));
                let mut lines = Vec::new();
                if separator == 1 && start_in_cell == 0 {
                    lines.push(Line::default());
                }
                let start_line = start_in_cell.saturating_sub(separator);
                let end_line = end_in_cell
                    .saturating_sub(separator)
                    .min(cell.projection.lines.len());
                lines.extend_from_slice(&cell.projection.lines[start_line..end_line]);
                let leading = usize::from(separator == 1 && start_in_cell == 0);
                let spans = cell
                    .projection
                    .spans
                    .iter()
                    .filter_map(|span| {
                        let start = span.start.max(start_line);
                        let end = span.end.min(end_line);
                        (start < end).then(|| ToolSpan {
                            start: leading + start - start_line,
                            end: leading + end - start_line,
                            call_id: span.call_id.clone(),
                        })
                    })
                    .collect::<Vec<_>>();
                fragments.push((lines, spans));
            }
        }
        let mut projection = ToolProjection::default();
        for (lines, spans) in fragments.into_iter().rev() {
            let base = projection.lines.len();
            projection.lines.extend(lines);
            projection.spans.extend(spans.into_iter().map(|mut span| {
                span.start += base;
                span.end += base;
                span
            }));
        }
        Window {
            projection,
            maximum_offset: (self.ends.len() == count)
                .then_some(self.ends.last().copied().unwrap_or(0).saturating_sub(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::cell::Cell;

    #[test]
    fn visible_work_is_bounded_by_viewport_for_one_or_ten_thousand_cells() {
        for count in [1_000, 10_000] {
            let mut cache = TranscriptWindow::default();
            let calls = Cell::new(0);
            let expanded = HashSet::new();
            let mut render = |width, rows| {
                cache.render(
                    Viewport {
                        count,
                        width,
                        rows,
                        offset: 0,
                    },
                    Version::Child(1),
                    &expanded,
                    |index| Some(Version::Child(i64::try_from(index).unwrap())),
                    |index| {
                        calls.set(calls.get() + 1);
                        Some((
                            ToolProjection {
                                lines: vec![Line::from(format!("你好 {index}"))],
                                spans: Vec::new(),
                            },
                            false,
                        ))
                    },
                )
            };
            let first = render(80, 24);
            assert_eq!(first.projection.lines.len(), 24);
            assert!(calls.get() <= 24);
            let cold_calls = calls.get();
            let warm = render(80, 24);
            assert_eq!(warm.projection.lines, first.projection.lines);
            assert_eq!(calls.get(), cold_calls);
            let resized = render(122, 43);
            assert_eq!(resized.projection.lines.len(), 43);
            assert!(calls.get() - cold_calls <= 43);
        }
    }

    #[test]
    fn window_clips_tool_spans_and_preserves_scroll_position() {
        let mut cache = TranscriptWindow::default();
        let expanded = HashSet::new();
        let window = cache.render(
            Viewport {
                count: 2,
                width: 30,
                rows: 3,
                offset: 1,
            },
            Version::Child(1),
            &expanded,
            |index| Some(Version::Child(i64::try_from(index).unwrap())),
            |index| {
                Some((
                    ToolProjection {
                        lines: vec![
                            Line::from(format!("{index}a")),
                            Line::from(format!("{index}b")),
                        ],
                        spans: vec![ToolSpan {
                            start: 0,
                            end: 2,
                            call_id: format!("tool-{index}"),
                        }],
                    },
                    false,
                ))
            },
        );
        let text: Vec<_> = window
            .projection
            .lines
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(text, ["0b", "", "1a"]);
        assert_eq!(
            window
                .projection
                .spans
                .iter()
                .map(|span| (span.start, span.end, span.call_id.as_str()))
                .collect::<Vec<_>>(),
            [(0, 1, "tool-0"), (2, 3, "tool-1")]
        );
    }

    #[test]
    fn deep_scroll_uses_the_row_index_after_the_first_visit() {
        let mut cache = TranscriptWindow::default();
        let versions = Cell::new(0);
        let layouts = Cell::new(0);
        let expanded = HashSet::new();
        let mut render = || {
            cache.render(
                Viewport {
                    count: 10_000,
                    width: 80,
                    rows: 24,
                    offset: 16_000,
                },
                Version::Child(7),
                &expanded,
                |index| {
                    versions.set(versions.get() + 1);
                    Some(Version::Child(i64::try_from(index).unwrap()))
                },
                |index| {
                    layouts.set(layouts.get() + 1);
                    Some((
                        ToolProjection {
                            lines: vec![Line::from(format!("cell {index}"))],
                            spans: Vec::new(),
                        },
                        false,
                    ))
                },
            )
        };
        let first = render();
        assert_eq!(first.projection.lines.len(), 24);
        let before = (versions.get(), layouts.get());
        let second = render();
        assert_eq!(second.projection.lines, first.projection.lines);
        assert_eq!((versions.get(), layouts.get()), before);
        assert!(cache.cells.len() <= MAX_CACHED_CELLS);
    }

    #[test]
    fn changed_revision_replaces_the_cached_visible_cell() {
        let mut cache = TranscriptWindow::default();
        let expanded = HashSet::new();
        let first = cache.render(
            Viewport {
                count: 1,
                width: 80,
                rows: 10,
                offset: 0,
            },
            Version::Child(1),
            &expanded,
            |_| Some(Version::Child(1)),
            |_| {
                Some((
                    ToolProjection {
                        lines: vec![Line::from("old")],
                        spans: Vec::new(),
                    },
                    false,
                ))
            },
        );
        let second = cache.render(
            Viewport {
                count: 1,
                width: 80,
                rows: 10,
                offset: 0,
            },
            Version::Child(2),
            &expanded,
            |_| Some(Version::Child(2)),
            |_| {
                Some((
                    ToolProjection {
                        lines: vec![Line::from("new")],
                        spans: Vec::new(),
                    },
                    false,
                ))
            },
        );
        assert_eq!(first.projection.lines, [Line::from("old")]);
        assert_eq!(second.projection.lines, [Line::from("new")]);
    }

    #[test]
    fn invalidating_historical_cell_rebuilds_its_layout_even_if_tail_is_unchanged() {
        let mut cache = TranscriptWindow::default();
        let expanded = HashSet::new();
        let render = |cache: &mut TranscriptWindow, label: &'static str| {
            cache.render(
                Viewport {
                    count: 2,
                    width: 80,
                    rows: 10,
                    offset: 0,
                },
                Version::Child(1),
                &expanded,
                |index| Some(Version::Child(index as i64)),
                |index| {
                    Some((
                        ToolProjection {
                            lines: vec![Line::from(if index == 0 { label } else { "tail" })],
                            spans: Vec::new(),
                        },
                        false,
                    ))
                },
            )
        };
        let first = render(&mut cache, "Running test");
        cache.invalidate(0);
        let second = render(&mut cache, "Ran test");
        assert_eq!(
            first.projection.lines,
            [
                Line::from("Running test"),
                Line::default(),
                Line::from("tail")
            ]
        );
        assert_eq!(
            second.projection.lines,
            [Line::from("Ran test"), Line::default(), Line::from("tail")]
        );
    }

    #[allow(clippy::print_stdout)]
    #[test]
    fn warm_large_history_reports_visible_window_latency() {
        let mut cache = TranscriptWindow::default();
        let expanded = HashSet::new();
        let layouts = Cell::new(0);
        let mut render =
            || {
                cache.render(
                Viewport { count: 10_000, width: 122, rows: 43, offset: 0 },
                Version::Child(1),
                &expanded,
                |index| Some(Version::Child(i64::try_from(index).unwrap())),
                |index| {
                    layouts.set(layouts.get() + 1);
                    Some((
                        ToolProjection {
                            lines: vec![Line::from(format!(
                                "输出 {index} · a realistic message with Unicode and tool details"
                            ))],
                            spans: Vec::new(),
                        },
                        false,
                    ))
                },
            )
            };
        render();
        let cold_layouts = layouts.get();
        let start = std::time::Instant::now();
        for _ in 0..1_000 {
            render();
        }
        let elapsed = start.elapsed();
        assert_eq!(layouts.get(), cold_layouts);
        println!(
            "10k-cell 122x43 warm visible window: {:.4} ms per frame",
            elapsed.as_secs_f64()
        );
    }
}
