//! What the plugin executor needs from a session: one method per operation of the plugin API.
//!
//! The executor owns the order, the arguments and the errors; the host owns what each operation
//! does to Nori, which is always something the session already knows how to do (its jobs, its
//! monitors, its skills, its queue). The session implements this in [`super::session_host`]; the
//! tests implement it with a recorder, so the executor is proven without a session.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use nori_pkl::catalog::Catalog;

use super::types::SessionDescription;

/// A bundle written to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Exported {
    pub(crate) path: PathBuf,
    pub(crate) conversation_id: String,
    pub(crate) bytes: i64,
}

/// A job as it stands after starting or stopping it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobOutcome {
    pub(crate) job: String,
    pub(crate) label: String,
    pub(crate) state: String,
}

/// What a skill returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillOutcome {
    pub(crate) journal: String,
    pub(crate) value: serde_json::Value,
}

/// The session a plugin command acts on. Every error is the words the person should read.
pub(crate) trait PluginHost: Send + Sync {
    /// The modules the session loaded.
    fn modules(&self) -> impl Future<Output = Arc<Catalog>> + Send;

    fn describe(&self) -> impl Future<Output = SessionDescription> + Send;

    /// Bundle the session into `path`, or into a file of the session's own when there is none.
    fn export(
        &self,
        path: Option<PathBuf>,
    ) -> impl Future<Output = Result<Exported, String>> + Send;

    /// Ask the frontend that owns the terminal to move the session.
    fn request_move(&self, machine: &str) -> impl Future<Output = Result<(), String>> + Send;

    fn start_job(&self, job: &str) -> impl Future<Output = Result<JobOutcome, String>> + Send;

    fn stop_job(&self, job: &str) -> impl Future<Output = Result<JobOutcome, String>> + Send;

    /// Arm a monitor module; the labels of the monitors it armed.
    fn start_monitor(
        &self,
        monitor: &str,
    ) -> impl Future<Output = Result<Vec<String>, String>> + Send;

    /// Disarm a monitor; the labels of the monitors it stopped.
    fn stop_monitor(
        &self,
        monitor: &str,
    ) -> impl Future<Output = Result<Vec<String>, String>> + Send;

    fn run_skill(
        &self,
        skill: &str,
        arguments: BTreeMap<String, String>,
    ) -> impl Future<Output = Result<SkillOutcome, String>> + Send;

    /// One transcript line and an OS notification; returns the line's headline.
    fn notify(
        &self,
        title: Option<String>,
        body: Option<String>,
    ) -> impl Future<Output = String> + Send;

    /// Queue a prompt as if the person had typed it.
    fn queue_prompt(&self, text: String) -> impl Future<Output = Result<(), String>> + Send;

    /// Change a session setting; the agent applies it at its next safe point.
    fn set_config(&self, key: &str, value: &str)
    -> impl Future<Output = Result<(), String>> + Send;
}
