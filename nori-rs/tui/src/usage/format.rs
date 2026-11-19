//! How the usage view prints numbers, times and provider names.

use chrono::DateTime;
use chrono::Utc;
use nori_tui_components::Theme;
use ratatui::style::Color;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

/// `812`, `12.4K`, `1.2M`, `3.4B`.
pub(super) fn format_tokens(count: i64) -> String {
    let count = count.max(0) as f64;
    let (value, suffix) = if count >= 1e9 {
        (count / 1e9, "B")
    } else if count >= 1e6 {
        (count / 1e6, "M")
    } else if count >= 1e3 {
        (count / 1e3, "K")
    } else {
        return format!("{count:.0}");
    };
    if value >= 100.0 {
        format!("{value:.0}{suffix}")
    } else {
        format!("{value:.1}{suffix}")
    }
}

/// `$0.42`, `$12.40`, `$142`, `<$0.01`.
pub(super) fn format_usd(amount: f64) -> String {
    if amount <= 0.0 {
        "$0.00".to_owned()
    } else if amount < 0.01 {
        "<$0.01".to_owned()
    } else if amount >= 1000.0 {
        format!("${:.1}K", amount / 1000.0)
    } else if amount >= 100.0 {
        format!("${amount:.0}")
    } else {
        format!("${amount:.2}")
    }
}

/// A fraction as `67%`, `<1%`.
pub(super) fn format_share(fraction: f64) -> String {
    let percent = fraction * 100.0;
    if percent <= 0.0 {
        "0%".to_owned()
    } else if percent < 1.0 {
        "<1%".to_owned()
    } else {
        format!("{percent:.0}%")
    }
}

/// `2h 14m`, `3d 4h`, `<1m`.
pub(super) fn format_countdown(until: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (until - now).num_minutes();
    if minutes < 1 {
        "<1m".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 48 * 60 {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d {}h", minutes / (24 * 60), (minutes / 60) % 24)
    }
}

pub(super) fn provider_label(provider: &str) -> String {
    nori_harness::usage::provider_title(provider)
}

/// A provider's accent, from the theme's own provider and accent tokens.
pub(super) fn provider_style(theme: &Theme, provider: &str) -> Style {
    match provider {
        "claude" => theme.provider_claude,
        "codex" => theme.provider_codex,
        "gemini" => theme.provider_gemini,
        "antigravity" => theme.provider_antigravity,
        "deepseek" => theme.accent_cyan,
        "grok" => theme.accent_violet,
        _ => theme.provider_nori,
    }
}

pub(super) fn color_of(style: Style) -> Color {
    style.fg.unwrap_or_default()
}

/// The theme's accents in rotation, for series that have no identity of their own.
pub(super) fn series_style(theme: &Theme, index: usize) -> Style {
    [
        theme.accent_cyan,
        theme.accent_mint,
        theme.accent_violet,
        theme.accent_amber,
        theme.accent_coral,
    ][index % 5]
}

pub(super) fn pad_right(text: &str, width: i32) -> String {
    let width = usize::try_from(width.max(0)).unwrap_or(0);
    let used = text.width();
    if used >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - used))
    }
}

pub(super) fn pad_left(text: &str, width: i32) -> String {
    let width = usize::try_from(width.max(0)).unwrap_or(0);
    let used = text.width();
    if used >= width {
        text.to_owned()
    } else {
        format!("{}{text}", " ".repeat(width - used))
    }
}

/// `text` cut to `width` cells with a trailing `…`.
pub(super) fn clip(text: &str, width: i32) -> String {
    let width = usize::try_from(width.max(1)).unwrap_or(1);
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for ch in text.chars() {
        if out.width() + 1 >= width {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}
