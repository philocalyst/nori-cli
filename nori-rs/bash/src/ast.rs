//! Pure-Rust Bash AST safety analyzer and syntax highlighter powered by `brush-parser`.

mod collector;
mod highlight;
mod highlight_fallback;
mod highlight_hints;
mod words;

use brush_parser::ParseError;
use brush_parser::Parser;
use brush_parser::ParserOptions;
use serde::Deserialize;
use serde::Serialize;

pub use highlight::HighlightKind;
pub use highlight::HighlightSpan;
pub use highlight::highlight_spans;

/// A command invocation extracted from a parsed Bash AST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokedCommand {
    pub name: String,
    pub args: Vec<String>,
    pub start_byte: i64,
    pub end_byte: i64,
    pub in_subshell: bool,
}

/// Kind of nested shell execution context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubshellKind {
    Subshell,
    CommandSubstitution,
    ProcessSubstitution,
}

/// A subshell, command substitution, or process substitution found in a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubshellFact {
    pub kind: SubshellKind,
    pub source: String,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
}

/// Kind of I/O redirection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RedirectionKind {
    Read,
    Write,
    Append,
    ReadAndWrite,
    Clobber,
    DuplicateInput,
    DuplicateOutput,
    OutputAndError,
    HereDocument,
    HereString,
}

/// A redirection extracted from a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectionFact {
    pub fd: Option<i64>,
    pub kind: RedirectionKind,
    pub target: String,
    pub is_write: bool,
}

/// Role in which a path literal appeared in the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PathContext {
    Command,
    Argument,
    RedirectRead,
    RedirectWrite,
}

/// A filesystem path literal referenced by a script.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PathLiteral {
    pub path: String,
    pub context: PathContext,
}

/// Deterministic safety facts extracted from a parsed Bash script.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptFacts {
    pub commands: Vec<InvokedCommand>,
    pub command_names: Vec<String>,
    pub subshells: Vec<SubshellFact>,
    pub redirections: Vec<RedirectionFact>,
    pub path_literals: Vec<PathLiteral>,
    pub function_definitions: Vec<String>,
    pub has_dynamic_commands: bool,
}

/// Parses `input` with `brush-parser` and extracts invoked commands, subshells,
/// redirections, and referenced path literals for deterministic guard checks.
pub fn inspect_script(input: &str) -> Result<ScriptFacts, ParseError> {
    let options = ParserOptions::default();
    let mut parser = Parser::new(input.as_bytes(), &options);
    let program = parser.parse_program()?;

    let mut collector = collector::FactCollector::new(&options);
    collector.visit_program(&program, false);
    Ok(collector.finish())
}
