use std::path::Path;

use pulldown_cmark::Event;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use ratatui::text::Line;
use ratatui::text::Text;

mod table_segments;

pub(crate) use table_segments::committable_prefix_len;
pub(crate) use table_segments::markdown_segments;

/// Rows a line wraps into beyond which it is wrapped first-fit rather than optimally.
pub(crate) const LONG_LINE_ROWS: usize = 16;

pub fn render_markdown_text(input: &str) -> Text<'static> {
    render_markdown_text_with_width(input, None)
}

pub(crate) fn render_markdown_text_with_width(input: &str, width: Option<usize>) -> Text<'static> {
    render_markdown_text_with_width_and_cwd(input, width, None)
}

pub(crate) fn render_markdown_text_with_width_and_cwd(
    input: &str,
    width: Option<usize>,
    cwd: Option<&Path>,
) -> Text<'static> {
    Text::from(render_markdown_continuation(input, width, cwd, false))
}

/// Render `input` as the continuation of earlier output when `preceded`.
///
/// Rendering a document in pieces must give the lines of rendering it in one go. The only state a
/// block inherits from the blocks before it is that they ended, so it opens with a blank separator
/// line. `preceded` reproduces exactly that state for the first segment of `input`, which lets the
/// streaming path render each block once and never again. A table is rendered on its own, and is
/// set apart from whatever comes before and after it by a blank line, as every block is.
pub(crate) fn render_markdown_continuation(
    input: &str,
    width: Option<usize>,
    cwd: Option<&Path>,
    preceded: bool,
) -> Vec<Line<'static>> {
    let segments = table_segments::markdown_segments(input);
    if segments.iter().any(|segment| segment.is_table) {
        let mut lines = Vec::new();
        for segment in segments {
            let source = &input[segment.range];
            let follows = preceded || !lines.is_empty();
            if segment.is_table {
                let mut markdown = nori_tui_components::Markdown::new(source);
                if let Some(width) = width.and_then(|width| u16::try_from(width).ok()) {
                    markdown = markdown.width(width);
                }
                let rendered = markdown.render_text();
                if follows && !rendered.lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.extend(rendered.lines);
            } else {
                lines.extend(render_markdown_without_tables(source, width, cwd, follows).lines);
            }
        }
        return lines;
    }
    render_markdown_without_tables(input, width, cwd, preceded).lines
}

/// Byte offsets at which each top-level block of `input` starts, snapped to the start of its line.
pub(crate) fn top_level_block_starts(input: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut depth = 0usize;
    for (event, range) in Parser::new_ext(input, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    starts.push(range.start);
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ if depth == 0 => starts.push(range.start),
            _ => {}
        }
    }
    for start in &mut starts {
        *start = input[..*start].rfind('\n').map_or(0, |newline| newline + 1);
    }
    starts.dedup();
    starts
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options
}

fn render_markdown_without_tables(
    input: &str,
    width: Option<usize>,
    cwd: Option<&Path>,
    preceded: bool,
) -> Text<'static> {
    let parser = Parser::new_ext(input, markdown_options());
    let mut w = Writer::new(parser, width, cwd.map(Path::to_path_buf));
    if preceded {
        // A blank line stands in for the earlier output; it is dropped again below.
        w.text.lines.push(Line::default());
        w.needs_newline = true;
    }
    w.run();
    if preceded {
        w.text.lines.remove(0);
    }
    w.text
}

mod writer;

use writer::Writer;

#[cfg(test)]
mod markdown_render_tests {
    include!("markdown_render_tests.rs");
}

#[cfg(test)]
mod tests;
