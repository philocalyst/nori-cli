//! `apply_patch`: the freeform multi-file patch format (`*** Begin Patch` ... `*** End Patch`).
//!
//! The grammar is Codex's (see the crate NOTICE). Parsing and application are written for this
//! crate: hunks are located with the same tiers as `edit` (exact, trailing whitespace,
//! indentation with re-indent, Unicode), the whole patch is validated in memory first, and files
//! are only written when every hunk applied.

mod apply;
mod chunks;
mod parse;

use thiserror::Error;

use crate::edit::MissHint;

pub(crate) use apply::changed_entries;
pub(crate) use apply::plan as plan_patch;
pub(crate) use apply::run;
pub(crate) use apply::touched_paths;
pub use parse::Chunk;
pub use parse::Hunk;
pub use parse::parse_patch;

#[derive(Debug, Error)]
pub enum PatchError {
    #[error(
        "The patch must start with `*** Begin Patch` and end with `*** End Patch`. Send only the \
         patch text, not a JSON object or a shell command."
    )]
    MissingEnvelope,

    #[error(
        "Patch line {line_number}: `{line}` is not a valid file header. Use `*** Add File: path`, \
         `*** Delete File: path` or `*** Update File: path` (optionally followed by `*** Move to: \
         path`). Paths must be relative."
    )]
    BadHeader { line_number: i64, line: String },

    #[error(
        "Patch line {line_number} (in `{path}`): `{line}` must start with ` ` (context), `-` \
         (remove) or `+` (add). Every added line of `*** Add File` must start with `+`."
    )]
    BadLine {
        line_number: i64,
        path: String,
        line: String,
    },

    #[error(
        "`*** Update File: {path}` has no changes. Give at least one hunk, or drop the header."
    )]
    EmptyUpdate { path: String },

    #[error("The patch contains no file operations.")]
    Empty,

    #[error("`*** Delete File: {path}` names a file that does not exist. Nothing was changed.")]
    DeleteMissing { path: String },

    #[error(
        "In `{path}`, the context line `{context}` (after `@@`) was not found, so the patch was \
         not applied. `read` the file and use a line that exists as the `@@` context."
    )]
    ContextNotFound { path: String, context: String },

    #[error(
        "In `{path}`, hunk {hunk} does not match the file, so no file was changed.\n{hint}\n\
         `read` the file and regenerate the patch with exact context lines."
    )]
    HunkNotFound {
        path: String,
        hunk: i64,
        hint: MissHint,
    },

    #[error(
        "`*** Move to: {target}` already exists, so `{path}` was not moved. Nothing was changed."
    )]
    MoveTargetExists { path: String, target: String },
}
