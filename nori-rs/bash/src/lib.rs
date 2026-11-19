//! In-process POSIX/Bash execution, AST analysis, and programmable completion via `brush`.

pub mod ast;
pub mod completion;
pub mod session;
mod tracked_tasks;

pub use ast::HighlightKind;
pub use ast::HighlightSpan;
pub use ast::InvokedCommand;
pub use ast::PathContext;
pub use ast::PathLiteral;
pub use ast::RedirectionFact;
pub use ast::RedirectionKind;
pub use ast::ScriptFacts;
pub use ast::SubshellFact;
pub use ast::SubshellKind;
pub use ast::highlight_spans;
pub use ast::inspect_script;
pub use completion::CompletionCandidate;
pub use completion::CompletionKind;
pub use session::BashError;
pub use session::BashOutput;
pub use session::BashSession;
pub use session::CancellationToken;
pub use session::ExecOptions;
pub use session::OutputLine;
