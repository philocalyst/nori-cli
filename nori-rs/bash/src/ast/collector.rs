//! AST traversal for extracting deterministic Bash safety facts.

use std::collections::HashSet;

use brush_parser::ParserOptions;
use brush_parser::ast::AssignmentValue;
use brush_parser::ast::Command;
use brush_parser::ast::CommandPrefixOrSuffixItem;
use brush_parser::ast::CompoundCommand;
use brush_parser::ast::CompoundList;
use brush_parser::ast::ExtendedTestExpr;
use brush_parser::ast::IoFileRedirectKind;
use brush_parser::ast::IoFileRedirectTarget;
use brush_parser::ast::IoRedirect;
use brush_parser::ast::Program;
use brush_parser::ast::RedirectList;
use brush_parser::ast::SimpleCommand;
use brush_parser::ast::SourceLocation as _;
use brush_parser::unquote_str;

use super::InvokedCommand;
use super::PathContext;
use super::PathLiteral;
use super::RedirectionFact;
use super::RedirectionKind;
use super::ScriptFacts;
use super::SubshellFact;
use super::SubshellKind;
use super::words::is_path_literal;
use super::words::word_is_dynamic;

pub(super) struct FactCollector<'a> {
    pub(super) parser_options: &'a ParserOptions,
    pub(super) facts: ScriptFacts,
    seen_commands: HashSet<String>,
    seen_paths: HashSet<PathLiteral>,
}

impl<'a> FactCollector<'a> {
    pub(super) fn new(parser_options: &'a ParserOptions) -> Self {
        Self {
            parser_options,
            facts: ScriptFacts::default(),
            seen_commands: HashSet::new(),
            seen_paths: HashSet::new(),
        }
    }

    pub(super) fn finish(self) -> ScriptFacts {
        self.facts
    }

    fn record_command_name(&mut self, name: &str) {
        if self.seen_commands.insert(name.to_owned()) {
            self.facts.command_names.push(name.to_owned());
        }
    }

    pub(super) fn record_path(&mut self, path: String, context: PathContext) {
        if path.is_empty() {
            return;
        }
        let item = PathLiteral { path, context };
        if self.seen_paths.insert(item.clone()) {
            self.facts.path_literals.push(item);
        }
    }

    pub(super) fn visit_program(&mut self, program: &Program, in_subshell: bool) {
        for complete in &program.complete_commands {
            self.visit_compound_list(complete, in_subshell);
        }
    }

    fn visit_compound_list(&mut self, list: &CompoundList, in_subshell: bool) {
        for item in &list.0 {
            for (_, pipeline) in item.0.iter() {
                for command in &pipeline.seq {
                    self.visit_command(command, in_subshell);
                }
            }
        }
    }

    fn visit_command(&mut self, command: &Command, in_subshell: bool) {
        match command {
            Command::Simple(simple) => self.visit_simple_command(simple, in_subshell),
            Command::Compound(compound, redirects) => {
                if let Some(redirects) = redirects {
                    self.visit_redirect_list(redirects, in_subshell);
                }
                self.visit_compound_command(compound, in_subshell);
            }
            Command::Function(func) => {
                self.facts
                    .function_definitions
                    .push(unquote_str(&func.fname.value));
                if let Some(redirects) = &func.body.1 {
                    self.visit_redirect_list(redirects, in_subshell);
                }
                self.visit_compound_command(&func.body.0, in_subshell);
            }
        }
    }

