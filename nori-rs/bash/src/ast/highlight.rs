//! Token- and AST-driven Bash syntax highlighter powered by `brush-parser`.

use brush_parser::ParserOptions;
use brush_parser::Token;
use brush_parser::parse_tokens;
use brush_parser::tokenize_str;
use brush_parser::word::WordPiece;
use serde::Deserialize;
use serde::Serialize;

use super::highlight_fallback::highlight_incomplete_fallback;
use super::highlight_hints::AstHints;
use super::highlight_hints::collect_ast_hints;
use super::highlight_hints::is_assignment_word;
use super::highlight_hints::is_number;
use super::highlight_hints::is_quoted;
use super::highlight_hints::is_redirect_operator;
use super::highlight_hints::is_shell_keyword;
use super::highlight_hints::takes_subcommand;

/// Syntax token classification for Bash input highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HighlightKind {
    Command,
    Subcommand,
    Keyword,
    FunctionDef,
    Flag,
    Variable,
    String,
    Number,
    Operator,
    Comment,
    Argument,
}

/// A highlighted byte range in a Bash input string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightSpan {
    pub start_byte: i64,
    pub end_byte: i64,
    pub kind: HighlightKind,
}

/// Tokenizes and classifies `input` into ordered, non-overlapping [`HighlightSpan`]s.
///
/// Works on both complete scripts and partially typed command lines.
pub fn highlight_spans(input: &str) -> Vec<HighlightSpan> {
    if input.is_empty() {
        return Vec::new();
    }
    match tokenize_str(input) {
        Ok(tokens) => highlight_from_tokens(input, &tokens),
        Err(_) => highlight_incomplete_fallback(input),
    }
}

fn highlight_from_tokens(input: &str, tokens: &[Token]) -> Vec<HighlightSpan> {
    let options = ParserOptions::default();
    let hints = parse_tokens(tokens, &options)
        .map(|program| collect_ast_hints(&program))
        .unwrap_or_default();

    let mut spans = Vec::with_capacity(tokens.len());
    let mut cursor = 0usize;
    let mut state = TokenHighlightState::default();

    for (idx, token) in tokens.iter().enumerate() {
        let loc = token.location();
        let start = loc.start.byte_offset.min(input.len());
        let end = loc.end.byte_offset.min(input.len());

        if start > cursor {
            emit_comments_in_gap(input, cursor, start, &mut spans);
        }

        match token {
            Token::Operator(op, _) => {
                if op != "\n" {
                    push_span(&mut spans, start, end, HighlightKind::Operator);
                }
                state.on_operator(op);
            }
            Token::Word(word, _) if start < end => {
                let next_is_parens = matches!(
                    (tokens.get(idx + 1), tokens.get(idx + 2)),
                    (Some(Token::Operator(a, _)), Some(Token::Operator(b, _)))
                        if a == "(" && b == ")"
                );
                classify_word_token(
                    input,
                    word,
                    start,
                    end,
                    next_is_parens,
                    &hints,
                    &options,
                    &mut state,
                    &mut spans,
                );
            }
            Token::Word(..) => {}
        }

        cursor = cursor.max(end);
    }

    if cursor < input.len() {
        emit_comments_in_gap(input, cursor, input.len(), &mut spans);
    }

    spans
}

struct TokenHighlightState {
    expect_command: bool,
    expect_func_name: bool,
    expect_subcommand: bool,
    expect_redirect_target: bool,
}

impl Default for TokenHighlightState {
    fn default() -> Self {
        Self {
            expect_command: true,
            expect_func_name: false,
            expect_subcommand: false,
            expect_redirect_target: false,
        }
    }
}

