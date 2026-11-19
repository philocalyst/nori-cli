use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use std::path::Path;
use syntect::highlighting::FontStyle;
use syntect::parsing::SyntaxSet;

/// Maximum input size (512 KB) before we fall back to plain text.
const MAX_INPUT_BYTES: i64 = 512 * 1024;

/// Maximum number of lines before we fall back to plain text.
const MAX_INPUT_LINES: i64 = 10_000;

fn syntax_set() -> &'static SyntaxSet {
    use std::sync::OnceLock;
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(two_face::syntax::extra_newlines)
}

#[allow(clippy::disallowed_methods)]
fn syntect_color_to_ratatui(c: syntect::highlighting::Color) -> Color {
    if c.a == 0x01 {
        // Terminal default
        Color::default()
    } else if c.a == 0x00 {
        // ANSI palette index stored in red component
        Color::Indexed(c.r)
    } else {
        // True RGB color
        Color::Rgb(c.r, c.g, c.b)
    }
}

fn syntect_style_to_ratatui(style: syntect::highlighting::Style) -> Style {
    let fg = syntect_color_to_ratatui(style.foreground);
    let mut ratatui_style = Style::default().fg(fg);
    if style.font_style.contains(FontStyle::BOLD) {
        ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
    }
    ratatui_style
}

/// Spans for one highlighted line, without its newline.
fn region_spans(regions: Vec<(syntect::highlighting::Style, &str)>) -> Vec<Span<'static>> {
    regions
        .into_iter()
        .filter_map(|(style, text)| {
            // Strip the trailing newline since a Line represents a single line.
            let text = text.strip_suffix('\n').unwrap_or(text);
            (!text.is_empty())
                .then(|| Span::styled(text.to_string(), syntect_style_to_ratatui(style)))
        })
        .collect()
}

/// Highlights a code block one line at a time, carrying the parser state from line to line, so a
/// block that streams in is highlighted once in total instead of once per frame.
///
/// Lines come out exactly as [`highlight_code_to_lines`] produces them for the whole block, except
/// that the size limits there do not apply: the cost here is linear however long the block is.
pub(crate) struct LineHighlighter {
    parse: syntect::parsing::ParseState,
    highlight: syntect::highlighting::HighlightState,
    highlighter: syntect::highlighting::Highlighter<'static>,
}

impl LineHighlighter {
    /// `None` when the language is unknown; such blocks are shown plain.
    pub(crate) fn for_lang(lang: &str) -> Option<Self> {
        let syntax = syntax_set().find_syntax_by_token(lang)?;
        let highlighter = syntect::highlighting::Highlighter::new(super::syntax_theme::current());
        let highlight = syntect::highlighting::HighlightState::new(
            &highlighter,
            syntect::parsing::ScopeStack::new(),
        );
        Some(Self {
            parse: syntect::parsing::ParseState::new(syntax),
            highlight,
            highlighter,
        })
    }

    /// Spans for the next line of the block, given with its newline. `None` when the highlighter
    /// fails, after which the block should continue plain.
    pub(crate) fn highlight(&mut self, line: &str) -> Option<Vec<Span<'static>>> {
        let ops = self.parse.parse_line(line, syntax_set()).ok()?;
        let regions = syntect::highlighting::HighlightIterator::new(
            &mut self.highlight,
            &ops,
            line,
            &self.highlighter,
        )
        .collect();
        Some(region_spans(regions))
    }

    /// Spans for a line still being typed, leaving the state for the line's final form.
    pub(crate) fn preview(&self, partial: &str) -> Option<Vec<Span<'static>>> {
        let mut parse = self.parse.clone();
        let mut highlight = self.highlight.clone();
        let ops = parse.parse_line(partial, syntax_set()).ok()?;
        let regions = syntect::highlighting::HighlightIterator::new(
            &mut highlight,
            &ops,
            partial,
            &self.highlighter,
        )
        .collect();
        Some(region_spans(regions))
    }
}

/// Languages the shell highlighter owns, so a fenced `bash` block, an exec cell and the
/// composer's `!` mode all paint a command the same way.
pub(crate) fn is_shell_language(lang: &str) -> bool {
    matches!(
        lang.trim().to_ascii_lowercase().as_str(),
        "bash" | "sh" | "shell" | "zsh" | "console" | "shell-session" | "shellsession"
    )
}

fn plain_lines(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|l| Line::from(l.to_string()))
        .chain(text.ends_with('\n').then(|| Line::from("")))
        .collect()
}

fn is_too_large(input: &str) -> bool {
    input.len() as i64 > MAX_INPUT_BYTES || input.lines().count() as i64 > MAX_INPUT_LINES
}

