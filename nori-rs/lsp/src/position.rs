//! Positions: 1-based line and character columns on the Nori side, line and code-unit offsets on
//! the language-server side (UTF-8, UTF-16 or UTF-32 as negotiated), and locating a symbol name on
//! a line so callers do not have to count columns.

use helix_lsp::OffsetEncoding;
use helix_lsp::lsp;

use crate::error::LspError;
use crate::error::LspResult;
use crate::types::Range;
use crate::types::ResolvedTarget;
use crate::types::Target;
use crate::types::TextEdit;

/// How far from the requested line a `symbol` is searched for.
const SYMBOL_SEARCH_RADIUS: usize = 10;

/// Splits like the protocol: `\n`, `\r\n` and `\r` end a line, and a final terminator leaves an
/// empty last line.
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..index]);
                start = index + 1;
            }
            b'\r' => {
                lines.push(&text[start..index]);
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    lines.push(&text[start..]);
    lines
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn to_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn to_index(value: i32) -> usize {
    usize::try_from(value).unwrap_or(0)
}

/// The 0-based character offsets at which `symbol` starts on `line`: whole-word matches when there
/// are any, otherwise plain substring matches.
fn symbol_columns(line: &str, symbol: &str) -> Vec<usize> {
    if symbol.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = line.chars().collect();
    let needle: Vec<char> = symbol.chars().collect();
    let mut whole = Vec::new();
    let mut partial = Vec::new();
    if chars.len() >= needle.len() {
        for start in 0..=chars.len() - needle.len() {
            if chars[start..start + needle.len()] != needle[..] {
                continue;
            }
            let before_ok = start == 0 || !is_word_char(chars[start - 1]);
            let after = start + needle.len();
            let after_ok = after == chars.len() || !is_word_char(chars[after]);
            if before_ok && after_ok {
                whole.push(start);
            }
            partial.push(start);
        }
    }
    if whole.is_empty() { partial } else { whole }
}

fn nearest(columns: &[usize], wanted: Option<usize>) -> Option<usize> {
    match wanted {
        Some(wanted) => columns
            .iter()
            .copied()
            .min_by_key(|column| column.abs_diff(wanted)),
        None => columns.first().copied(),
    }
}

fn word_at(line: &str, column: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut start = column.min(chars.len());
    let mut end = start;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    while end < chars.len() && is_word_char(chars[end]) {
        end += 1;
    }
    chars[start..end].iter().collect()
}

/// Finds the position a [`Target`] names inside `text`. The `symbol` wins over `column`.
pub fn resolve_target(text: &str, target: &Target) -> LspResult<ResolvedTarget> {
    let lines = split_lines(text);
    if target.line < 1 {
        return Err(LspError::InvalidPosition {
            message: format!("line numbers start at 1, got {}", target.line),
        });
    }
    let requested = to_index(target.line) - 1;
    if requested >= lines.len() {
        return Err(LspError::InvalidPosition {
            message: format!(
                "{} has {} lines; line {} does not exist",
                target.path.display(),
                lines.len(),
                target.line
            ),
        });
    }
    let wanted_column = target
        .column
        .filter(|column| *column >= 1)
        .map(|column| to_index(column) - 1);

    let (line_index, column) = match target.symbol.as_deref().filter(|symbol| !symbol.is_empty()) {
        Some(symbol) => {
            let mut found = None;
            for distance in 0..=SYMBOL_SEARCH_RADIUS {
                let candidates = [requested.checked_sub(distance), Some(requested + distance)];
                for line_index in candidates.into_iter().flatten() {
                    let Some(line) = lines.get(line_index) else {
                        continue;
                    };
                    let columns = symbol_columns(line, symbol);
                    let wanted = if distance == 0 { wanted_column } else { None };
                    if let Some(column) = nearest(&columns, wanted) {
                        found = Some((line_index, column));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found.ok_or_else(|| LspError::InvalidPosition {
                message: format!(
                    "`{symbol}` does not appear within {SYMBOL_SEARCH_RADIUS} lines of line {} in {}",
                    target.line,
                    target.path.display()
                ),
            })?
        }
        None => {
            let line = lines[requested];
            let column = match wanted_column {
                Some(column) => column.min(line.chars().count()),
                None => line
                    .chars()
                    .position(is_word_char)
                    .unwrap_or_else(|| line.chars().take_while(|ch| ch.is_whitespace()).count()),
            };
            (requested, column)
        }
    };

    let word = word_at(lines[line_index], column);
    let adjusted = line_index != requested
        || target
            .column
            .is_some_and(|asked| to_index(asked.max(1)) != column + 1);
    Ok(ResolvedTarget {
        path: target.path.clone(),
        line: to_i32(line_index + 1),
        column: to_i32(column + 1),
        word,
        adjusted,
    })
}

/// The protocol position of the 1-based character `column` on `line`.
pub fn to_lsp_position(
    lines: &[&str],
    line: i32,
    column: i32,
    encoding: OffsetEncoding,
) -> lsp::Position {
    let line_index = to_index(line.max(1)) - 1;
    let text = lines.get(line_index).copied().unwrap_or("");
    let chars_before = to_index(column.max(1)) - 1;
    let units: usize = text
        .chars()
        .take(chars_before)
        .map(|ch| unit_len(ch, encoding))
        .sum();
    lsp::Position::new(
        u32::try_from(line_index).unwrap_or(u32::MAX),
        u32::try_from(units).unwrap_or(u32::MAX),
    )
}

fn unit_len(ch: char, encoding: OffsetEncoding) -> usize {
    match encoding {
        OffsetEncoding::Utf8 => ch.len_utf8(),
        OffsetEncoding::Utf16 => ch.len_utf16(),
        OffsetEncoding::Utf32 => 1,
    }
}

/// The 1-based line and character column of a protocol position. A character past the end of the
/// line lands on the line end, as the protocol says.
pub fn from_lsp_position(
    lines: &[&str],
    position: lsp::Position,
    encoding: OffsetEncoding,
) -> (i32, i32) {
    let line_index = usize::try_from(position.line).unwrap_or(usize::MAX);
    let Some(text) = lines.get(line_index) else {
        // Past the last line: the end of the file.
        let last = lines.len().saturating_sub(1);
        let width = lines.get(last).map_or(0, |line| line.chars().count());
        return (to_i32(last + 1), to_i32(width + 1));
    };
    let wanted = usize::try_from(position.character).unwrap_or(usize::MAX);
    let mut units = 0;
    let mut chars = 0;
    for ch in text.chars() {
        let next = units + unit_len(ch, encoding);
        if next > wanted {
            break;
        }
        units = next;
        chars += 1;
    }
    (to_i32(line_index + 1), to_i32(chars + 1))
}

pub fn from_lsp_range(lines: &[&str], range: lsp::Range, encoding: OffsetEncoding) -> Range {
    let (line, column) = from_lsp_position(lines, range.start, encoding);
    let (end_line, end_column) = from_lsp_position(lines, range.end, encoding);
    Range {
        line,
        column,
        end_line,
        end_column,
    }
}

/// Applies `edits` (file coordinates) to `text`. Edits must not overlap; they may come in any
/// order. Line endings of untouched text are preserved.
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> LspResult<String> {
    let mut starts = vec![0];
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => starts.push(index + 1),
            b'\r' => {
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                starts.push(index + 1);
            }
            _ => {}
        }
        index += 1;
    }
    let line_text = |line: i32| -> &str {
        let index = to_index(line.max(1)) - 1;
        let start = starts.get(index).copied().unwrap_or(text.len());
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        text[start..end].trim_end_matches(['\n', '\r'])
    };
    let offset = |line: i32, column: i32| -> usize {
        let index = to_index(line.max(1)) - 1;
        let Some(start) = starts.get(index).copied() else {
            return text.len();
        };
        let line = line_text(line);
        let within: usize = line
            .chars()
            .take(to_index(column.max(1)) - 1)
            .map(char::len_utf8)
            .sum();
        start + within
    };

    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            (
                offset(edit.range.line, edit.range.column),
                offset(edit.range.end_line, edit.range.end_column),
                edit.new_text.as_str(),
            )
        })
        .collect();
    spans.sort_by_key(|(start, end, _)| (*start, *end));
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, replacement) in spans {
        if start < cursor || end < start {
            return Err(LspError::InvalidPosition {
                message: "the edits overlap".to_owned(),
            });
        }
        out.push_str(&text[cursor..start]);
        out.push_str(replacement);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn target(line: i32) -> Target {
        Target::new("a.rs", line)
    }

    #[test]
    fn splits_on_all_terminators() {
        assert_eq!(split_lines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        assert_eq!(split_lines("a\n"), vec!["a", ""]);
        assert_eq!(split_lines(""), vec![""]);
    }

    #[test]
    fn symbol_wins_over_a_wrong_column() {
        let text = "fn main() {\n    let value = compute(value_two);\n}\n";
        let resolved = resolve_target(text, &target(2).column(1).symbol("compute")).unwrap();
        assert_eq!((resolved.line, resolved.column), (2, 17));
        assert_eq!(resolved.word, "compute");
        assert!(resolved.adjusted);
    }

    #[test]
    fn symbol_prefers_whole_words_and_the_closest_occurrence() {
        let text = "let a = fooo + foo + foo;";
        let resolved = resolve_target(text, &target(1).column(21).symbol("foo")).unwrap();
        assert_eq!(resolved.column, 22);
        let first = resolve_target(text, &target(1).symbol("foo")).unwrap();
        assert_eq!(first.column, 16);
    }

    #[test]
    fn symbol_is_searched_on_nearby_lines() {
        let text = "one\ntwo\nthree needle\nfour";
        let resolved = resolve_target(text, &target(1).symbol("needle")).unwrap();
        assert_eq!((resolved.line, resolved.column), (3, 7));
        assert!(resolved.adjusted);
        let missing = resolve_target(text, &target(1).symbol("absent"));
        assert!(matches!(missing, Err(LspError::InvalidPosition { .. })));
    }

    #[test]
    fn missing_column_picks_the_first_identifier() {
        let resolved = resolve_target("    let x = 1;", &target(1)).unwrap();
        assert_eq!((resolved.column, resolved.word.as_str()), (5, "let"));
    }

    #[test]
    fn rejects_lines_outside_the_file() {
        assert!(resolve_target("x", &target(0)).is_err());
        assert!(resolve_target("x", &target(5)).is_err());
    }

    #[test]
    fn encodings_count_code_units() {
        let lines = ["a\u{1F600}b"];
        let utf16 = to_lsp_position(&lines, 1, 3, OffsetEncoding::Utf16);
        assert_eq!(utf16.character, 3);
        let utf8 = to_lsp_position(&lines, 1, 3, OffsetEncoding::Utf8);
        assert_eq!(utf8.character, 5);
        let utf32 = to_lsp_position(&lines, 1, 3, OffsetEncoding::Utf32);
        assert_eq!(utf32.character, 2);
        assert_eq!(
            from_lsp_position(&lines, lsp::Position::new(0, 3), OffsetEncoding::Utf16),
            (1, 3)
        );
        assert_eq!(
            from_lsp_position(&lines, lsp::Position::new(0, 99), OffsetEncoding::Utf16),
            (1, 4)
        );
    }

    #[test]
    fn applies_edits_in_any_order_and_keeps_crlf() {
        let text = "let a = 1;\r\nlet b = a;\r\n";
        let edit = |line, column, end_column, new_text: &str| TextEdit {
            range: Range {
                line,
                column,
                end_line: line,
                end_column,
            },
            new_text: new_text.to_owned(),
        };
        let out =
            apply_edits(text, &[edit(2, 9, 10, "renamed"), edit(1, 5, 6, "renamed")]).unwrap();
        assert_eq!(out, "let renamed = 1;\r\nlet b = renamed;\r\n");
    }
}
