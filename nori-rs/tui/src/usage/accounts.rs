//! Spend per account: what each account of each provider cost, by model, with gateway routes
//! told apart from direct ones. Shown when the period has more than one account of a provider
//! or any gateway route; with one account per provider it would only repeat the model table.

use nori_harness::usage::AccountUsage;
use ratatui::text::Line;
use ratatui::text::Span;

use super::format::clip;
use super::format::format_tokens;
use super::format::format_usd;
use super::format::pad_left;
use super::format::pad_right;
use super::format::provider_label;
use super::format::provider_style;
use super::view::View;

/// Models listed under each account before the rest are folded into one row.
const MODELS_SHOWN: usize = 3;
const COST_WIDTH: i32 = 9;
const TOKENS_WIDTH: i32 = 8;

pub(super) fn accounts(view: &View) -> Vec<Line<'static>> {
    if !view.summary.has_account_detail() {
        return Vec::new();
    }
    let theme = view.theme;
    let name_width = (view.width - 4 - COST_WIDTH - TOKENS_WIDTH).max(12);
    let mut lines = vec![
        view.section_title("Accounts"),
        Line::from(vec![
            Span::styled(pad_right("", name_width + 2), theme.faint),
            Span::styled(pad_left("cost", COST_WIDTH), theme.faint),
            Span::styled(pad_left("tokens", TOKENS_WIDTH), theme.faint),
        ]),
    ];
    // A provider with one direct account is already its provider row; only providers that
    // split across accounts or gateways are broken down.
    let split = |usage: &AccountUsage| {
        let of_provider = view
            .summary
            .accounts
            .iter()
            .filter(|other| other.account.provider == usage.account.provider);
        of_provider.clone().count() > 1
            || of_provider.clone().any(|other| other.account.via.is_some())
    };
    let mut accounts: Vec<&AccountUsage> = view
        .summary
        .accounts
        .iter()
        .filter(|usage| split(usage))
        .collect();
    accounts.sort_by(|a, b| a.account.cmp(&b.account));
    for usage in accounts {
        let style = provider_style(theme, &usage.account.provider);
        let title = format!(
            "{} · {}",
            provider_label(&usage.account.provider),
            usage.account.describe()
        );
        lines.push(Line::from(vec![
            Span::styled("● ", style),
            Span::styled(pad_right(&clip(&title, name_width), name_width), theme.text),
            Span::styled(
                pad_left(&format_usd(usage.cost_usd), COST_WIDTH),
                theme.text_strong,
            ),
            Span::styled(
                pad_left(&format_tokens(usage.tokens.total()), TOKENS_WIDTH),
                theme.muted,
            ),
        ]));
        for model in usage.models.iter().take(MODELS_SHOWN) {
            let name = nori_harness::recovery::model_label(&model.model, &model.model, None);
            let cost = if model.cost_usd <= 0.0 && model.unpriced_tokens > 0 {
                "unpriced".to_owned()
            } else {
                format_usd(model.cost_usd)
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    pad_right(&clip(&name, name_width - 2), name_width - 2),
                    theme.text_secondary,
                ),
                Span::styled(pad_left(&cost, COST_WIDTH), theme.text_secondary),
                Span::styled(
                    pad_left(&format_tokens(model.tokens.total()), TOKENS_WIDTH),
                    theme.faint,
                ),
            ]));
        }
        if usage.models.len() > MODELS_SHOWN {
            lines.push(Line::from(Span::styled(
                format!("    + {} more", usage.models.len() - MODELS_SHOWN),
                theme.faint,
            )));
        }
    }
    lines
}
