//! The Pkl plugin runtime: a `nori:Plugin` module declares commands, each a typed list of
//! operations on Nori, and this runs them (`docs/pkl-runtime.md#plugins`).
//!
//! * [`executor`] settles a command's arguments, fills them into its operations, and runs the
//!   operations in order until one fails; the error names the step.
//! * [`host`] is what the executor needs from a session, one method per operation, and
//!   [`session_host`] is the session's answer: every operation calls something the session already
//!   does (its jobs, monitors, skills, prompt queue, configuration, the portable bundle).
//! * [`remote`] puts the API on the remote-control channel (`_nori/plugins`, `_nori/plugin.run`).
//!
//! Entry points are `HarnessHandle::plugins` and `HarnessHandle::run_plugin`. Nothing here runs
//! on the UI path: a run is a task of its own, and the one operation that needs the frontend
//! (`session.move`, which owns the terminal's handover) is a request the frontend answers.

#[cfg(test)]
mod action_tests;
mod actions;
mod arguments;
mod executor;
mod host;
mod invocation;
pub mod remote;
mod session_host;
#[cfg(test)]
mod tests;
mod types;

pub(crate) use actions::PluginActionsFeed;
pub(crate) use invocation::resolve;
pub use types::CommandInfo;
pub use types::JobDescription;
pub use types::MonitorDescription;
pub use types::ParameterInfo;
pub use types::PermissionDescription;
pub use types::PluginError;
pub use types::PluginInfo;
pub use types::PluginRun;
pub use types::SessionDescription;
pub use types::SessionSetting;
pub use types::StepResult;
