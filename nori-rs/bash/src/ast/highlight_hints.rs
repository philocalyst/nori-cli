//! AST span hints and lexical classification predicates for Bash highlighting.

use std::collections::HashSet;

use brush_parser::ast::Command;
use brush_parser::ast::CompoundCommand;
use brush_parser::ast::CompoundList;
use brush_parser::ast::Program;
use brush_parser::ast::SourceLocation as _;

#[derive(Default)]
pub(super) struct AstHints {
    pub(super) command_spans: HashSet<(usize, usize)>,
    pub(super) func_def_spans: HashSet<(usize, usize)>,
}

pub(super) fn collect_ast_hints(program: &Program) -> AstHints {
    let mut hints = AstHints::default();
    for complete in &program.complete_commands {
        collect_hints_list(complete, &mut hints);
    }
    hints
}

fn collect_hints_list(list: &CompoundList, hints: &mut AstHints) {
    for item in &list.0 {
        for (_, pipeline) in item.0.iter() {
            for cmd in &pipeline.seq {
                collect_hints_command(cmd, hints);
            }
        }
    }
}

fn collect_hints_command(cmd: &Command, hints: &mut AstHints) {
    match cmd {
        Command::Simple(simple) => {
            if let Some(word) = &simple.word_or_name
                && let Some(loc) = word.location()
            {
                hints
                    .command_spans
                    .insert((loc.start.byte_offset, loc.end.byte_offset));
            }
        }
        Command::Compound(compound, _) => collect_hints_compound(compound, hints),
        Command::Function(func) => {
            if let Some(loc) = func.fname.location() {
                hints
                    .func_def_spans
                    .insert((loc.start.byte_offset, loc.end.byte_offset));
            }
            collect_hints_compound(&func.body.0, hints);
        }
    }
}

fn collect_hints_compound(compound: &CompoundCommand, hints: &mut AstHints) {
    match compound {
        CompoundCommand::BraceGroup(g) => collect_hints_list(&g.list, hints),
        CompoundCommand::Subshell(s) => collect_hints_list(&s.list, hints),
        CompoundCommand::ForClause(f) => collect_hints_list(&f.body.list, hints),
        CompoundCommand::ArithmeticForClause(f) => collect_hints_list(&f.body.list, hints),
        CompoundCommand::CaseClause(c) => {
            for case_item in &c.cases {
                if let Some(list) = &case_item.cmd {
                    collect_hints_list(list, hints);
                }
            }
        }
        CompoundCommand::IfClause(i) => {
            collect_hints_list(&i.condition, hints);
            collect_hints_list(&i.then, hints);
            if let Some(elses) = &i.elses {
                for e in elses {
                    if let Some(cond) = &e.condition {
                        collect_hints_list(cond, hints);
                    }
                    collect_hints_list(&e.body, hints);
                }
            }
        }
        CompoundCommand::WhileClause(w) | CompoundCommand::UntilClause(w) => {
            collect_hints_list(&w.0, hints);
            collect_hints_list(&w.1.list, hints);
        }
        CompoundCommand::Coprocess(c) => collect_hints_command(&c.body, hints),
        CompoundCommand::Arithmetic(_) | CompoundCommand::ExtendedTest(_) => {}
    }
}

pub(super) fn is_shell_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "in"
            | "do"
            | "done"
            | "while"
            | "until"
            | "case"
            | "esac"
            | "select"
            | "function"
            | "time"
            | "coproc"
            | "{"
            | "}"
            | "!"
            | "[["
            | "]]"
    )
}

pub(super) fn is_redirect_operator(op: &str) -> bool {
    matches!(
        op,
        "<" | ">" | ">>" | ">|" | "<<" | "<<-" | "<<<" | "<&" | ">&" | "&>" | "&>>" | "<>"
    )
}

pub(super) fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(super) fn is_quoted(word: &str) -> bool {
    (word.starts_with('"') && word.ends_with('"') && word.len() >= 2)
        || (word.starts_with('\'') && word.ends_with('\'') && word.len() >= 2)
        || (word.starts_with("$'") && word.ends_with('\'') && word.len() >= 3)
        || (word.starts_with("$\"") && word.ends_with('"') && word.len() >= 3)
}

pub(super) fn is_number(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit())
}

pub(super) fn takes_subcommand(cmd: &str) -> bool {
    matches!(
        cmd,
        "git"
            | "cargo"
            | "docker"
            | "kubectl"
            | "npm"
            | "pnpm"
            | "yarn"
            | "gh"
            | "brew"
            | "systemctl"
            | "rustup"
            | "pip"
            | "uv"
            | "jj"
            | "nori"
            | "tmux"
    )
}
