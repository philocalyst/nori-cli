//! Native Rust Pkl runtime for Nori: policy files, programs and journals.
//!
//! - [`native`] evaluates Pkl modules to data and parses them losslessly for structural edits.
//!   Nothing here launches a subprocess; unsupported constructs fail with the file and line.
//! - [`policy`] loads the workflow policy (`default.pkl`, `$NORI_HOME/init.pkl`,
//!   `<repo>/.nori/nori.pkl`), deep-merged in that order.
//! - [`recovery`] reads the shared `recovery` block shape used by policy files and programs.
//! - [`machine`] captures the machine properties policies and programs may read.
//! - [`schemas`] embeds the `nori:Job`, `nori:Monitor` and `nori:Plugin` schemas as virtual modules.
//! - [`kinds`] holds one typed Rust binding per declarative kind (job, monitor, skill, plugin).
//! - [`catalog`] finds, evaluates, validates and watches the modules of those kinds under
//!   `$NORI_HOME` and `<repo>/.nori`.
//!
//! Evaluation produces data only. Programs themselves (`nori:Program`) are compiled and run by
//! `nori-conduit`; replaying a journal inspects recorded data and never evaluates it.
pub mod catalog;
pub mod kinds;
pub mod machine;
pub mod native;
mod policy;
pub mod recovery;
pub mod schemas;
pub use native::EvaluatorOptions;
pub use policy::global_policy_path;
pub use policy::load_policy;
pub use policy::load_policy_with_machine;
pub use policy::project_policy_path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Pkl evaluation failed: {0}")]
    Evaluation(String),
    #[error("invalid policy: {0}")]
    Invalid(String),
}