    fn visit_compound_command(&mut self, compound: &CompoundCommand, in_subshell: bool) {
        match compound {
            CompoundCommand::Arithmetic(_) => {}
            CompoundCommand::ArithmeticForClause(for_clause) => {
                self.visit_compound_list(&for_clause.body.list, in_subshell);
            }
            CompoundCommand::BraceGroup(group) => {
                self.visit_compound_list(&group.list, in_subshell);
            }
            CompoundCommand::Subshell(sub) => {
                self.facts.subshells.push(SubshellFact {
                    kind: SubshellKind::Subshell,
                    source: sub.to_string(),
                    start_byte: Some(i64::try_from(sub.loc.start.byte_offset).unwrap_or(0)),
                    end_byte: Some(i64::try_from(sub.loc.end.byte_offset).unwrap_or(0)),
                });
                self.visit_compound_list(&sub.list, true);
            }
            CompoundCommand::ForClause(for_clause) => {
                if let Some(values) = &for_clause.values {
                    for word in values {
                        self.inspect_word(word, true);
                    }
                }
                self.visit_compound_list(&for_clause.body.list, in_subshell);
            }
            CompoundCommand::CaseClause(case_clause) => {
                self.inspect_word(&case_clause.value, false);
                for case_item in &case_clause.cases {
                    for pattern in &case_item.patterns {
                        self.inspect_word(pattern, false);
                    }
                    if let Some(cmd) = &case_item.cmd {
                        self.visit_compound_list(cmd, in_subshell);
                    }
                }
            }
            CompoundCommand::IfClause(if_clause) => {
                self.visit_compound_list(&if_clause.condition, in_subshell);
                self.visit_compound_list(&if_clause.then, in_subshell);
                if let Some(elses) = &if_clause.elses {
                    for else_clause in elses {
                        if let Some(cond) = &else_clause.condition {
                            self.visit_compound_list(cond, in_subshell);
                        }
                        self.visit_compound_list(&else_clause.body, in_subshell);
                    }
                }
            }
            CompoundCommand::WhileClause(clause) | CompoundCommand::UntilClause(clause) => {
                self.visit_compound_list(&clause.0, in_subshell);
                self.visit_compound_list(&clause.1.list, in_subshell);
            }
            CompoundCommand::Coprocess(coproc) => {
                self.visit_command(&coproc.body, true);
            }
            CompoundCommand::ExtendedTest(test_cmd) => {
                self.visit_extended_test(&test_cmd.expr);
            }
        }
    }

    fn visit_extended_test(&mut self, expr: &ExtendedTestExpr) {
        match expr {
            ExtendedTestExpr::And(l, r) | ExtendedTestExpr::Or(l, r) => {
                self.visit_extended_test(l);
                self.visit_extended_test(r);
            }
            ExtendedTestExpr::Not(inner) | ExtendedTestExpr::Parenthesized(inner) => {
                self.visit_extended_test(inner);
            }
            ExtendedTestExpr::UnaryTest(_, word) => {
                self.inspect_word(word, true);
            }
            ExtendedTestExpr::BinaryTest(_, left, right) => {
                self.inspect_word(left, true);
                self.inspect_word(right, true);
            }
        }
    }

    fn visit_simple_command(&mut self, simple: &SimpleCommand, in_subshell: bool) {
        if let Some(prefix) = &simple.prefix {
            for item in &prefix.0 {
                self.visit_prefix_or_suffix_item(item, false);
            }
        }

        let is_cd = simple
            .word_or_name
            .as_ref()
            .is_some_and(|w| unquote_str(&w.value) == "cd");
        let mut args = Vec::new();
        if let Some(suffix) = &simple.suffix {
            for item in &suffix.0 {
                if let CommandPrefixOrSuffixItem::Word(word) = item {
                    args.push(unquote_str(&word.value));
                }
            }
        }

        if let Some(word) = &simple.word_or_name {
            let name = unquote_str(&word.value);
            if word_is_dynamic(&word.value, self.parser_options)
                || matches!(name.as_str(), "eval" | "source" | ".")
            {
                self.facts.has_dynamic_commands = true;
            }
            self.inspect_word(word, false);
            if is_path_literal(&name) {
                self.record_path(name.clone(), PathContext::Command);
            }
            let (start_byte, end_byte) = word.location().map_or((0, 0), |loc| {
                (
                    i64::try_from(loc.start.byte_offset).unwrap_or(0),
                    i64::try_from(loc.end.byte_offset).unwrap_or(0),
                )
            });
            self.record_command_name(&name);
            self.facts.commands.push(InvokedCommand {
                name,
                args,
                start_byte,
                end_byte,
                in_subshell,
            });
        }

        if let Some(suffix) = &simple.suffix {
            for item in &suffix.0 {
                self.visit_prefix_or_suffix_item(item, is_cd);
            }
        }
    }

