use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

/// Semantic styling tokens used by every component in this crate.
///
/// Applications can provide a different theme without changing component
/// state or behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub text: Style,
    pub muted: Style,
    /// Compact focus and interaction indicators such as pointers, rails, and shortcuts.
    pub pointer: Style,
    pub backdrop: Style,
    pub menu_surface: Style,
    pub menu_item_surface: Style,
    pub menu_item_surface_alt: Style,
    pub surface: Style,
    pub input: Style,
    pub row: Style,
    pub row_alt: Style,
    pub detail_surface: Style,
    pub selected: Style,
    pub disabled: Style,
    pub border: Style,
    pub separator: Style,
    pub title: Style,
    pub info: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub code: Style,
    pub link: Style,
    pub table_header: Style,
    pub table_rule: Style,
    pub provider_claude: Style,
    pub provider_codex: Style,
    pub provider_gemini: Style,
    pub provider_antigravity: Style,
    pub provider_nori: Style,
    /// Raised content such as tool cards and subagent lanes.
    pub card: Style,
    /// Wells behind code, commands and diffs.
    pub code_well: Style,
    /// The user's prompt rows in the transcript.
    pub prompt_surface: Style,
    /// The prompt a rewind is focused on: warm, so it reads apart from the cards around it.
    pub rewind_surface: Style,
    /// The single status line under the composer.
    pub statusline: Style,
    /// The strongest foreground, for the one thing a row is about.
    pub text_strong: Style,
    /// Secondary prose: tool summaries, metadata that still reads as content.
    pub text_secondary: Style,
    /// Barely-there structure: tree rails, counters, timestamps.
    pub faint: Style,
    pub accent_mint: Style,
    pub accent_cyan: Style,
    pub accent_violet: Style,
    pub accent_amber: Style,
    pub accent_coral: Style,
    pub diff_add: Style,
    pub diff_del: Style,
    pub diff_add_word: Style,
    pub diff_del_word: Style,
    pub syntax: SyntaxTheme,
    pub badge: BadgeTheme,
}

/// Token styles for highlighted code, shell commands and diffs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntaxTheme {
    pub keyword: Style,
    pub function: Style,
    pub r#type: Style,
    pub string: Style,
    /// Command-line flags such as `--force`.
    pub flag: Style,
    pub variable: Style,
    pub operator: Style,
    pub comment: Style,
}

/// The status line's mode badge, one style per mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadgeTheme {
    pub insert: Style,
    pub normal: Style,
    pub rewind: Style,
    pub bash: Style,
    pub resume: Style,
    pub actors: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            text: Style::new(),
            muted: Style::new().fg(Color::DarkGray),
            pointer: Style::new().fg(Color::Green),
            backdrop: Style::new(),
            menu_surface: Style::new(),
            menu_item_surface: Style::new(),
            menu_item_surface_alt: Style::new(),
            surface: Style::new(),
            input: Style::new(),
            row: Style::new(),
            row_alt: Style::new(),
            detail_surface: Style::new(),
            selected: Style::new(),
            disabled: Style::new().fg(Color::DarkGray),
            border: Style::new().fg(Color::DarkGray),
            separator: Style::new().fg(Color::DarkGray),
            title: Style::new().add_modifier(Modifier::BOLD),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            code: Style::new().fg(Color::Cyan),
            link: Style::new()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
            table_header: Style::new().add_modifier(Modifier::BOLD),
            table_rule: Style::new().fg(Color::DarkGray),
            provider_claude: Style::new().fg(Color::Yellow),
            provider_codex: Style::new().fg(Color::White),
            provider_gemini: Style::new().fg(Color::Blue),
            provider_antigravity: Style::new().fg(Color::Blue),
            provider_nori: Style::new().fg(Color::Green),
            card: Style::new(),
            code_well: Style::new(),
            prompt_surface: Style::new(),
            rewind_surface: Style::new(),
            statusline: Style::new(),
            text_strong: Style::new().add_modifier(Modifier::BOLD),
            text_secondary: Style::new(),
            faint: Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            accent_mint: Style::new().fg(Color::Green),
            accent_cyan: Style::new().fg(Color::Cyan),
            accent_violet: Style::new().fg(Color::Magenta),
            accent_amber: Style::new().fg(Color::Yellow),
            accent_coral: Style::new().fg(Color::Red),
            diff_add: Style::new().fg(Color::Green),
            diff_del: Style::new().fg(Color::Red),
            diff_add_word: Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            diff_del_word: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            syntax: SyntaxTheme {
                keyword: Style::new()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::ITALIC),
                function: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                r#type: Style::new().fg(Color::Yellow),
                string: Style::new().fg(Color::Green),
                flag: Style::new()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::ITALIC),
                variable: Style::new().fg(Color::LightRed),
                operator: Style::new().fg(Color::Red),
                comment: Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            },
            badge: BadgeTheme {
                insert: Style::new().add_modifier(Modifier::BOLD),
                normal: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                rewind: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                bash: Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                resume: Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                actors: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            },
        }
    }
}

