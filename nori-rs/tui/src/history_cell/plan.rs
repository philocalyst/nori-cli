//! The agent's plan: `◈ Plan · 2/5` and one row per step, the step in progress the only bright
//! one. The same rows fill the plan cell in the transcript and the pinned drawer.

use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;

use super::HistoryCell;
use super::chrome::Chrome;
use super::chrome::GUTTER;
use super::chrome::Header;
use super::chrome::Target;
use crate::ui_types::PlanItem;
use crate::ui_types::PlanUpdate;
use crate::ui_types::StepStatus;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

/// Plan steps as styled rows; `explanation` (the agent's note on the change) comes first, quiet.
pub(crate) fn render_plan_lines(
    explanation: Option<&str>,
    plan: &[PlanItem],
    width: u16,
) -> Vec<Line<'static>> {
    let chrome = Chrome::live();
    let width = usize::from(width);
    let done = plan
        .iter()
        .filter(|item| matches!(item.status, StepStatus::Completed))
        .count();
    let mut lines = vec![chrome.header(
        Header {
            glyph: Span::styled("◈", chrome.violet()),
            verb: "Plan".to_owned(),
            target: Target::None,
            chip: Vec::new(),
            meta: if plan.is_empty() {
                Vec::new()
            } else {
                vec![chrome.fact(format!("{done}/{}", plan.len()))]
            },
        },
        width,
    )];

    let hang = |marker: Span<'static>| {
        RtOptions::new(width.saturating_sub(1).max(GUTTER + 2))
            .initial_indent(Line::from(vec!["  ".into(), marker, " ".into()]))
            .subsequent_indent("    ".into())
    };
    let note = explanation.map(str::trim).filter(|text| !text.is_empty());
    if let Some(note) = note {
        let quiet = Line::from(Span::styled(
            note.to_owned(),
            chrome.muted().add_modifier(Modifier::ITALIC),
        ));
        lines.extend(word_wrap_lines(
            [quiet],
            RtOptions::new(width.saturating_sub(1).max(GUTTER + 2))
                .initial_indent("  ".into())
                .subsequent_indent("  ".into()),
        ));
    }
    if plan.is_empty() {
        lines.push(Line::from(vec![
            "  ".into(),
            Span::styled("no steps provided", chrome.faint()),
        ]));
    }
    for PlanItem { step, status } in plan {
        let (marker, style) = match status {
            StepStatus::Completed => (
                Span::styled("✓", chrome.mint()),
                chrome.faint().add_modifier(Modifier::CROSSED_OUT),
            ),
            StepStatus::InProgress => (Span::styled("▸", chrome.cyan()), chrome.strong()),
            StepStatus::Pending => (Span::styled("○", chrome.faint()), chrome.muted()),
        };
        lines.extend(word_wrap_lines(
            [Line::from(Span::styled(step.clone(), style))],
            hang(marker),
        ));
    }
    lines
}

/// A plan update as it appears in the transcript. When the plan only moved on from `previous`
/// (the same steps, some of them in a new state), only the steps that moved are drawn under the
/// new count: the plan is not printed again and again.
pub(crate) fn new_plan_update(update: PlanUpdate, previous: Option<&PlanUpdate>) -> PlanUpdateCell {
    let PlanUpdate { explanation, plan } = update;
    let moved = previous
        .filter(|previous| {
            previous.plan.len() == plan.len()
                && previous
                    .plan
                    .iter()
                    .zip(&plan)
                    .all(|(before, after)| before.step == after.step)
        })
        .map(|previous| {
            plan.iter()
                .zip(&previous.plan)
                .map(|(after, before)| after.status != before.status)
                .collect::<Vec<bool>>()
        });
    PlanUpdateCell {
        explanation,
        plan,
        moved,
    }
}

#[derive(Debug)]
pub(crate) struct PlanUpdateCell {
    explanation: Option<String>,
    plan: Vec<PlanItem>,
    /// Which steps changed state since the plan last shown, when it is the same plan.
    moved: Option<Vec<bool>>,
}

impl HistoryCell for PlanUpdateCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(moved) = &self.moved else {
            return render_plan_lines(self.explanation.as_deref(), &self.plan, width);
        };
        let shown: Vec<PlanItem> = self
            .plan
            .iter()
            .zip(moved)
            .filter(|(_, moved)| **moved)
            .map(|(item, _)| item.clone())
            .collect();
        let mut lines = render_plan_lines(self.explanation.as_deref(), &shown, width);
        // The count is of the whole plan, not of the steps shown.
        let chrome = Chrome::live();
        let done = self
            .plan
            .iter()
            .filter(|item| matches!(item.status, StepStatus::Completed))
            .count();
        lines[0] = chrome.header(
            Header {
                glyph: Span::styled("◈", chrome.violet()),
                verb: "Plan".to_owned(),
                target: Target::None,
                chip: Vec::new(),
                meta: vec![chrome.fact(format!("{done}/{}", self.plan.len()))],
            },
            usize::from(width),
        );
        if shown.is_empty() {
            lines.truncate(1);
        }
        lines
    }
}
