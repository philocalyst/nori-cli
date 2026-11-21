//! Splits markdown source into prose and GFM table blocks.
//!
//! The TUI renders tables with the shared adaptive renderer in
//! `nori-tui-components` and everything else with the local writer, so the source has to be cut
//! into segments before either renderer runs.
//!
//! The same scanner answers a second question for the streaming path: where is it safe to stop.
//! Table rendering is *not* append-only — column widths are derived from every row, so appending a
//! body row rewrites the header, the header rule, and each separator emitted so far. A streaming
//! consumer that commits rendered lines by prefix must therefore never commit a table that can
//! still grow, which is what [`committable_prefix_len`] reports.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarkdownSegment {
    pub(crate) range: Range<usize>,
    pub(crate) is_table: bool,
}

/// Split `input` into alternating prose and table segments, in source order.
pub(crate) fn markdown_segments(input: &str) -> Vec<MarkdownSegment> {
    let lines = source_lines(input);

    let mut segments = Vec::new();
    let mut prose_start = 0;
    let mut index = 0;
    // A table never starts inside a fenced code block, however much its lines look like one.
    let mut fence: Option<crate::markdown_stream::FenceOpener> = None;
    while index + 1 < lines.len() {
        let line = input[lines[index].clone()].trim_end_matches('\n');
        if let Some(opener) = &fence {
            if opener.is_closed_by(line) {
                fence = None;
            }
            index += 1;
            continue;
        }
        if let Some(opener) = crate::markdown_stream::FenceOpener::parse(line) {
            fence = Some(opener);
            index += 1;
            continue;
        }
        let header = input[lines[index].clone()].trim();
        let delimiter = input[lines[index + 1].clone()].trim();
        if !header.contains('|') || !is_table_delimiter(delimiter) {
            index += 1;
            continue;
        }

        let table_start = lines[index].start;
        if prose_start < table_start {
            segments.push(MarkdownSegment {
                range: prose_start..table_start,
                is_table: false,
            });
        }
        let mut table_end_index = index + 2;
        while table_end_index < lines.len() {
            let row = input[lines[table_end_index].clone()].trim();
            if row.is_empty() || !row.contains('|') {
                break;
            }
            table_end_index += 1;
        }
        let table_end = lines[table_end_index.saturating_sub(1)].end;
        segments.push(MarkdownSegment {
            range: table_start..table_end,
            is_table: true,
        });
        prose_start = table_end;
        index = table_end_index;
    }
    if prose_start < input.len() {
        segments.push(MarkdownSegment {
            range: prose_start..input.len(),
            is_table: false,
        });
    }
    if segments.is_empty() {
        segments.push(MarkdownSegment {
            range: 0..input.len(),
            is_table: false,
        });
    }
    segments
}

/// Length of the prefix of `input` whose rendering cannot be changed by more source.
///
/// Two trailing constructs are excluded:
///
/// - A table that runs to the end of `input` is still open. The next row widens columns and
///   rewrites every line the table has already produced.
/// - A final line containing a pipe may be a table header whose delimiter row has not arrived yet.
///   Committing it renders the header as prose, and no later delta can take that back.
///
/// Both close as soon as a blank line, a non-table line, or stream finalization follows.
///
/// A list is held back whole while it is tight (no blank line between its parts yet): a blank
/// line still to come would make it loose, and a loose list puts a blank line between parts it
/// already showed. Once loose it stays loose, and its lines are final as they come.
pub(crate) fn committable_prefix_len(input: &str) -> usize {
    if opens_with_list_item(input) && !has_interior_blank_line(input) {
        return 0;
    }
    if let Some(segment) = markdown_segments(input).last()
        && segment.is_table
        && segment.range.end >= input.len()
    {
        return segment.range.start;
    }
    match last_line_start(input) {
        Some(start) if input[start..].contains('|') => start,
        _ => input.len(),
    }
}

/// Whether the first line with content is a list item: `-`, `*` or `+`, or a number with `.` or
/// `)`, then a space, after at most three spaces.
fn opens_with_list_item(input: &str) -> bool {
    let Some(line) = input.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let marker = if digits > 0 && digits <= 9 {
        rest[digits..].strip_prefix(['.', ')'])
    } else {
        rest.strip_prefix(['-', '*', '+'])
    };
    marker.is_some_and(|after| after.is_empty() || after.starts_with([' ', '\t']))
}

