//! The one shell highlighter: brush-parser tokens and AST (through `nori_bash`) painted with the
//! live [`Theme`]'s tokens.
//!
//! Exec cells, `!` user-shell cells, subagent activity rows, picker previews and the composer's
//! `!` mode all come here, so a command looks the same wherever it is shown.

use nori_bash::HighlightKind;
use nori_tui_components::Theme;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// The style of each kind of shell token, from the theme's tokens.
#[derive(Clone, Copy)]
pub(crate) struct ShellStyles {
    command: Style,
    subcommand: Style,
    flag: Style,
    variable: Style,
    string: Style,
    number: Style,
    operator: Style,
    keyword: Style,
    function: Style,
    comment: Style,
}

impl ShellStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        let syntax = &theme.syntax;
        Self {
            command: theme.accent_mint.add_modifier(Modifier::BOLD),
            subcommand: syntax.r#type,
            flag: syntax.flag,
            variable: syntax.variable,
            string: syntax.string,
            number: theme.accent_amber,
            operator: syntax.operator.add_modifier(Modifier::BOLD),
            keyword: syntax.keyword,
            function: syntax.function,
            comment: syntax.comment,
        }
    }

    pub(crate) fn live() -> Self {
        Self::from_theme(&crate::history_cell::chrome::live_theme())
    }

    fn of(&self, kind: HighlightKind) -> Style {
        match kind {
            HighlightKind::Command => self.command,
            HighlightKind::Subcommand => self.subcommand,
            HighlightKind::Keyword => self.keyword,
            HighlightKind::FunctionDef => self.function,
            HighlightKind::Flag => self.flag,
            HighlightKind::Variable => self.variable,
            HighlightKind::String => self.string,
            HighlightKind::Number => self.number,
            HighlightKind::Operator => self.operator,
            HighlightKind::Comment => self.comment,
            HighlightKind::Argument => Style::default(),
        }
    }
}

/// Highlight a shell command or script into styled spans. Newlines stay inside the spans; see
/// [`highlight_shell_lines`] to split them into rows.
pub(crate) fn highlight_shell(cmd: &str, styles: &ShellStyles) -> Vec<Span<'static>> {
    if cmd.is_empty() {
        return Vec::new();
    }
    let mut spans = nori_bash::highlight_spans(cmd);
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));

    let mut out = Vec::with_capacity(spans.len().saturating_mul(2));
    let mut cursor = 0usize;
    for span in spans {
        let (Ok(start), Ok(end)) = (
            usize::try_from(span.start_byte),
            usize::try_from(span.end_byte),
        ) else {
            continue;
        };
        let start = start.min(cmd.len());
        let end = end.min(cmd.len());
        if end <= start || end <= cursor {
            continue;
        }
        let start = start.max(cursor);
        if !cmd.is_char_boundary(start) || !cmd.is_char_boundary(end) {
            continue;
        }
        if start > cursor && cmd.is_char_boundary(cursor) {
            out.push(Span::raw(cmd[cursor..start].to_owned()));
        }
        out.push(Span::styled(
            cmd[start..end].to_owned(),
            styles.of(span.kind),
        ));
        cursor = end;
    }
    if cursor < cmd.len() && cmd.is_char_boundary(cursor) {
        out.push(Span::raw(cmd[cursor..].to_owned()));
    }
    out
}

/// A script highlighted as a whole (so heredocs and multi-line strings parse) and split into one
/// row of spans per line.
pub(crate) fn highlight_shell_lines(script: &str, styles: &ShellStyles) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for span in highlight_shell(script, styles) {
        let mut parts = span.content.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                rows.last_mut()
                    .into_iter()
                    .for_each(|row| row.push(Span::styled(part.to_owned(), span.style)));
            }
            if parts.peek().is_some() {
                rows.push(Vec::new());
            }
        }
    }
    rows
}

/// Highlight one command line with the live theme.
pub(crate) fn highlight_bash_spans(cmd: &str) -> Vec<Span<'static>> {
    highlight_shell(cmd, &ShellStyles::live())
}

/// Highlight a multi-line script with the live theme.
pub(crate) fn highlight_bash_script(script: &str) -> Vec<Line<'static>> {
    if script.is_empty() {
        return vec![Line::from("")];
    }
    highlight_shell_lines(script, &ShellStyles::live())
        .into_iter()
        .map(Line::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn styles() -> ShellStyles {
        ShellStyles::from_theme(&Theme::for_terminal_background(Some((20, 16, 22))))
    }

    #[test]
    fn highlights_commands_subcommands_flags_vars_strings_and_operators() {
        let cmd = r#"check_wake() { cargo test -p "$1" -- --nocapture; } && git diff"#;
        let styles = styles();
        let spans = highlight_shell(cmd, &styles);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, cmd);
        let style_of = |needle: &str| {
            spans
                .iter()
                .find(|span| span.content.contains(needle))
                .map(|span| span.style)
        };
        assert_eq!(style_of("check_wake"), Some(styles.function));
        assert_eq!(style_of("cargo"), Some(styles.command));
        assert_eq!(style_of("-p"), Some(styles.flag));
        assert_eq!(style_of("&&"), Some(styles.operator));
    }
}
