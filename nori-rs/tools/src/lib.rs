//! Nori's shared agent-facing file tools: `read`, `write`, `edit`, `multi_edit`, `apply_patch`.
//!
//! One implementation for every agent path. A host registers [`catalog`] with its model API and
//! forwards each call to [`Workspace::call`]:
//!
//! ```ignore
//! let workspace = Workspace::local("/path/to/project");
//! let specs = nori_tools::catalog(EditStyle::Replace);
//! let result = workspace.call("edit", serde_json::json!({
//!     "file_path": "src/lib.rs", "old_string": "a", "new_string": "b",
//! })).await;
//! // result.text and result.is_error go to the model; result.changes feed diffs and undo.
//! ```
//!
//! The pieces:
//! - [`spec`]: the tool catalog as data (names, aliases, descriptions, JSON schemas).
//! - [`ledger`]: the per-session read ledger behind the read-before-edit contract.
//! - [`edit`]: the pure, tiered edit engine (exact, line endings, Unicode, trailing whitespace,
//!   indentation with re-indent), with actionable misses and atomic `multi_edit`.
//! - [`fs`]: the [`ToolFs`] seam; [`LocalFs`] is this machine, a remote machine implements the
//!   trait.
//!
//! The crate depends on no harness, ACP or provider crate.

pub mod edit;
mod edit_tool;
pub mod error;
pub mod fs;
mod io;
pub mod ledger;
pub mod patch;
mod plan;
mod preview;
mod read;
mod repair;
pub mod result;
pub mod spec;
mod suggest;
mod util;
mod workspace;
mod write_tool;

#[cfg(test)]
mod tool_tests;

#[cfg(test)]
mod cancel_preview_tests;

pub use error::ToolError;
pub use fs::DirEntry;
pub use fs::FileKind;
pub use fs::FileStat;
pub use fs::LocalFs;
pub use fs::ToolFs;
pub use ledger::Freshness;
pub use ledger::LedgerSnapshot;
pub use ledger::LineRange;
pub use ledger::ReadLedger;
pub use ledger::ReadRecord;
pub use preview::PermissionAction;
pub use preview::Preview;
pub use preview::PreviewChange;
pub use preview::PreviewPath;
pub use result::ChangeKind;
pub use result::EditSummary;
pub use result::FileChange;
pub use result::ImageAttachment;
pub use result::ToolResult;
pub use spec::EditStyle;
pub use spec::ToolFormat;
pub use spec::ToolKind;
pub use spec::ToolSpec;
pub use spec::catalog;
pub use workspace::Workspace;
pub use workspace::WorkspaceConfig;
