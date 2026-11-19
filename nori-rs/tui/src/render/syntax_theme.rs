//! The syntect theme for code, built from the live [`Theme`]'s tokens, so highlighted code, diffs
//! and shell commands share one palette and follow the terminal's light or dark background.

use std::str::FromStr;
use std::sync::Mutex;

use nori_tui_components::Theme;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use syntect::highlighting::Color as SyntectColor;
use syntect::highlighting::FontStyle;
use syntect::highlighting::ScopeSelectors;
use syntect::highlighting::StyleModifier;
use syntect::highlighting::Theme as SyntectTheme;
use syntect::highlighting::ThemeItem;
use syntect::highlighting::ThemeSettings;

/// The syntect theme for the terminal as it is now. A theme is built once per distinct palette
/// and kept for the life of the process: highlighters borrow it.
pub(super) fn current() -> &'static SyntectTheme {
    static CACHE: Mutex<Option<(Theme, &'static SyntectTheme)>> = Mutex::new(None);
    let live = crate::history_cell::chrome::live_theme();
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((theme, built)) = *cache
        && theme == live
    {
        return built;
    }
    let built: &'static SyntectTheme = Box::leak(Box::new(build(&live)));
    *cache = Some((live, built));
    built
}

fn build(theme: &Theme) -> SyntectTheme {
    let syntax = &theme.syntax;
    // `(scope selector, style)`: later, more specific selectors win in syntect.
    let rules: [(&str, Style); 22] = [
        ("comment", syntax.comment),
        ("string", syntax.string),
        ("constant.numeric", theme.accent_amber),
        ("constant.language", theme.accent_amber),
        ("constant.character", syntax.string),
        ("constant.other", syntax.r#type),
        ("keyword", syntax.keyword),
        ("storage", syntax.keyword),
        ("keyword.operator", syntax.operator),
        ("entity.name.function", syntax.function),
        ("support.function", syntax.function),
        ("meta.function-call", syntax.function),
        ("entity.name.type", syntax.r#type),
        ("entity.name.class", syntax.r#type),
        ("support.type", syntax.r#type),
        ("support.class", syntax.r#type),
        ("variable.language", syntax.keyword),
        ("entity.name.tag", syntax.keyword),
        ("entity.other.attribute-name", syntax.r#type),
        ("markup.heading", theme.text_strong),
        ("markup.inserted", theme.accent_mint),
        ("markup.deleted", theme.accent_coral),
    ];
    SyntectTheme {
        name: Some("nori".to_owned()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(TERMINAL_DEFAULT),
            background: Some(TERMINAL_DEFAULT),
            ..ThemeSettings::default()
        },
        scopes: rules
            .into_iter()
            .filter_map(|(selector, style)| {
                Some(ThemeItem {
                    scope: ScopeSelectors::from_str(selector).ok()?,
                    style: StyleModifier {
                        foreground: style.fg.map(syntect_color),
                        background: None,
                        font_style: font_style(style),
                    },
                })
            })
            .collect(),
    }
}

/// syntect has no "terminal default" colour; alpha `0x01` is the marker this module's reader
/// turns back into [`Color::Reset`].
const TERMINAL_DEFAULT: SyntectColor = SyntectColor {
    r: 0,
    g: 0,
    b: 0,
    a: 0x01,
};

/// A ratatui colour in syntect's: RGB with full alpha, an ANSI palette index as alpha `0x00` with
/// the index in red.
fn syntect_color(color: Color) -> SyntectColor {
    let (r, g, b, a) = match color {
        Color::Rgb(r, g, b) => (r, g, b, 0xff),
        Color::Indexed(index) => (index, 0, 0, 0x00),
        Color::Black => (0, 0, 0, 0x00),
        Color::Red => (1, 0, 0, 0x00),
        Color::Green => (2, 0, 0, 0x00),
        Color::Yellow => (3, 0, 0, 0x00),
        Color::Blue => (4, 0, 0, 0x00),
        Color::Magenta => (5, 0, 0, 0x00),
        Color::Cyan => (6, 0, 0, 0x00),
        Color::Gray => (7, 0, 0, 0x00),
        Color::DarkGray => (8, 0, 0, 0x00),
        Color::LightRed => (9, 0, 0, 0x00),
        Color::LightGreen => (10, 0, 0, 0x00),
        Color::LightYellow => (11, 0, 0, 0x00),
        Color::LightBlue => (12, 0, 0, 0x00),
        Color::LightMagenta => (13, 0, 0, 0x00),
        Color::LightCyan => (14, 0, 0, 0x00),
        Color::White => (15, 0, 0, 0x00),
        Color::Reset => (0, 0, 0, 0x01),
    };
    SyntectColor { r, g, b, a }
}

fn font_style(style: Style) -> Option<FontStyle> {
    let mut font = FontStyle::empty();
    if style.add_modifier.contains(Modifier::BOLD) {
        font |= FontStyle::BOLD;
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        font |= FontStyle::ITALIC;
    }
    (!font.is_empty()).then_some(font)
}
