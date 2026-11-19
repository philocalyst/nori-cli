//! Pinned plan drawer: renders the latest plan state as a fixed panel in the
//! viewport between the active cell and the bottom pane.
//!
//! Two modes are supported:
//! - **Expanded**: full checklist (via `PinnedPlanDrawer`)
//! - **Collapsed**: single-line progress summary (via `PinnedPlanDrawerCollapsed`)

use crate::ui_types::PlanItem;
use crate::ui_types::PlanUpdate;
use crate::ui_types::StepStatus;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Header;
use crate::history_cell::chrome::Target;
use crate::history_cell::render_plan_lines;
use crate::render::renderable::Renderable;

/// A viewport-pinned widget that displays the latest plan as a checklist.
pub(crate) struct PinnedPlanDrawer<'a> {
    plan: &'a PlanUpdate,
}

impl<'a> PinnedPlanDrawer<'a> {
    pub(crate) fn new(plan: &'a PlanUpdate) -> Self {
        Self { plan }
    }
}

impl Renderable for PinnedPlanDrawer<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = render_plan_lines(
            self.plan.explanation.as_deref(),
            &self.plan.plan,
            area.width,
        );
        Paragraph::new(Text::from(lines)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let lines = render_plan_lines(self.plan.explanation.as_deref(), &self.plan.plan, width);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }
}

/// A viewport-pinned widget that displays a one-line plan progress summary.
pub(crate) struct PinnedPlanDrawerCollapsed<'a> {
    plan: &'a PlanUpdate,
}

impl<'a> PinnedPlanDrawerCollapsed<'a> {
    pub(crate) fn new(plan: &'a PlanUpdate) -> Self {
        Self { plan }
    }
}

impl Renderable for PinnedPlanDrawerCollapsed<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let line = build_collapsed_line(&self.plan.plan, area.width as usize);
        Paragraph::new(Text::from(line)).render(area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

/// The collapsed summary: `◈ Plan  ▸ the step in progress · 2/5`. With nothing in progress the
/// next pending step is shown, and a finished plan says so.
fn build_collapsed_line(plan: &[PlanItem], width: usize) -> Line<'static> {
    let chrome = Chrome::live();
    let done = plan
        .iter()
        .filter(|item| matches!(item.status, StepStatus::Completed))
        .count();
    let current = plan
        .iter()
        .find(|item| matches!(item.status, StepStatus::InProgress));
    let next = plan
        .iter()
        .find(|item| matches!(item.status, StepStatus::Pending));
    let step = match (current, next) {
        (Some(item), _) => vec![
            Span::styled("▸ ", chrome.cyan()),
            Span::styled(item.step.clone(), chrome.strong()),
        ],
        (None, Some(item)) => vec![
            Span::styled("○ ", chrome.faint()),
            Span::styled(item.step.clone(), chrome.muted()),
        ],
        (None, None) => vec![Span::styled("all done", chrome.mint())],
    };
    chrome.header(
        Header {
            glyph: Span::styled("◈", chrome.violet()),
            verb: "Plan".to_owned(),
            target: Target::Spans(step),
            chip: Vec::new(),
            meta: vec![chrome.fact(format!("{done}/{}", plan.len()))],
        },
        width,
    )
}