/// Highlight source code in the given language into styled ratatui `Line`s.
///
/// Falls back to plain unstyled text if the language is unknown, or if
/// the input exceeds safety limits (512 KB or 10 000 lines).
pub(crate) fn highlight_code_to_lines(code: &str, lang: &str) -> Vec<Line<'static>> {
    if code.is_empty() {
        return vec![Line::from("")];
    }

    if is_too_large(code) {
        return plain_lines(code);
    }

    if is_shell_language(lang) {
        return super::bash_highlight::highlight_bash_script(code);
    }

    let ss = syntax_set();
    let syntax = match ss.find_syntax_by_token(lang) {
        Some(s) => s,
        None => return plain_lines(code),
    };

    let theme = super::syntax_theme::current();
    let mut highlighter = syntect::easy::HighlightLines::new(syntax, theme);

    let mut result: Vec<Line<'static>> = Vec::new();

    for line_str in syntect::util::LinesWithEndings::from(code) {
        let regions = match highlighter.highlight_line(line_str, ss) {
            Ok(r) => r,
            Err(_) => return plain_lines(code),
        };
        result.push(Line::from(region_spans(regions)));
    }

    if result.is_empty() {
        vec![Line::from("")]
    } else {
        result
    }
}

/// Highlight source code by inferring the language from a file path.
pub(crate) fn highlight_code_to_lines_for_path(code: &str, path: &Path) -> Vec<Line<'static>> {
    if code.is_empty() {
        return vec![Line::from("")];
    }

    if is_too_large(code) {
        return plain_lines(code);
    }

    let ss = syntax_set();
    let syntax = match ss.find_syntax_for_file(path) {
        Ok(Some(s)) => s,
        Ok(None) | Err(_) => return plain_lines(code),
    };

    let theme = super::syntax_theme::current();
    let mut highlighter = syntect::easy::HighlightLines::new(syntax, theme);
    let mut result: Vec<Line<'static>> = Vec::new();

    for line_str in syntect::util::LinesWithEndings::from(code) {
        let regions = match highlighter.highlight_line(line_str, ss) {
            Ok(r) => r,
            Err(_) => return plain_lines(code),
        };

        let spans: Vec<Span<'static>> = regions
            .into_iter()
            .map(|(style, text)| {
                let ratatui_style = syntect_style_to_ratatui(style);
                let text = text.strip_suffix('\n').unwrap_or(text);
                if text.is_empty() {
                    return Span::from("".to_string());
                }
                Span::styled(text.to_string(), ratatui_style)
            })
            .filter(|s| !s.content.is_empty())
            .collect();

        result.push(Line::from(spans));
    }

    if result.is_empty() {
        vec![Line::from("")]
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn reconstructed(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn has_non_default_fg(lines: &[Line<'static>]) -> bool {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|sp| sp.style.fg.is_some() && sp.style.fg != Some(Color::default()))
    }

    #[test]
    fn highlight_bash_produces_colored_spans() {
        let lines = highlight_code_to_lines("echo hello", "bash");
        assert!(
            has_non_default_fg(&lines),
            "expected at least one span with a non-default fg color, got: {lines:?}"
        );
    }

    #[test]
    fn highlight_unknown_lang_returns_plain() {
        let lines = highlight_code_to_lines("some random text", "zzz_nonexistent");
        for line in &lines {
            for span in &line.spans {
                assert_eq!(
                    span.style,
                    Style::default(),
                    "expected default style for unknown language, got: {span:?}"
                );
            }
        }
    }

    #[test]
    fn highlight_code_to_lines_rust() {
        let lines = highlight_code_to_lines("fn main() {}", "rust");
        assert!(
            has_non_default_fg(&lines),
            "expected colored output for Rust code, got: {lines:?}"
        );
    }

    #[test]
    fn highlight_code_to_lines_for_path_rust() {
        let lines = highlight_code_to_lines_for_path("fn main() {}", Path::new("src/main.rs"));
        assert!(
            has_non_default_fg(&lines),
            "expected colored output for Rust path, got: {lines:?}"
        );
    }

    #[test]
    fn highlight_large_input_returns_plain() {
        // Create input that exceeds 512KB
        let big = "x".repeat(512 * 1024 + 1);
        let lines = highlight_code_to_lines(&big, "bash");
        // All spans should have default style (plain text)
        for line in &lines {
            for span in &line.spans {
                assert_eq!(
                    span.style,
                    Style::default(),
                    "expected plain text for large input, got styled span: {span:?}"
                );
            }
        }
    }

    #[test]
    fn highlight_bash_to_lines_preserves_text() {
        let input = "echo \"hello world\"\nls -la\n";
        let lines = highlight_code_to_lines(input, "bash");
        let text = reconstructed(&lines);
        // Each Line represents a line without its trailing newline, so joining
        // with "\n" does not reproduce a trailing newline from the original input.
        assert_eq!(text, "echo \"hello world\"\nls -la");
    }

    #[test]
    fn highlight_piped_command_produces_multiple_colors() {
        // A piped shell command like "df -h --total 2>/dev/null | tail -1"
        // should produce spans with at least two distinct foreground colors,
        // confirming that the highlighter treats different tokens differently.
        let lines = highlight_code_to_lines("df -h --total 2>/dev/null | tail -1", "bash");
        let distinct_colors: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|sp| sp.style.fg)
            .collect();
        assert!(
            distinct_colors.len() >= 2,
            "expected at least 2 distinct fg colors for a piped command, got {}: {distinct_colors:?}",
            distinct_colors.len(),
        );
    }
}
