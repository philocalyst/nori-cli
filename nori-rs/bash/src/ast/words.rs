//! Word-level AST inspection helpers for command substitutions and path literals.

use brush_parser::Parser;
use brush_parser::ParserOptions;
use brush_parser::ast::Word;
use brush_parser::unquote_str;
use brush_parser::word::WordPiece;

use super::PathContext;
use super::SubshellFact;
use super::SubshellKind;
use super::collector::FactCollector;

impl FactCollector<'_> {
    pub(super) fn inspect_word(&mut self, word: &Word, check_arg_path: bool) {
        if check_arg_path && let Some(path) = extract_arg_path(&unquote_str(&word.value)) {
            self.record_path(path, PathContext::Argument);
        }
        let Ok(pieces) = brush_parser::word::parse(&word.value, self.parser_options) else {
            return;
        };
        for piece in pieces {
            self.inspect_word_piece(&piece.piece);
        }
    }

    fn inspect_word_piece(&mut self, piece: &WordPiece) {
        match piece {
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
                for item in inner {
                    self.inspect_word_piece(&item.piece);
                }
            }
            WordPiece::CommandSubstitution(inner)
            | WordPiece::BackquotedCommandSubstitution(inner) => {
                let inner_src = strip_command_sub_delimiters(inner);
                self.facts.subshells.push(SubshellFact {
                    kind: SubshellKind::CommandSubstitution,
                    source: inner_src.to_owned(),
                    start_byte: None,
                    end_byte: None,
                });
                let mut parser = Parser::new(inner_src.as_bytes(), self.parser_options);
                if let Ok(program) = parser.parse_program() {
                    self.visit_program(&program, true);
                }
            }
            WordPiece::Text(_)
            | WordPiece::SingleQuotedText(_)
            | WordPiece::AnsiCQuotedText(_)
            | WordPiece::TildeExpansion(_)
            | WordPiece::ParameterExpansion(_)
            | WordPiece::EscapeSequence(_)
            | WordPiece::ArithmeticExpression(_) => {}
        }
    }
}

pub(super) fn word_is_dynamic(raw: &str, options: &ParserOptions) -> bool {
    brush_parser::word::parse(raw, options)
        .is_ok_and(|pieces| pieces.iter().any(|p| piece_is_dynamic(&p.piece)))
}

fn piece_is_dynamic(piece: &WordPiece) -> bool {
    match piece {
        WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_) => true,
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            inner.iter().any(|item| piece_is_dynamic(&item.piece))
        }
        WordPiece::Text(_)
        | WordPiece::SingleQuotedText(_)
        | WordPiece::AnsiCQuotedText(_)
        | WordPiece::TildeExpansion(_)
        | WordPiece::EscapeSequence(_)
        | WordPiece::ArithmeticExpression(_) => false,
    }
}

fn strip_command_sub_delimiters(raw: &str) -> &str {
    if let Some(inner) = raw.strip_prefix("$(").and_then(|s| s.strip_suffix(')')) {
        inner
    } else if let Some(inner) = raw.strip_prefix('`').and_then(|s| s.strip_suffix('`')) {
        inner
    } else {
        raw
    }
}

pub(super) fn is_path_literal(s: &str) -> bool {
    if s.is_empty()
        || s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("ssh://")
        || s.starts_with("git://")
    {
        return false;
    }
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with("~/")
        || matches!(s, "." | ".." | "~")
        || s.contains('/')
}

fn extract_arg_path(arg: &str) -> Option<String> {
    if arg.starts_with('-') {
        if let Some((_, rhs)) = arg.split_once('=')
            && is_path_literal(rhs)
        {
            return Some(rhs.to_owned());
        }
        return None;
    }
    is_path_literal(arg).then(|| arg.to_owned())
}