impl Theme {
    /// Builds the default theme with neutral surfaces derived from the terminal background.
    ///
    /// Consumers should pass a background only when the terminal reports both its RGB
    /// background and true-color support. `None` deliberately leaves every neutral
    /// background unset rather than substituting fixed palette indices.
    #[allow(clippy::disallowed_methods)]
    pub fn for_terminal_background(terminal_bg: Option<(u8, u8, u8)>) -> Self {
        let mut theme = Self::default();
        let Some(background) = terminal_bg else {
            return theme;
        };

        theme.backdrop = theme.backdrop.bg(relative_surface(background, 4));
        let menu_surface = relative_surface(background, 8);
        theme.menu_surface = theme.menu_surface.bg(menu_surface);
        theme.menu_item_surface = theme.menu_item_surface.bg(darken_surface(menu_surface, 3));
        theme.menu_item_surface_alt = theme
            .menu_item_surface_alt
            .bg(darken_surface(menu_surface, 6));
        theme.row = theme.row.bg(relative_surface(background, 4));
        theme.row_alt = theme.row_alt.bg(relative_surface(background, 7));
        theme.input = theme.input.bg(relative_surface(background, 8));
        theme.detail_surface = theme.detail_surface.bg(relative_surface(background, 8));
        theme.selected = theme.selected.bg(relative_surface(background, 10));
        // Surfaces stay relative to the user's own background so native scrollback and
        // Nori's rows read as one page; only accents and syntax use the fixed palette.
        theme.card = theme.card.bg(relative_surface(background, 4));
        theme.code_well = theme
            .code_well
            .bg(darken_surface(background_color(background), 6));
        theme.prompt_surface = theme.prompt_surface.bg(relative_surface(background, 7));
        theme.rewind_surface = theme.rewind_surface.bg(tinted_surface(
            background,
            rgb_bytes(palette_for(background).amber),
            9,
        ));
        theme.statusline = theme
            .statusline
            .bg(darken_surface(background_color(background), 3));
        let palette = if is_light(background) { &LIGHT } else { &DARK };
        theme.text_strong = theme.text_strong.fg(rgb(palette.text_strong));
        theme.text_secondary = theme.text_secondary.fg(rgb(palette.text_secondary));
        theme.muted = theme.muted.fg(rgb(palette.muted));
        theme.faint = Style::new().fg(rgb(palette.faint));
        theme.accent_mint = theme.accent_mint.fg(rgb(palette.mint));
        theme.accent_cyan = theme.accent_cyan.fg(rgb(palette.cyan));
        theme.accent_violet = theme.accent_violet.fg(rgb(palette.violet));
        theme.accent_amber = theme.accent_amber.fg(rgb(palette.amber));
        theme.accent_coral = theme.accent_coral.fg(rgb(palette.coral));
        theme.pointer = theme.pointer.fg(rgb(palette.mint));
        theme.info = theme.info.fg(rgb(palette.cyan));
        theme.success = theme.success.fg(rgb(palette.mint));
        theme.warning = theme.warning.fg(rgb(palette.amber));
        theme.error = theme.error.fg(rgb(palette.coral));
        theme.diff_add = Style::new().bg(rgb(palette.diff_add_bg));
        theme.diff_del = Style::new().bg(rgb(palette.diff_del_bg));
        theme.diff_add_word = Style::new().bg(rgb(palette.diff_add_word_bg));
        theme.diff_del_word = Style::new().bg(rgb(palette.diff_del_word_bg));
        let syntax = &mut theme.syntax;
        syntax.keyword = syntax.keyword.fg(rgb(palette.keyword));
        syntax.function = syntax.function.fg(rgb(palette.function));
        syntax.r#type = syntax.r#type.fg(rgb(palette.r#type));
        syntax.string = syntax.string.fg(rgb(palette.string));
        syntax.flag = syntax.flag.fg(rgb(palette.flag));
        syntax.variable = syntax.variable.fg(rgb(palette.variable));
        syntax.operator = syntax.operator.fg(rgb(palette.operator));
        syntax.comment = syntax.comment.fg(rgb(palette.comment));
        // Agent identities, tuned for the background: an ANSI white, which reads on a dark
        // terminal, is invisible on a light one.
        theme.provider_claude = theme.provider_claude.fg(rgb(palette.provider_claude));
        theme.provider_codex = theme.provider_codex.fg(rgb(palette.provider_codex));
        theme.provider_gemini = theme.provider_gemini.fg(rgb(palette.provider_gemini));
        theme.provider_antigravity = theme
            .provider_antigravity
            .fg(rgb(palette.provider_antigravity));
        theme.provider_nori = theme.provider_nori.fg(rgb(palette.provider_nori));
        let badge = &mut theme.badge;
        badge.normal = badge.normal.fg(rgb(palette.cyan));
        badge.rewind = badge.rewind.fg(rgb(palette.amber));
        badge.bash = badge.bash.fg(rgb(palette.mint));
        badge.resume = badge.resume.fg(rgb(palette.violet));
        badge.actors = badge.actors.fg(rgb(palette.cyan));
        theme
    }
}