impl TokenHighlightState {
    fn on_operator(&mut self, op: &str) {
        if is_redirect_operator(op) {
            self.expect_redirect_target = true;
            return;
        }
        self.expect_redirect_target = false;
        if matches!(
            op,
            ";" | ";;" | ";&" | ";;&" | "&" | "&&" | "|" | "|&" | "||" | "(" | "\n"
        ) {
            self.expect_command = true;
            self.expect_func_name = false;
            self.expect_subcommand = false;
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "single-pass classifier carries token span, AST hints, and state"
)]
fn classify_word_token(
    input: &str,
    word: &str,
    start: usize,
    end: usize,
    next_is_parens: bool,
    hints: &AstHints,
    options: &ParserOptions,
    state: &mut TokenHighlightState,
    spans: &mut Vec<HighlightSpan>,
) {
    if state.expect_redirect_target {
        state.expect_redirect_target = false;
        emit_word_pieces(
            input,
            word,
            start,
            end,
            HighlightKind::Argument,
            options,
            spans,
        );
        return;
    }

    if state.expect_func_name
        || hints.func_def_spans.contains(&(start, end))
        || (state.expect_command && next_is_parens)
    {
        state.expect_func_name = false;
        state.expect_command = false;
        state.expect_subcommand = false;
        push_span(spans, start, end, HighlightKind::FunctionDef);
        return;
    }

    if (state.expect_command && is_shell_keyword(word))
        || matches!(
            word,
            "then" | "do" | "fi" | "done" | "esac" | "else" | "elif" | "]]" | "}"
        )
    {
        if word == "function" {
            state.expect_func_name = true;
            state.expect_command = false;
        } else {
            state.expect_command = matches!(
                word,
                "if" | "elif" | "else" | "then" | "do" | "while" | "until" | "time" | "!" | "{"
            );
        }
        state.expect_subcommand = false;
        push_span(spans, start, end, HighlightKind::Keyword);
        return;
    }

    if state.expect_command && is_assignment_word(word) {
        emit_assignment_spans(word, start, end, spans);
        return;
    }

    if state.expect_command || hints.command_spans.contains(&(start, end)) {
        state.expect_command = false;
        state.expect_subcommand = takes_subcommand(word);
        let kind = if word.starts_with('$') {
            HighlightKind::Variable
        } else {
            HighlightKind::Command
        };
        push_span(spans, start, end, kind);
        return;
    }

    if word.starts_with('-') {
        push_span(spans, start, end, HighlightKind::Flag);
        return;
    }

    if state.expect_subcommand && !word.starts_with('$') && !word.contains('/') && !is_quoted(word)
    {
        state.expect_subcommand = false;
        push_span(spans, start, end, HighlightKind::Subcommand);
        return;
    }
    state.expect_subcommand = false;

    if is_number(word) {
        push_span(spans, start, end, HighlightKind::Number);
    } else if is_quoted(word) {
        emit_word_pieces(
            input,
            word,
            start,
            end,
            HighlightKind::String,
            options,
            spans,
        );
    } else if word.starts_with('$') {
        push_span(spans, start, end, HighlightKind::Variable);
    } else {
        emit_word_pieces(
            input,
            word,
            start,
            end,
            HighlightKind::Argument,
            options,
            spans,
        );
    }
}

fn emit_assignment_spans(word: &str, start: usize, end: usize, spans: &mut Vec<HighlightSpan>) {
    if let Some(eq_idx) = word.find('=')
        && start + eq_idx <= end
        && end - start == word.len()
    {
        push_span(spans, start, start + eq_idx, HighlightKind::Variable);
        push_span(
            spans,
            start + eq_idx,
            start + eq_idx + 1,
            HighlightKind::Operator,
        );
        let val = &word[eq_idx + 1..];
        let val_kind = if is_quoted(val) {
            HighlightKind::String
        } else if is_number(val) {
            HighlightKind::Number
        } else {
            HighlightKind::Argument
        };
        push_span(spans, start + eq_idx + 1, end, val_kind);
    } else {
        push_span(spans, start, end, HighlightKind::Variable);
    }
}

fn emit_word_pieces(
    input: &str,
    word: &str,
    start: usize,
    end: usize,
    default_kind: HighlightKind,
    options: &ParserOptions,
    spans: &mut Vec<HighlightSpan>,
) {
    if end.saturating_sub(start) == word.len()
        && word.contains('$')
        && let Ok(pieces) = brush_parser::word::parse(word, options)
        && !pieces.is_empty()
    {
        let mut cursor = start;
        for piece in pieces {
            let p_start = start + piece.start_index;
            let p_end = start + piece.end_index;
            if p_start > cursor {
                push_span(spans, cursor, p_start, default_kind);
            }
            match piece.piece {
                WordPiece::ParameterExpansion(_)
                | WordPiece::CommandSubstitution(_)
                | WordPiece::BackquotedCommandSubstitution(_)
                | WordPiece::ArithmeticExpression(_) => {
                    push_span(spans, p_start, p_end, HighlightKind::Variable);
                }
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    emit_nested_pieces(start, p_start, p_end, &inner, spans);
                }
                WordPiece::Text(_)
                | WordPiece::SingleQuotedText(_)
                | WordPiece::AnsiCQuotedText(_)
                | WordPiece::TildeExpansion(_)
                | WordPiece::EscapeSequence(_) => push_span(spans, p_start, p_end, default_kind),
            }
            cursor = p_end;
        }
        if cursor < end && input.is_char_boundary(cursor) {
            push_span(spans, cursor, end, default_kind);
        }
        return;
    }
    push_span(spans, start, end, default_kind);
}

fn emit_nested_pieces(
    word_start: usize,
    seq_start: usize,
    seq_end: usize,
    inner: &[brush_parser::word::WordPieceWithSource],
    spans: &mut Vec<HighlightSpan>,
) {
    let mut cursor = seq_start;
    for item in inner {
        let s = word_start + item.start_index;
        let e = word_start + item.end_index;
        if s > cursor {
            push_span(spans, cursor, s, HighlightKind::String);
        }
        let kind = match item.piece {
            WordPiece::ParameterExpansion(_)
            | WordPiece::CommandSubstitution(_)
            | WordPiece::BackquotedCommandSubstitution(_)
            | WordPiece::ArithmeticExpression(_) => HighlightKind::Variable,
            _ => HighlightKind::String,
        };
        push_span(spans, s, e, kind);
        cursor = e;
    }
    if cursor < seq_end {
        push_span(spans, cursor, seq_end, HighlightKind::String);
    }
}

pub(super) fn push_span(
    spans: &mut Vec<HighlightSpan>,
    start: usize,
    end: usize,
    kind: HighlightKind,
) {
    if start < end {
        spans.push(HighlightSpan {
            start_byte: i64::try_from(start).unwrap_or(0),
            end_byte: i64::try_from(end).unwrap_or(0),
            kind,
        });
    }
}

fn emit_comments_in_gap(input: &str, from: usize, to: usize, spans: &mut Vec<HighlightSpan>) {
    let Some(gap) = input.get(from..to) else {
        return;
    };
    let mut offset = 0usize;
    for line in gap.split_inclusive('\n') {
        let line_no_nl = line.strip_suffix('\n').unwrap_or(line);
        let bytes = line_no_nl.as_bytes();
        if let Some(hash_pos) = bytes
            .iter()
            .position(|&b| b == b'#')
            .filter(|&i| i == 0 || bytes[i - 1].is_ascii_whitespace())
        {
            push_span(
                spans,
                from + offset + hash_pos,
                from + offset + line_no_nl.len(),
                HighlightKind::Comment,
            );
        }
        offset += line.len();
    }
}