    fn visit_prefix_or_suffix_item(
        &mut self,
        item: &CommandPrefixOrSuffixItem,
        treat_word_as_path: bool,
    ) {
        match item {
            CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                self.visit_redirect(redirect);
            }
            CommandPrefixOrSuffixItem::Word(word) => {
                self.inspect_word(word, true);
                if treat_word_as_path {
                    let arg = unquote_str(&word.value);
                    if !arg.starts_with('-') && arg != "-" {
                        self.record_path(arg, PathContext::Argument);
                    }
                }
            }
            CommandPrefixOrSuffixItem::AssignmentWord(assign, _) => match &assign.value {
                AssignmentValue::Scalar(word) => self.inspect_word(word, true),
                AssignmentValue::Array(items) => {
                    for (key, value) in items {
                        if let Some(k) = key {
                            self.inspect_word(k, false);
                        }
                        self.inspect_word(value, true);
                    }
                }
            },
            CommandPrefixOrSuffixItem::ProcessSubstitution(_, sub) => {
                self.facts.subshells.push(SubshellFact {
                    kind: SubshellKind::ProcessSubstitution,
                    source: sub.to_string(),
                    start_byte: Some(i64::try_from(sub.loc.start.byte_offset).unwrap_or(0)),
                    end_byte: Some(i64::try_from(sub.loc.end.byte_offset).unwrap_or(0)),
                });
                self.visit_compound_list(&sub.list, true);
            }
        }
    }

    fn visit_redirect_list(&mut self, list: &RedirectList, _in_subshell: bool) {
        for redirect in &list.0 {
            self.visit_redirect(redirect);
        }
    }

    fn visit_redirect(&mut self, redirect: &IoRedirect) {
        match redirect {
            IoRedirect::File(fd, kind, target) => {
                let (mapped_kind, is_write) = map_file_redirect_kind(kind);
                let fd = fd.map(i64::from);
                match target {
                    IoFileRedirectTarget::Filename(word) => {
                        self.inspect_word(word, false);
                        let target_str = unquote_str(&word.value);
                        let ctx = if is_write {
                            PathContext::RedirectWrite
                        } else {
                            PathContext::RedirectRead
                        };
                        self.record_path(target_str.clone(), ctx);
                        self.facts.redirections.push(RedirectionFact {
                            fd,
                            kind: mapped_kind,
                            target: target_str,
                            is_write,
                        });
                    }
                    IoFileRedirectTarget::Fd(target_fd) => {
                        self.facts.redirections.push(RedirectionFact {
                            fd,
                            kind: mapped_kind,
                            target: format!("&{target_fd}"),
                            is_write,
                        });
                    }
                    IoFileRedirectTarget::Duplicate(word) => {
                        self.inspect_word(word, false);
                        self.facts.redirections.push(RedirectionFact {
                            fd,
                            kind: mapped_kind,
                            target: unquote_str(&word.value),
                            is_write,
                        });
                    }
                    IoFileRedirectTarget::ProcessSubstitution(proc_kind, sub) => {
                        self.facts.subshells.push(SubshellFact {
                            kind: SubshellKind::ProcessSubstitution,
                            source: sub.to_string(),
                            start_byte: Some(i64::try_from(sub.loc.start.byte_offset).unwrap_or(0)),
                            end_byte: Some(i64::try_from(sub.loc.end.byte_offset).unwrap_or(0)),
                        });
                        self.facts.redirections.push(RedirectionFact {
                            fd,
                            kind: mapped_kind,
                            target: format!("{proc_kind}({sub})"),
                            is_write,
                        });
                        self.visit_compound_list(&sub.list, true);
                    }
                }
            }
            IoRedirect::OutputAndError(word, _) => {
                self.inspect_word(word, false);
                let target_str = unquote_str(&word.value);
                self.record_path(target_str.clone(), PathContext::RedirectWrite);
                self.facts.redirections.push(RedirectionFact {
                    fd: None,
                    kind: RedirectionKind::OutputAndError,
                    target: target_str,
                    is_write: true,
                });
            }
            IoRedirect::HereDocument(fd, doc) => {
                if doc.requires_expansion {
                    self.inspect_word(&doc.doc, false);
                }
                self.facts.redirections.push(RedirectionFact {
                    fd: fd.map(i64::from),
                    kind: RedirectionKind::HereDocument,
                    target: unquote_str(&doc.here_end.value),
                    is_write: false,
                });
            }
            IoRedirect::HereString(fd, word) => {
                self.inspect_word(word, false);
                self.facts.redirections.push(RedirectionFact {
                    fd: fd.map(i64::from),
                    kind: RedirectionKind::HereString,
                    target: unquote_str(&word.value),
                    is_write: false,
                });
            }
        }
    }
}

const fn map_file_redirect_kind(kind: &IoFileRedirectKind) -> (RedirectionKind, bool) {
    match kind {
        IoFileRedirectKind::Read => (RedirectionKind::Read, false),
        IoFileRedirectKind::Write => (RedirectionKind::Write, true),
        IoFileRedirectKind::Append => (RedirectionKind::Append, true),
        IoFileRedirectKind::ReadAndWrite => (RedirectionKind::ReadAndWrite, true),
        IoFileRedirectKind::Clobber => (RedirectionKind::Clobber, true),
        IoFileRedirectKind::DuplicateInput => (RedirectionKind::DuplicateInput, false),
        IoFileRedirectKind::DuplicateOutput => (RedirectionKind::DuplicateOutput, true),
    }
}
