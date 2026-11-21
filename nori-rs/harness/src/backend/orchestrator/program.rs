//! Actor operations exposed to conversation programs.
use std::sync::Arc;

use super::Orchestrator;

/// Something an actor's own recovery engine did during a program's turn, kept for the run's
/// journal.
#[derive(Debug, Clone)]
pub(crate) enum RecoveryNote {
    Retry(nori_protocol::RetryScheduled),
    Route(nori_protocol::RouteChanged),
}

/// The recovery overrides programs gave their actors for the turn in flight, and what the
/// actors' engines did, by actor (its id as the user's view shows it). Shared by the tree.
#[derive(Debug, Default)]
pub(crate) struct ProgramState {
    overrides: std::sync::Mutex<std::collections::HashMap<String, nori_workflow::RecoveryOverride>>,
    notes: std::sync::Mutex<std::collections::HashMap<String, Vec<RecoveryNote>>>,
}

impl ProgramState {
    pub(crate) fn set_override(&self, actor: &str, over: nori_workflow::RecoveryOverride) {
        self.overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(actor.to_owned(), over);
    }

    pub(super) fn recovery_override(&self, actor: &str) -> Option<nori_workflow::RecoveryOverride> {
        self.overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(actor)
            .cloned()
    }

    pub(super) fn note(&self, actor: &str, note: RecoveryNote) {
        self.notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(actor.to_owned())
            .or_default()
            .push(note);
    }

    pub(crate) fn drain(&self, actor: &str) -> Vec<RecoveryNote> {
        self.notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(actor)
            .unwrap_or_default()
    }
}

/// What a program restricts an actor to. Enforced where the actor's Nori tools are served:
/// `tools` selects which host tools exist for it, and `sandbox` is applied to its `bash` by
/// the AST sandbox. Children the actor delegates to inherit the sandbox.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActorScope {
    /// The Nori host tools the actor is given; `None` leaves the session's tool policy alone.
    #[serde(default)]
    pub(crate) tools: Option<Vec<String>>,
    /// What the actor's skills tell it about its tools, appended to the tool instructions.
    #[serde(default)]
    pub(crate) instructions: Vec<String>,
    /// The tools and paths its `bash` and host tools may use.
    #[serde(default)]
    pub(crate) sandbox: Option<nori_conduit::SandboxPolicy>,
    /// A Nori home of its own: configuration is seeded from the parent's, credentials are not.
    #[serde(default)]
    pub(crate) home: Option<std::path::PathBuf>,
}

impl ActorScope {
    /// The Nori tools the actor may see: its own selection narrowed by what the sandbox allows.
    pub(crate) fn tool_names(&self) -> Option<std::collections::BTreeSet<String>> {
        let allowed = self
            .sandbox
            .as_ref()
            .filter(|sandbox| !sandbox.allowed_tools.is_empty())
            .map(|sandbox| {
                sandbox
                    .allowed_tools
                    .iter()
                    .map(|name| {
                        name.split_once(':')
                            .map_or(name.as_str(), |(tool, _)| tool)
                            .to_owned()
                    })
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .filter(|names| !names.contains("*"));
        let chosen = self.tools.as_ref().map(|tools| {
            tools
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
        });
        match (chosen, allowed) {
            (Some(chosen), Some(allowed)) => Some(chosen.intersection(&allowed).cloned().collect()),
            (Some(names), None) | (None, Some(names)) => Some(names),
            (None, None) => None,
        }
    }
}

/// What a program asks of a new actor beyond its agent and model.
pub(crate) struct ProgramActor {
    pub(crate) alias: String,
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    /// `implementation`, `research`, `review`, `design`, `test` or `general`.
    pub(crate) role: Option<String>,
    /// Other settings of the agent, by setting id.
    pub(crate) options: Vec<(String, String)>,
    /// Work in its own git worktree at the current commit.
    pub(crate) worktree: bool,
    pub(crate) scope: ActorScope,
}

impl Orchestrator {
    pub(crate) fn install_program_backend(
        &self,
        backend: std::sync::Weak<crate::backend::AcpBackend>,
    ) {
        if self.inner.caller_view.is_none()
            && let Some(live) = backend.upgrade()
        {
            // Provider children the backend observes join the tree's directory.
            live.native_children
                .attach_directory(Arc::clone(&self.inner.seed.tree), backend.clone());
            if !self.is_child() {
                self.inner
                    .seed
                    .tree
                    .directory
                    .with(|registry| registry.root_backend = Some(backend.clone()));
            }
        }
        let _ = self.inner.program_backend.set(backend);
    }

    pub(crate) async fn execute_conversation_program(
        &self,
        source: String,
    ) -> anyhow::Result<crate::runtime::ConversationProgramResult> {
        let backend = self
            .inner
            .program_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("conversation runtime is no longer available"))?;
        crate::runtime::program::execute(&source, &backend, &backend.transcript_recorder_cell())
            .await
    }

    /// Run the session's skill `name` with `arguments`.
    pub(crate) async fn execute_skill(
        &self,
        name: &str,
        arguments: std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<crate::runtime::ConversationProgramResult> {
        let backend = self
            .inner
            .program_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("conversation runtime is no longer available"))?;
        let modules = self.modules().await;
        let skill = modules.skill(name).ok_or_else(|| {
            let known: Vec<&str> = modules
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect();
            anyhow::anyhow!(
                "there is no skill named {name}; the skills are: {}",
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            )
        })?;
        let source = tokio::fs::read_to_string(&skill.path).await?;
        crate::runtime::program::execute_source(
            &source,
            Some(skill.path.clone()),
            &arguments,
            &backend,
            &backend.transcript_recorder_cell(),
        )
        .await
    }
}

impl crate::backend::AcpBackend {
    /// Hold while dispatching one program effect so a rewind cannot race admission.
    pub(crate) async fn program_effect_admission(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.program_effects.read().await
    }

    /// Generation of the conversation's currently bound ACP session.
    pub(crate) fn program_conversation_generation(&self) -> i64 {
        self.orchestrator.admission_generation()
    }

    /// Give the actor's recovery engine what the program says for the turn about to be sent.
    pub(crate) fn program_set_recovery(&self, actor: &str, over: nori_workflow::RecoveryOverride) {
        self.orchestrator
            .seed()
            .tree
            .program
            .set_override(actor, over);
    }

    /// What the actor's recovery engine did since this was last called.
    pub(crate) fn program_recovery_notes(&self, actor: &str) -> Vec<super::RecoveryNote> {
        self.orchestrator.seed().tree.program.drain(actor)
    }

    /// Start an idle actor as a program declared it.
    pub(crate) async fn program_actor_spawn(
        &self,
        actor: ProgramActor,
    ) -> anyhow::Result<super::ActorView> {
        self.orchestrator
            .spawn_configured_actor(actor)
            .await
            .map_err(anyhow::Error::msg)
    }
}
