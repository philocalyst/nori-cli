use crate::terminal_palette::default_bg;
use nori_tui_components::Theme;
use ratatui::style::Color;
use ratatui::style::Style;

pub fn user_message_style() -> Style {
    user_message_style_for(None)
}

pub(crate) fn component_theme() -> Theme {
    Theme::for_terminal_background(relative_terminal_background())
}

/// User messages and the composer inherit the terminal's background.
pub fn user_message_style_for(_terminal_bg: Option<(u8, u8, u8)>) -> Style {
    Style::default().bg(Color::Reset)
}

fn relative_terminal_background() -> Option<(u8, u8, u8)> {
    supports_color::on_cached(supports_color::Stream::Stdout)
        .filter(|level| level.has_16m)
        .and_then(|_| default_bg())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_style_has_no_absolute_fallback() {
        assert_eq!(user_message_style_for(None).bg, Some(Color::Reset));
    }

    #[test]
    fn user_message_style_inherits_dark_and_light_terminal_backgrounds() {
        assert_eq!(
            user_message_style_for(Some((20, 20, 20))).bg,
            Some(Color::Reset)
        );
        assert_eq!(
            user_message_style_for(Some((240, 240, 240))).bg,
            Some(Color::Reset)
        );
    }

    #[test]
    fn user_message_and_composer_background_snapshot() {
        use crate::history_cell::HistoryCell;
        use crate::history_cell::UserHistoryCell;

        let message = UserHistoryCell {
            message: "Ordinary text".to_string(),
        };
        let backgrounds = message
            .display_lines(40)
            .iter()
            .map(|line| format!("{:?}", line.style.bg))
            .collect::<Vec<_>>()
            .join(", ");
        let composer = user_message_style().bg;
        insta::assert_snapshot!(
            format!("Message rows: {backgrounds}\nComposer: {composer:?}"),
            @"
        Message rows: None
        Composer: Some(Reset)
        "
        );
    }
}