/// Whether a blank line comes between two lines with content.
fn has_interior_blank_line(input: &str) -> bool {
    let mut seen_content = false;
    let mut blank_after_content = false;
    for line in input.lines() {
        if line.trim().is_empty() {
            blank_after_content |= seen_content;
        } else if blank_after_content {
            return true;
        } else {
            seen_content = true;
        }
    }
    false
}

/// Start offset of the final line, ignoring one trailing newline. `None` when `input` has no line.
fn last_line_start(input: &str) -> Option<usize> {
    let trimmed = input.strip_suffix('\n').unwrap_or(input);
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.rfind('\n').map_or(0, |index| index + 1))
}

/// Byte ranges of each line in `input`, including its trailing newline.
fn source_lines(input: &str) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in input.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        lines.push(start..offset);
    }
    lines
}

fn is_table_delimiter(line: &str) -> bool {
    let line = line.trim_matches('|');
    let cells = line.split('|').map(str::trim).collect::<Vec<_>>();
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let rule = cell.trim_start_matches(':').trim_end_matches(':');
            !rule.is_empty() && rule.chars().all(|character| character == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const TABLE: &str = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";

    #[test]
    fn segments_split_prose_around_a_table() {
        let input = format!("intro\n\n{TABLE}\noutro\n");
        let segments = markdown_segments(&input);
        assert_eq!(
            segments,
            vec![
                MarkdownSegment {
                    range: 0..7,
                    is_table: false
                },
                MarkdownSegment {
                    range: 7..7 + TABLE.len(),
                    is_table: true
                },
                MarkdownSegment {
                    range: 7 + TABLE.len()..input.len(),
                    is_table: false
                },
            ]
        );
    }

    #[test]
    fn source_without_a_table_is_one_prose_segment() {
        let input = "just prose\nand more\n";
        assert_eq!(
            markdown_segments(input),
            vec![MarkdownSegment {
                range: 0..input.len(),
                is_table: false
            }]
        );
    }

    #[test]
    fn prefix_excludes_a_table_that_runs_to_the_end() {
        let input = format!("intro\n\n{TABLE}");
        assert_eq!(committable_prefix_len(&input), 7);
    }

    #[test]
    fn prefix_excludes_a_header_and_delimiter_with_no_body_rows() {
        let input = "intro\n\n| A | B |\n| --- | --- |\n";
        assert_eq!(committable_prefix_len(input), 7);
    }

    #[test]
    fn prefix_keeps_a_table_closed_by_a_blank_line() {
        let input = format!("{TABLE}\n");
        assert_eq!(committable_prefix_len(&input), input.len());
    }

    #[test]
    fn prefix_keeps_a_table_closed_by_following_prose() {
        let input = format!("{TABLE}outro\n");
        assert_eq!(committable_prefix_len(&input), input.len());
    }

    #[test]
    fn prefix_keeps_source_without_a_table() {
        let input = "no tables here\n";
        assert_eq!(committable_prefix_len(input), input.len());
    }

    #[test]
    fn prefix_excludes_a_trailing_header_awaiting_its_delimiter() {
        let input = "intro\n\n| A | B |\n";
        assert_eq!(committable_prefix_len(input), 7);
    }

    #[test]
    fn prefix_keeps_a_pipe_line_once_a_blank_line_rules_out_a_delimiter() {
        let input = "a | b\n\n";
        assert_eq!(committable_prefix_len(input), input.len());
    }

    #[test]
    fn prefix_keeps_an_earlier_pipe_line_followed_by_prose() {
        let input = "use `a | b` syntax\nnext line\n";
        assert_eq!(committable_prefix_len(input), input.len());
    }

    #[test]
    fn prefix_excludes_only_the_trailing_table() {
        let input = format!("{TABLE}\nmiddle\n\n{TABLE}");
        let first_table_and_prose = TABLE.len() + "\nmiddle\n\n".len();
        assert_eq!(committable_prefix_len(&input), first_table_and_prose);
    }

    #[test]
    fn delimiter_rows_accept_alignment_colons() {
        assert!(is_table_delimiter("| :-- | :-: | --: |"));
        assert!(is_table_delimiter("--- | ---"));
        assert!(!is_table_delimiter("| a | b |"));
        assert!(!is_table_delimiter("| | |"));
    }
}
