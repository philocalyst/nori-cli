//! The colours of a diff, taken from the live theme: the tints of lines that only appeared or
//! went (`diff_add`, `diff_del`), the stronger marks of the words an edited line gained or lost
//! (`diff_add_word`, `diff_del_word`), the accents of the bars in the margin, and the faint
//! line-number gutter. Tints sit behind text only, never to the edge of the row.

use nori_tui_components::Theme;

use super::line::Mark;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

pub(super) struct DiffRenderStyleContext {
    /// A line that only appeared: its tint behind its text, or (no backgrounds) its colour.
    add_line: Style,
    del_line: Style,
    /// The words an edited line gained or lost, over its colours.
    add_word: Style,
    del_word: Style,
    add_mark: Style,
    del_mark: Style,
    edit_mark: Style,
    gutter: Style,
}

impl DiffRenderStyleContext {
    pub(super) fn from_theme(theme: &Theme) -> Self {
        let mut context = Self {
            add_line: theme.diff_add,
            del_line: theme.diff_del,
            add_word: theme.diff_add_word,
            del_word: theme.diff_del_word,
            add_mark: theme.accent_mint,
            del_mark: theme.accent_coral,
            edit_mark: theme.accent_amber,
            gutter: theme.faint,
        };
        if theme.diff_add.bg.is_none() {
            context.use_indexed_tints();
        }
        context
    }

    /// A line that only appeared, from its indent on.
    pub(super) fn added(&self) -> Style {
        self.add_line
    }

    /// A line that only went, from its indent on: struck through.
    pub(super) fn removed(&self) -> Style {
        self.del_line.add_modifier(Modifier::CROSSED_OUT)
    }

    /// The words an edited line gained.
    pub(super) fn gained(&self) -> Style {
        self.add_word
    }

    /// The words an edited line lost, struck through where they stood.
    pub(super) fn lost(&self) -> Style {
        self.del_word.add_modifier(Modifier::CROSSED_OUT)
    }

    /// The context for the terminal the TUI is drawing on.
    pub(super) fn detected() -> Self {
        Self::from_theme(&crate::history_cell::chrome::live_theme())
    }

    /// A 256-colour terminal has no true-colour surfaces in the theme but can still tint lines
    /// from the xterm palette; 16-colour terminals keep coloured text.
    #[allow(clippy::disallowed_methods)]
    fn use_indexed_tints(&mut self) {
        let has_256 = supports_color::on_cached(supports_color::Stream::Stdout)
            .is_some_and(|level| level.has_256);
        if !has_256 {
            return;
        }
        let light = crate::terminal_palette::default_bg().is_some_and(crate::color::is_light);
        let (add, del, add_word, del_word) = if light {
            (194, 224, 157, 217)
        } else {
            (22, 52, 28, 88)
        };
        self.add_line = Style::default().bg(Color::Indexed(add));
        self.del_line = Style::default().bg(Color::Indexed(del));
        self.add_word = Style::default().bg(Color::Indexed(add_word));
        self.del_word = Style::default().bg(Color::Indexed(del_word));
    }
}

/// The line-number gutter: barely there, like every piece of structure.
pub(super) fn style_gutter(ctx: &DiffRenderStyleContext) -> Style {
    ctx.gutter
}

/// The bar in the margin of a changed line: the accent of how it changed.
pub(super) fn style_mark(ctx: &DiffRenderStyleContext, mark: Mark) -> Style {
    match mark {
        Mark::Added => ctx.add_mark,
        Mark::Removed => ctx.del_mark,
        Mark::Edited => ctx.edit_mark,
        Mark::Kept => ctx.gutter,
    }
}
