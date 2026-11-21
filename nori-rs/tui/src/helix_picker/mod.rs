//! Glue between the terminal and the Helix picker engine in `nori-tui-components`: translating
//! terminal events into engine input, and the small formatting every picker row shares.
//!
//! The engine owns the frame, query language, ranking and keys; the pickers built on it
//! ([`crate::resume_picker`], [`crate::subagent_switcher`]) own only their rows and previews.

use nori_tui_components::HelixEvent;
use nori_tui_components::HelixItem;
use nori_tui_components::HelixState;
use nori_tui_components::Theme;
use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tui::TuiEvent;

/// Feed a keyboard, paste or mouse event to the engine. `Draw` is the host's to handle.
pub(crate) fn feed<I: HelixItem>(
    state: &mut HelixState<I>,
    event: &TuiEvent,
) -> Option<HelixEvent<I::Id>> {
    match event {
        TuiEvent::Key(key) => Some(state.on_key(*key)),
        TuiEvent::Paste(text) => Some(state.on_paste(text)),
        TuiEvent::Mouse(mouse) => Some(state.on_mouse(*mouse)),
        TuiEvent::Draw => None,
    }
}

/// The accent an agent's dot and name are drawn in, from the theme's provider palette.
pub(crate) fn agent_style(theme: &Theme, agent: &str) -> Style {
    let name = agent.to_ascii_lowercase();
    if name.contains("claude") {
        theme.provider_claude
    } else if name.contains("codex") || name.contains("gpt") {
        theme.provider_codex
    } else if name.contains("gemini") {
        theme.provider_gemini
    } else if name.contains("antigravity") {
        theme.provider_antigravity
    } else if name.contains("nori") || name.contains("eliza") {
        theme.provider_nori
    } else {
        theme.accent_cyan
    }
}

/// How long ago `iso` was, in the coarsest whole unit: `now`, `12m`, `3h`, `2d`, `5w`.
pub(crate) fn compact_age(iso: &str) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return String::new();
    };
    let elapsed = chrono::Utc::now().signed_duration_since(then);
    match elapsed.num_seconds() {
        ..60 => "now".to_owned(),
        seconds @ 60..3600 => format!("{}m", seconds / 60),
        seconds @ 3600..86_400 => format!("{}h", seconds / 3600),
        seconds @ 86_400..604_800 => format!("{}d", seconds / 86_400),
        seconds => format!("{}w", seconds / 604_800),
    }
}

/// `text` cut to `max` columns, the cut marked with `…`.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let mut clipped = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        used += grapheme.width();
        if used + 1 > max {
            break;
        }
        clipped.push_str(grapheme);
    }
    if max > 0 {
        clipped.push('…');
    }
    clipped
}
