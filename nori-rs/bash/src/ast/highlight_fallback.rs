//! Fallback lexical highlighter for incomplete/unclosed Bash input.

use super::highlight::HighlightKind;
use super::highlight::HighlightSpan;
use super::highlight::push_span;
use super::highlight_hints::is_number;
use super::highlight_hints::is_shell_keyword;

pub(super) fn highlight_incomplete_fallback(input: &str) -> Vec<HighlightSpan> {
    let mut spans = Vec::new();
    let mut idx = 0usize;
    let mut expect_command = true;
    while idx < input.len() {
        let rest = &input[idx..];
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch == ' ' || ch == '\t' || ch == '\n' {
            if ch == '\n' {
                expect_command = true;
            }
            idx += ch.len_utf8();
            continue;
        }
        if ch == '#' {
            let len = rest.find('\n').unwrap_or(rest.len());
            push_span(&mut spans, idx, idx + len, HighlightKind::Comment);
            idx += len;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            let start = idx;
            idx += 1;
            while idx < input.len() {
                let c = input[idx..].chars().next().unwrap_or(ch);
                idx += c.len_utf8();
                if ch == '"' && c == '\\' && idx < input.len() {
                    let esc = input[idx..].chars().next().unwrap_or(' ');
                    idx += esc.len_utf8();
                    continue;
                }
                if c == ch {
                    break;
                }
            }
            push_span(&mut spans, start, idx, HighlightKind::String);
            expect_command = false;
            continue;
        }
        if matches!(ch, '|' | '&' | ';' | '<' | '>' | '(' | ')') {
            let start = idx;
            idx += 1;
            if idx < input.len()
                && matches!(
                    &input[start..=idx],
                    "&&" | "||" | ">>" | "<<" | ">&" | "<&" | ";;"
                )
            {
                idx += 1;
            }
            push_span(&mut spans, start, idx, HighlightKind::Operator);
            if !matches!(&input[start..idx], "<" | ">" | ">>" | "<<" | ">&" | "<&") {
                expect_command = true;
            }
            continue;
        }
        let start = idx;
        while idx < input.len() {
            let c = input[idx..].chars().next().unwrap_or(' ');
            if c.is_ascii_whitespace()
                || matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '\'' | '"')
            {
                break;
            }
            idx += c.len_utf8();
        }
        let word = &input[start..idx];
        let kind = if expect_command {
            expect_command = false;
            if is_shell_keyword(word) {
                expect_command = true;
                HighlightKind::Keyword
            } else {
                HighlightKind::Command
            }
        } else if word.starts_with('-') {
            HighlightKind::Flag
        } else if word.starts_with('$') {
            HighlightKind::Variable
        } else if is_number(word) {
            HighlightKind::Number
        } else {
            HighlightKind::Argument
        };
        push_span(&mut spans, start, idx, kind);
    }
    spans
}
