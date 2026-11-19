//! What every section of the usage view reads: the summary, the metric and the theme.

use chrono::DateTime;
use chrono::Utc;
use nori_harness::usage::ModelUsage;
use nori_harness::usage::UsageSummary;
use nori_tui_components::Theme;
use ratatui::text::Line;
use ratatui::text::Span;

use super::format::format_tokens;
use super::format::format_usd;

/// What the figures measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cost,
    Tokens,
}

impl Metric {
    pub fn toggled(self) -> Self {
        match self {
            Self::Cost => Self::Tokens,
            Self::Tokens => Self::Cost,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cost => "cost",
            Self::Tokens => "tokens",
        }
    }
}

/// A model of one provider: how the view names the row a person selects.
pub type ModelKey = (String, String);

/// What the body of the overlay shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Overview,
    /// One model, in detail.
    Model(ModelKey),
    /// The effective price table.
    Prices,
}

pub(super) struct View<'a> {
    pub theme: &'a Theme,
    /// The model row the cursor is on, if any.
    pub selected: Option<&'a ModelKey>,
    /// Cells available to a section, inside the frame's padding.
    pub width: i32,
    pub metric: Metric,
    pub now: DateTime<Utc>,
    pub summary: &'a UsageSummary,
}

impl View<'_> {
    /// The number a series is ranked and drawn by.
    pub(super) fn measure(&self, cost_usd: f64, tokens: i64) -> f64 {
        match self.metric {
            Metric::Cost => cost_usd,
            Metric::Tokens => tokens as f64,
        }
    }

    pub(super) fn show(&self, cost_usd: f64, tokens: i64) -> String {
        match self.metric {
            Metric::Cost => format_usd(cost_usd),
            Metric::Tokens => format_tokens(tokens),
        }
    }

    pub(super) fn section_title(&self, title: &str) -> Line<'static> {
        Line::from(Span::styled(title.to_owned(), self.theme.text_strong))
    }
}

/// The summary's models in the order the breakdown lists them: most of `metric` first.
pub(super) fn ranked_models(summary: &UsageSummary, metric: Metric) -> Vec<&ModelUsage> {
    let measure = |model: &ModelUsage| match metric {
        Metric::Cost => model.cost_usd,
        Metric::Tokens => model.tokens.total() as f64,
    };
    let mut models: Vec<&ModelUsage> = summary.models.iter().collect();
    models.sort_by(|a, b| measure(b).total_cmp(&measure(a)));
    models
}