/// The fixed true-color palette, tuned once for dark and once for light backgrounds.
struct Palette {
    text_strong: u32,
    text_secondary: u32,
    muted: u32,
    faint: u32,
    mint: u32,
    cyan: u32,
    violet: u32,
    amber: u32,
    coral: u32,
    diff_add_bg: u32,
    diff_del_bg: u32,
    diff_add_word_bg: u32,
    diff_del_word_bg: u32,
    keyword: u32,
    function: u32,
    r#type: u32,
    string: u32,
    flag: u32,
    variable: u32,
    operator: u32,
    comment: u32,
    provider_claude: u32,
    provider_codex: u32,
    provider_gemini: u32,
    provider_antigravity: u32,
    provider_nori: u32,
}

const DARK: Palette = Palette {
    text_strong: 0xF8F4FC,
    text_secondary: 0xE0D9E8,
    muted: 0x92859E,
    faint: 0x5C5068,
    mint: 0x5CEEAC,
    cyan: 0x5ED6FF,
    violet: 0xB884FF,
    amber: 0xFFC05C,
    coral: 0xFF6876,
    diff_add_bg: 0x13261D,
    diff_del_bg: 0x28141A,
    diff_add_word_bg: 0x204A35,
    diff_del_word_bg: 0x4E202C,
    keyword: 0xD686FF,
    function: 0x7AC6FF,
    r#type: 0xFFCB6B,
    string: 0xA6E3A1,
    flag: 0xB4A2F6,
    variable: 0xFFA07A,
    operator: 0xFF6E94,
    comment: 0x665A74,
    provider_claude: 0xFF9E64,
    provider_codex: 0xE8E4EC,
    provider_gemini: 0x7AA2F7,
    provider_antigravity: 0x5ED6FF,
    provider_nori: 0x5CEEAC,
};

const LIGHT: Palette = Palette {
    text_strong: 0x1B1220,
    text_secondary: 0x3A3042,
    muted: 0x6E6278,
    faint: 0xA79DB0,
    mint: 0x0E8A57,
    cyan: 0x0A7FA8,
    violet: 0x7A3FD1,
    amber: 0xA86400,
    coral: 0xC8303F,
    diff_add_bg: 0xDDF3E6,
    diff_del_bg: 0xF8DEE2,
    diff_add_word_bg: 0xB4E4C7,
    diff_del_word_bg: 0xF1B7C0,
    keyword: 0x9A2FC9,
    function: 0x1F68B3,
    r#type: 0x946000,
    string: 0x2F7A2B,
    flag: 0x6A55C2,
    variable: 0xB4502A,
    operator: 0xC2245A,
    comment: 0x8C8096,
    provider_claude: 0xB85A1E,
    provider_codex: 0x1B1220,
    provider_gemini: 0x2F5FBF,
    provider_antigravity: 0x0A7FA8,
    provider_nori: 0x0E8A57,
};

fn palette_for(background: (u8, u8, u8)) -> &'static Palette {
    if is_light(background) { &LIGHT } else { &DARK }
}

fn rgb_bytes(hex: u32) -> (u8, u8, u8) {
    let [_, red, green, blue] = hex.to_be_bytes();
    (red, green, blue)
}

/// `background` with `strength` percent of `tint` mixed in.
#[allow(clippy::disallowed_methods)]
fn tinted_surface(background: (u8, u8, u8), tint: (u8, u8, u8), strength: u16) -> Color {
    Color::Rgb(
        mix_channel(background.0, tint.0, strength),
        mix_channel(background.1, tint.1, strength),
        mix_channel(background.2, tint.2, strength),
    )
}

#[allow(clippy::disallowed_methods)]
fn rgb(hex: u32) -> Color {
    let [_, red, green, blue] = hex.to_be_bytes();
    Color::Rgb(red, green, blue)
}

#[allow(clippy::disallowed_methods)]
fn background_color((red, green, blue): (u8, u8, u8)) -> Color {
    Color::Rgb(red, green, blue)
}

#[allow(clippy::disallowed_methods)]
fn darken_surface(surface: Color, amount: u8) -> Color {
    match surface {
        Color::Rgb(red, green, blue) => Color::Rgb(
            red.saturating_sub(amount),
            green.saturating_sub(amount),
            blue.saturating_sub(amount),
        ),
        other => other,
    }
}

#[allow(clippy::disallowed_methods)]
fn relative_surface(background: (u8, u8, u8), strength: u16) -> Color {
    let target = if is_light(background) { 0 } else { 255 };
    Color::Rgb(
        mix_channel(background.0, target, strength),
        mix_channel(background.1, target, strength),
        mix_channel(background.2, target, strength),
    )
}

fn is_light((red, green, blue): (u8, u8, u8)) -> bool {
    let luma = u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114;
    luma > 128_000
}

fn mix_channel(channel: u8, target: u8, strength: u16) -> u8 {
    let mixed = (u16::from(channel) * (100 - strength) + u16::from(target) * strength) / 100;
    u8::try_from(mixed).unwrap_or(target)
}

#[cfg(test)]
mod tests;
