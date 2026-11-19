//! Prints the `/usage` view over fixture data: every period and metric at 132 and 80 columns,
//! the per-model drill-down, the price table and a provider filter.
//!
//! `nb run -p nori-tui --features storybook --example usage_storybook [-- <name>]` where
//! `<name>` selects one specimen (default: all).

use nori_harness::usage::UsagePeriod;
use nori_tui::storybook::UsageMetric;
use nori_tui::storybook::UsageSpecimen;
use nori_tui::storybook::usage_specimen;

struct Case {
    name: &'static str,
    width: u16,
    height: u16,
    period: UsagePeriod,
    metric: UsageMetric,
    provider: Option<&'static str>,
    specimen: UsageSpecimen,
}

fn main() {
    let filter = std::env::args().nth(1);
    let mut cases = Vec::new();
    for (width, height) in [(132, 62), (80, 70)] {
        for (period, metric) in [
            (UsagePeriod::Week, UsageMetric::Cost),
            (UsagePeriod::Day, UsageMetric::Tokens),
            (UsagePeriod::Month, UsageMetric::Cost),
        ] {
            cases.push(Case {
                name: "overview",
                width,
                height,
                period,
                metric,
                provider: None,
                specimen: UsageSpecimen::Overview { selected: Some(1) },
            });
        }
        cases.push(Case {
            name: "model",
            width,
            height: 46,
            period: UsagePeriod::Week,
            metric: UsageMetric::Cost,
            provider: None,
            specimen: UsageSpecimen::Model { rank: 0 },
        });
        cases.push(Case {
            name: "prices",
            width,
            height: 30,
            period: UsagePeriod::Week,
            metric: UsageMetric::Cost,
            provider: None,
            specimen: UsageSpecimen::Prices,
        });
        cases.push(Case {
            name: "provider",
            width,
            height: 52,
            period: UsagePeriod::Week,
            metric: UsageMetric::Cost,
            provider: Some("codex"),
            specimen: UsageSpecimen::Overview { selected: None },
        });
    }
    for case in cases {
        if filter.as_deref().is_some_and(|name| name != case.name) {
            continue;
        }
        println!(
            "--- {} {}x{} {} {} ---",
            case.name,
            case.width,
            case.height,
            case.period.label(),
            case.metric.label()
        );
        println!(
            "{}",
            usage_specimen(
                case.width,
                case.height,
                case.period,
                case.metric,
                case.provider,
                &case.specimen
            )
        );
    }
}
