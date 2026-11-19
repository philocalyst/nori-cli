//! Highlighting query terms inside already-styled lines.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// Byte ranges of every occurrence of `terms` in `text`, merged and sorted. A term with no
/// uppercase letter matches regardless of case.
fn occurrences(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let folded = text.to_lowercase();
    // Lowercasing may change byte lengths for a few scripts; fall back to exact search then.
    let aligned = folded.len() == text.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms.iter().filter(|term| !term.is_empty()) {
        let (haystack, needle) = if term.chars().any(char::is_uppercase) || !aligned {
            (text, term.clone())
        } else {
            (folded.as_str(), term.to_lowercase())
        };
        let mut from = 0;
        while let Some(found) = haystack[from..].find(&needle) {
            let start = from + found;
            let end = start + needle.len();
            if text.is_char_boundary(start) && text.is_char_boundary(end) {
                ranges.push((start, end));
            }
            from = end.max(start + 1);
            while from < haystack.len() && !haystack.is_char_boundary(from) {
                from += 1;
            }
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            Some(_) | None => merged.push((start, end)),
        }
    }
    merged
}

/// Whether any of `terms` occurs in the plain text of `line`.
pub(super) fn line_matches(line: &Line<'_>, terms: &[String]) -> bool {
    if terms.is_empty() {
        return false;
    }
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    !occurrences(&text, terms).is_empty()
}

/// `line` with every occurrence of `terms` set in `emphasis` on top of the span's own style.
pub fn highlight_terms(line: Line<'static>, terms: &[String], emphasis: Style) -> Line<'static> {
    if terms.iter().all(String::is_empty) {
        return line;
    }
    let emphasis = emphasis.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let ranges = occurrences(span.content.as_ref(), terms);
        if ranges.is_empty() {
            spans.push(span);
            continue;
        }
        let text = span.content.as_ref();
        let mut cursor = 0;
        for (start, end) in ranges {
            if start > cursor {
                spans.push(Span::styled(text[cursor..start].to_owned(), span.style));
            }
            spans.push(Span::styled(
                text[start..end].to_owned(),
                span.style.patch(emphasis),
            ));
            cursor = end;
        }
        if cursor < text.len() {
            spans.push(Span::styled(text[cursor..].to_owned(), span.style));
        }
    }
    Line::from(spans).style(line.style)
}

/// Set the characters at `indices` of `text` (character positions) in `emphasis`, cutting the
/// text to `max_width` columns with an ellipsis. Returns the spans and the columns they take.
pub fn highlight_indices(
    text: &str,
    indices: &[usize],
    base: Style,
    emphasis: Style,
    max_width: usize,
) -> (Vec<Span<'static>>, usize) {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    let total = UnicodeWidthStr::width(text);
    let limit = if total > max_width {
        max_width.saturating_sub(1)
    } else {
        max_width
    };
    let emphasis = base.patch(emphasis).add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    let mut used = 0;
    let mut matches = indices.iter().copied().peekable();
    let mut cut = false;
    for (index, character) in text.chars().enumerate() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + width > limit {
            cut = true;
            break;
        }
        used += width;
        let matched = matches.next_if_eq(&index).is_some();
        if matched != run_matched && !run.is_empty() {
            let style = if run_matched { emphasis } else { base };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_matched = matched;
        run.push(character);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_matched { emphasis } else { base }));
    }
    if cut {
        spans.push(Span::styled("…", base));
        used += 1;
    }
    (spans, used)
}
