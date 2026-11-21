//! Conversation programs on the harness: load a `nori:Program` module, run it through
//! `nori-conduit` against this session's actors, and keep the run's journal.
//!
//! The conduit runtime owns execution, recovery and journaling; this module is the adapter.
//! [`host::HarnessProgramHost`] is the one host trait implementation: it starts and addresses
//! actors through the backend's actor controls (the tree-wide actor directory) and performs
//! the session-level effects. Each run keeps `.nori/runs/<run-id>/journal.jsonl` (versioned
//! envelopes) and `state.pkl` (a re-loadable snapshot) in the working directory; the
//! conversation journal records the program's source, its run id and its outcome, so a rewound
//! or resumed conversation still knows what ran. Reopening a journal never runs a program.

mod effects;
mod home;
mod host;
mod usage;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use nori_conduit::FileTransport;
use nori_conduit::RunConfig;
use nori_conduit::UnavailableToolRuntime;
use nori_conduit::load_source_with;
use nori_conduit::run::new_run_id;
use nori_conduit::run_program;
use tokio::sync::RwLock;

use crate::backend::AcpBackend;
use crate::transcript::TranscriptRecorder;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ConversationProgramResult {
    /// The run id: the directory name under `.nori/runs/`.
    pub program_id: String,
    /// What the program returned.
    pub value: serde_json::Value,
    /// The latest value of every binding, as text.
    pub bindings: std::collections::BTreeMap<String, String>,
    /// The run's journal of envelopes.
    pub journal: String,
    /// The run's re-loadable Pkl snapshot.
    pub snapshot: String,
}

pub(crate) async fn execute(
    source: &str,
    backend: &Arc<AcpBackend>,
    recorder_cell: &Arc<RwLock<Option<Arc<TranscriptRecorder>>>>,
) -> anyhow::Result<ConversationProgramResult> {
    execute_source(source, None, &BTreeMap::new(), backend, recorder_cell).await
}

/// Run a program, or a skill with its `arguments` (see `nori_conduit::load_source_with`).
pub(crate) async fn execute_source(
    source: &str,
    path: Option<PathBuf>,
    arguments: &BTreeMap<String, String>,
    backend: &Arc<AcpBackend>,
    recorder_cell: &Arc<RwLock<Option<Arc<TranscriptRecorder>>>>,
) -> anyhow::Result<ConversationProgramResult> {
    let recorder = recorder_cell
        .read()
        .await
        .clone()
        .ok_or_else(|| anyhow!("conversation programs require an event log"))?;
    let run = new_run_id();
    let generation = backend.program_conversation_generation();
    recorder
        .record_command(
            "conversation-program",
            serde_json::json!({
                "run": run, "source": source, "path": path, "arguments": arguments,
                "generation": generation,
            })
            .to_string(),
        )
        .await?;
    let run_dir = backend.program_cwd().join(".nori").join("runs").join(&run);
    let outcome: anyhow::Result<ConversationProgramResult> = async {
        let properties = backend.program_machine_properties().await?;
        let compiled = {
            let source = source.to_owned();
            let path = path.clone();
            let arguments = arguments.clone();
            tokio::task::spawn_blocking(move || {
                load_source_with(&source, path.as_deref(), properties, &arguments)
            })
            .await??
        };
        let host = Arc::new(host::HarnessProgramHost::new(
            Arc::clone(backend),
            Arc::clone(&recorder),
            backend.program_recovery().await,
            generation,
        ));
        let mut config = RunConfig::new(host, Arc::new(FileTransport::open(&run_dir)?));
        config.run_id = Some(run.clone());
        config.run_dir = Some(run_dir.clone());
        config.source = path.clone();
        config.tools = Arc::new(UnavailableToolRuntime);
        let outcome = run_program(compiled, config).await?;
        Ok(ConversationProgramResult {
            program_id: outcome.run,
            value: outcome.value.to_plain_json(),
            bindings: outcome.bindings,
            journal: run_dir.join(FileTransport::JOURNAL).display().to_string(),
            snapshot: run_dir.join("state.pkl").display().to_string(),
        })
    }
    .await;
    let receipt = match &outcome {
        Ok(result) => serde_json::json!({
            "run": run, "success": true, "journal": result.journal,
        }),
        Err(error) => serde_json::json!({
            "run": run, "success": false, "error": error.to_string(),
            "journal": run_dir.join(FileTransport::JOURNAL),
        }),
    };
    recorder
        .record_command("conversation-program-result", receipt.to_string())
        .await?;
    outcome
}
