//! The presentation layer: what a session says, as the client wants to draw it.
//!
//! * [`types`]: the events and snapshots the TUI consumes (`ClientEvent`, `ToolSnapshot`, ...);
//! * [`normalizer`]: ACP session updates turned into those events;
//! * `delegation`, `native_tool`, `native_forge`, `native_history`: Nori's own tools, read as
//!   what they did;
//! * [`session_runtime`]: the session phase as the TUI sees it.

mod delegation;
mod native_forge;
mod native_history;
mod native_tool;
mod normalizer;
pub mod session_runtime;
mod types;

pub use delegation::DelegationCall;
pub(crate) use delegation::outcome as delegation_outcome;
pub use native_tool::NativeCall;
pub(crate) use native_tool::Tone as NativeTone;
pub(crate) use native_tool::view as native_view;
pub use normalizer::ClientEventNormalizer;
pub(crate) use normalizer::NoriTurnStatus;
pub(crate) use normalizer::nori_turn_status;
pub use types::*;
