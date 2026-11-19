//! The colours of a diff, taken from the live theme: line tints (`diff_add`, `diff_del`), the
//! stronger emphasis the changed words of an edited line get (`diff_add_word`, `diff_del_word`),
//! and the faint line-number gutter.

use nori_tui_components::Theme;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

pub(super) struct DiffRenderStyleContext {
    pub(super) add_bg: Option<Color>,
    pub(super) del_bg: Option<Color>,
    /// An added line: its tint, or (no backgrounds) its colour.
    add_line: Style,
    del_line: Style,
    /// What the changed words of an added line that has a removed counterpart are drawn with,
    /// over its syntax colours.
    pub(super) add_emphasis: Style,
    pub(super) del_emphasis: Style,
    add_sign: Style,
    del_sign: Style,
    gutter: Style,
}

impl DiffRenderStyleContext {
    pub(super) fn from_theme(theme: &Theme) -> Self {
        let mut context = Self {
            add_bg: theme.diff_add.bg,
            del_bg: theme.diff_del.bg,
            add_line: theme.diff_add,
            del_line: theme.diff_del,
            add_emphasis: theme.diff_add_word,
            del_emphasis: theme.diff_del_word,
            add_sign: theme.accent_mint,
            del_sign: theme.accent_coral,
            gutter: theme.faint,
        };
        if context.add_bg.is_none() {
            context.use_indexed_tints();
        }
        context
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
        self.add_bg = Some(Color::Indexed(add));
        self.del_bg = Some(Color::Indexed(del));
        self.add_line = Style::default().bg(Color::Indexed(add));
        self.del_line = Style::default().bg(Color::Indexed(del));
        self.add_emphasis = Style::default().bg(Color::Indexed(add_word));
        self.del_emphasis = Style::default().bg(Color::Indexed(del_word));
    }
}

/// The line-number gutter: barely there, like every piece of structure.
pub(super) fn style_gutter(ctx: &DiffRenderStyleContext) -> Style {
    ctx.gutter
}

pub(super) fn style_context() -> Style {
    Style::default()
}

/// An added line's own style: its tint, or its colour where there are no backgrounds.
pub(super) fn style_add(ctx: &DiffRenderStyleContext) -> Style {
    ctx.add_line
}

pub(super) fn style_del(ctx: &DiffRenderStyleContext) -> Style {
    ctx.del_line
}

/// The `+` / `-` sign: the accent colour on the line's tint.
pub(super) fn style_sign(ctx: &DiffRenderStyleContext, added: bool) -> Style {
    let (sign, line) = if added {
        (ctx.add_sign, ctx.add_line)
    } else {
        (ctx.del_sign, ctx.del_line)
    };
    line.patch(sign).add_modifier(Modifier::BOLD)
}
