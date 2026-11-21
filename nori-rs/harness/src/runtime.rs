//! Session runtime: launches an ACP backend from a [`SessionLaunchSpec`] and
//! owns the orchestration that frontends previously had to implement
//! themselves — Nori config assembly, the connect/shutdown/timeout race, the
//! op-forwarding loop, the session-control command loop, and backend event
//! forwarding.
//!
//! Frontends build a spec from their own configuration source, call
//! [`launch_session`], and consume [`SessionEvent`]s; no terminal or UI
//! concepts appear here.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_rmcp_client::OAuthCredentialsStoreMode;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::oneshot;

use crate::backend::AcpBackend;
use crate::backend::AcpBackendConfig;
use crate::backend::BackendEvent;
use crate::backend::PromptAdmission;
use crate::backend::SessionContext;
pub use crate::backend::prepared::PreparedAgent;
pub use crate::backend::prepared::SessionCatalog;
use crate::session_event_fanout::SessionEventFanout;
use crate::transcript::Transcript;
mod child_admission;
mod child_control;
mod config_lane;
mod handoff;
pub(crate) mod program;
mod prompt_queue;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Notice;
use nori_protocol::RequestFailure;
use nori_protocol::RequestFailureKind;
use nori_protocol::SessionEndReason;
use nori_protocol::SessionEnded;
pub use nori_protocol::SessionEvent;
use nori_protocol::acp;
use nori_protocol::acp::v1::SessionConfigOption;
pub use program::ConversationProgramResult;
use prompt_queue::PromptQueue;

/// Duration before showing a warning that connection is taking too long.
const CONNECT_WARNING_SECS: u64 = 8;
/// Duration after the warning before forcibly aborting the connection attempt.
const CONNECT_ABORT_SECS: u64 = 30;

/// Two-phase timeout: warn after `CONNECT_WARNING_SECS`, abort after an
/// additional `CONNECT_ABORT_SECS`.
async fn spawn_timeout_sequence(event_tx: &SessionEventFanout) {
    tokio::time::sleep(Duration::from_secs(CONNECT_WARNING_SECS)).await;
    let _ = event_tx.send(SessionEvent::Nori(NoriEvent::Notice(Notice {
        message: format!(
            "Connection is taking longer than expected. \
             Will abort in {CONNECT_ABORT_SECS}s if still unresponsive."
        ),
    })));
    tokio::time::sleep(Duration::from_secs(CONNECT_ABORT_SECS)).await;
}

fn forward_connect_event(
    event_tx: &SessionEventFanout,
    event: SessionEvent,
    raw_acp_error_observed: &mut bool,
) {
    *raw_acp_error_observed |= matches!(
        &event,
        SessionEvent::Acp(AcpEvent::Response {
            response: Err(_),
            ..
        })
    );
    let _ = event_tx.send(event);
}

/// Command for controlling ACP session state exposed by the agent.
enum HarnessCommand {
    CurrentSessionId {
        response_tx: oneshot::Sender<anyhow::Result<acp::v1::SessionId>>,
    },
    ControlledSubagentHandle {
        session_id: acp::v1::SessionId,
        response_tx: oneshot::Sender<anyhow::Result<Option<HarnessHandle>>>,
    },
    ChildConfigOptions {
        session_id: acp::v1::SessionId,
        response_tx: oneshot::Sender<anyhow::Result<Vec<acp::v1::SessionConfigOption>>>,
    },
    SetChildConfigOption {
        session_id: acp::v1::SessionId,
        config_id: String,
        value: String,
        response_tx: oneshot::Sender<anyhow::Result<Vec<acp::v1::SessionConfigOption>>>,
    },
    /// Evaluate and execute an append-only Pkl conversation program.
    ExecuteConversationProgram {
        source: String,
        path: Option<PathBuf>,
        /// A skill's arguments; empty for a program.
        arguments: std::collections::BTreeMap<String, String>,
        response_tx: oneshot::Sender<anyhow::Result<ConversationProgramResult>>,
    },
    /// Submit one ACP prompt and return its transport request ID.
    Prompt {
        content: Vec<acp::v1::ContentBlock>,
        meta: Option<acp::v1::Meta>,
        admission: PromptAdmission,
        response_tx: oneshot::Sender<anyhow::Result<acp::v1::RequestId>>,
    },
    /// Prioritize the oldest queued prompt by interrupting the current turn.
    PromoteQueued {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Shut down the active harness session.
    Shutdown {
        child_grace: std::time::Duration,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Respond to a delegated agent-to-client ACP request.
    RespondToAgent {
        request_id: acp::v1::RequestId,
        response: Result<acp::v1::ClientResponse, acp::v1::Error>,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    Cancel {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    CancelRequest {
        request_id: acp::v1::RequestId,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// What the session still has in flight.
    PendingWork {
        response_tx: oneshot::Sender<crate::backend::PendingWork>,
    },
    /// Drop a queued prompt, or one restored from the previous session, by its id.
    DropQueued {
        event_id: String,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Tear down the agent's connection; the session ends and can be resumed.
    StopAgent {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Stop every task the session's agents delegated. Cancelling a turn never does this.
    StopSubagents {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    AddHistory {
        text: String,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    HistoryEntry {
        log_id: i64,
        offset: i64,
        response_tx: oneshot::Sender<anyhow::Result<Option<crate::HistoryEntry>>>,
    },
    SearchHistory {
        max_results: i64,
        response_tx: oneshot::Sender<anyhow::Result<Vec<crate::HistoryEntry>>>,
    },
    CustomPrompts {
        response_tx: oneshot::Sender<Vec<crate::CustomPrompt>>,
    },
    Compact {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    Branch {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    SendSubagentMessage {
        session_id: acp::v1::SessionId,
        content: Vec<acp::v1::ContentBlock>,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    SteerSubagentContent {
        session_id: acp::v1::SessionId,
        content: Vec<acp::v1::ContentBlock>,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    PromoteSubagentQueue {
        session_id: acp::v1::SessionId,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    CancelSubagent {
        session_id: acp::v1::SessionId,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    ChildHistory {
        session_id: acp::v1::SessionId,
        response_tx: oneshot::Sender<anyhow::Result<Vec<SessionEvent>>>,
    },
    ForkChildHistory {
        session_id: acp::v1::SessionId,
        context: String,
        source: (String, i64, i64),
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    RewindChildHistory {
        session_id: acp::v1::SessionId,
        source: (String, i64, i64),
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    RestoreSeededNativeChildren {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    ForkHistory {
        context: String,
        source: Option<(String, i64, i64)>,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    RecordForkSource {
        session_id: String,
        start: i64,
        end: i64,
        context: String,
        entries: Vec<crate::transcript::TranscriptLine>,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    RetireHandoff {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    RecordHandoffAttempt {
        payload: String,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    RecordBackendBound {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    ConversationRecorder {
        response_tx:
            oneshot::Sender<anyhow::Result<Option<Arc<crate::transcript::TranscriptRecorder>>>>,
    },
    RewindAndRebind {
        session_id: String,
        head: Option<i64>,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    Undo {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    UndoSnapshots {
        response_tx: oneshot::Sender<Vec<crate::UndoSnapshot>>,
    },
    UndoTo {
        index: i64,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    RunUserShell {
        command: String,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    SetApprovalPolicy {
        policy: nori_config::AskForApproval,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    Goal {
        response_tx: oneshot::Sender<Option<nori_protocol::ThreadGoal>>,
    },
    SetGoal {
        objective: String,
        status: Option<nori_protocol::ThreadGoalStatus>,
        response_tx: oneshot::Sender<anyhow::Result<nori_protocol::ThreadGoal>>,
    },
    SetGoalStatus {
        status: nori_protocol::ThreadGoalStatus,
        response_tx: oneshot::Sender<anyhow::Result<nori_protocol::ThreadGoal>>,
    },
    ClearGoal {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Get the current ACP session config snapshot.
    GetSessionConfig {
        response_tx: oneshot::Sender<Vec<SessionConfigOption>>,
    },
    /// Set an ACP session config option.
    SetSessionConfigOption {
        config_id: String,
        value: String,
        response_tx: oneshot::Sender<anyhow::Result<Vec<SessionConfigOption>>>,
    },
    /// List the agent's known sessions via ACP `session/list`.
    ListSessions {
        cwd: PathBuf,
        response_tx: oneshot::Sender<anyhow::Result<Vec<acp::v1::SessionInfo>>>,
    },
    /// Close (release) the active session via ACP `session/close`.
    CloseSession {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Register an additional bounded consumer of the public event stream.
    SubscribeEvents {
        response_tx: oneshot::Sender<mpsc::Receiver<SessionEvent>>,
    },
    /// Flush the session transcript to disk (write barrier).
    FlushTranscript {
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    /// The session's skills (`nori:Skill` modules), for the person's command list.
    Skills {
        response_tx: oneshot::Sender<anyhow::Result<Vec<SkillInfo>>>,
    },
    /// The session's plugins (`nori:Plugin` modules) and their commands.
    Plugins {
        response_tx: oneshot::Sender<Vec<crate::backend::plugins::PluginInfo>>,
    },
    /// Run a command of one of the session's plugins.
    RunPlugin {
        plugin: String,
        command: String,
        arguments: std::collections::BTreeMap<String, String>,
        response_tx: oneshot::Sender<
            Result<crate::backend::plugins::PluginRun, crate::backend::plugins::PluginError>,
        >,
    },
}

/// A skill the session can run: what the person's command list shows and what `run_skill` needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub summary: String,
    /// The slash command that runs it, without the slash.
    pub command: String,
    /// Parameters in name order: name, description, and whether it must be given.
    pub parameters: Vec<SkillParameterInfo>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkillParameterInfo {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// Typed handle for controlling one Nori harness session.
#[derive(Clone)]
pub struct HarnessHandle {
    command_tx: mpsc::UnboundedSender<HarnessCommand>,
    /// The session's plugin commands as actions, kept current as its modules load.
    plugin_actions: crate::backend::plugins::PluginActionsFeed,
    first_prompt_started: Option<Arc<FirstPromptStarted>>,
    context_timeline: Arc<tokio::sync::RwLock<crate::context_timeline::ContextTimelineState>>,
}

struct FirstPromptStarted {
    fired: std::sync::atomic::AtomicBool,
    callback: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for HarnessHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessHandle")
            .field("command_tx", &self.command_tx)
            .field(
                "has_first_prompt_started_callback",
                &self.first_prompt_started.is_some(),
            )
            .finish()
    }
}

impl HarnessHandle {
    /// Load and run a `nori:Program` Pkl module through the harness. Its source and outcome are
    /// recorded in the conversation journal and every step in the run's own journal.
    pub async fn execute_conversation_program(
        &self,
        source: String,
    ) -> anyhow::Result<ConversationProgramResult> {
        self.request(|response_tx| HarnessCommand::ExecuteConversationProgram {
            source,
            path: None,
            arguments: std::collections::BTreeMap::new(),
            response_tx,
        })
        .await
    }

    /// Read and run a `nori:Program` module from a local file.
    pub async fn execute_conversation_program_path(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> anyhow::Result<ConversationProgramResult> {
        let path = tokio::fs::canonicalize(path).await?;
        let source = tokio::fs::read_to_string(&path).await?;
        self.request(|response_tx| HarnessCommand::ExecuteConversationProgram {
            source,
            path: Some(path),
            arguments: std::collections::BTreeMap::new(),
            response_tx,
        })
        .await
    }

    /// The session's skills, as their modules declare them.
    pub async fn skills(&self) -> anyhow::Result<Vec<SkillInfo>> {
        self.request(|response_tx| HarnessCommand::Skills { response_tx })
            .await
    }

    /// The session's plugins, as their modules declare them.
    pub async fn plugins(&self) -> anyhow::Result<Vec<crate::backend::plugins::PluginInfo>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::Plugins { response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))
    }

    /// The session's plugin commands as actions, for a frontend's palette and `/name`: all
    /// plugins', replaced whenever the modules load again (empty until they first have).
    pub fn plugin_actions(
        &self,
    ) -> tokio::sync::watch::Receiver<std::sync::Arc<[nori_actions::Action]>> {
        self.plugin_actions.subscribe()
    }

    /// Run the plugin command an action stands for. The invocation's argument becomes the
    /// command's arguments (one parameter takes it whole, several take `name=value` pairs and a
    /// bare value fills the first required one); then this is `run_plugin`. An id that names no
    /// plugin command, or an argument the command cannot take, is an error that says so.
    pub async fn run_plugin_action(
        &self,
        invocation: nori_actions::Invocation,
    ) -> Result<crate::backend::plugins::PluginRun, crate::backend::plugins::PluginError> {
        let plugins = self
            .plugins()
            .await
            .map_err(|_| crate::backend::plugins::PluginError::SessionClosed)?;
        let (plugin, command, arguments) = crate::backend::plugins::resolve(&plugins, &invocation)?;
        self.run_plugin(&plugin, &command, arguments).await
    }

    /// Run command `command` of the session's plugin `plugin` with `arguments`. Every operation
    /// is something Nori already does (see `docs/pkl-runtime.md#plugins`); the first to fail
    /// stops the command, and the error names it. The run is journaled as one `plugin` command.
    pub async fn run_plugin(
        &self,
        plugin: &str,
        command: &str,
        arguments: std::collections::BTreeMap<String, String>,
    ) -> Result<crate::backend::plugins::PluginRun, crate::backend::plugins::PluginError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::RunPlugin {
                plugin: plugin.to_owned(),
                command: command.to_owned(),
                arguments,
                response_tx,
            })
            .map_err(|_| crate::backend::plugins::PluginError::SessionClosed)?;
        response_rx
            .await
            .map_err(|_| crate::backend::plugins::PluginError::SessionClosed)?
    }

    /// Run the skill `name` (a `nori:Skill` module of the session's) with `arguments`: the
    /// person's `/name ...`. The agent runs skills with the `skill` tool.
    pub async fn run_skill(
        &self,
        name: &str,
        arguments: std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<ConversationProgramResult> {
        let path = self
            .skills()
            .await?
            .into_iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.path)
            .ok_or_else(|| anyhow::anyhow!("there is no skill named {name}"))?;
        let source = tokio::fs::read_to_string(&path).await?;
        self.request(|response_tx| HarnessCommand::ExecuteConversationProgram {
            source,
            path: Some(path),
            arguments,
            response_tx,
        })
        .await
    }
    /// Return the context observations captured at prompt boundaries.
    pub async fn context_timeline(
        &self,
    ) -> anyhow::Result<crate::context_timeline::ContextTimeline> {
        let waiting = std::time::Instant::now();
        let state = self.context_timeline.read().await;
        let lock_wait_us = waiting.elapsed().as_secs_f64() * 1_000_000.0;
        let cloning = std::time::Instant::now();
        let timeline = state.timeline.clone();
        let clone_us = cloning.elapsed().as_secs_f64() * 1_000_000.0;
        drop(state);
        tracing::debug!(
            target: "nori_context_profile",
            stage = "harness_snapshot",
            lock_wait_us,
            clone_us,
            turn_count = timeline.turns.len(),
            segment_count = timeline.segments.len(),
            "captured context timeline snapshot",
        );
        Ok(timeline)
    }

    /// Attach a callback that fires once, after this session's first prompt
    /// reaches the ACP transport and receives its wire request id.
    pub fn with_first_prompt_started_callback(
        mut self,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        self.first_prompt_started = Some(Arc::new(FirstPromptStarted {
            fired: std::sync::atomic::AtomicBool::new(false),
            callback: Box::new(callback),
        }));
        self
    }

    /// Submit a prompt and return the ACP request ID used on the wire.
    pub async fn prompt(
        &self,
        content: Vec<acp::v1::ContentBlock>,
    ) -> anyhow::Result<acp::v1::RequestId> {
        self.prompt_with_meta(content, None).await
    }

    /// Submit a prompt whose user-message echo carries `echo`, so a client that already shows the
    /// prompt can recognize the echo by id.
    pub async fn prompt_echoed(
        &self,
        content: Vec<acp::v1::ContentBlock>,
        echo: &nori_protocol::PromptEchoId,
    ) -> anyhow::Result<acp::v1::RequestId> {
        self.prompt_with_meta(content, Some(echo.to_meta())).await
    }

    /// Interrupt the active turn and start this content ahead of queued prompts.
    pub async fn steer_echoed(
        &self,
        content: Vec<acp::v1::ContentBlock>,
        echo: &nori_protocol::PromptEchoId,
    ) -> anyhow::Result<acp::v1::RequestId> {
        self.prompt_with_meta_and_admission(content, Some(echo.to_meta()), PromptAdmission::Steer)
            .await
    }

    /// Interrupt the active turn and start the oldest queued prompt, if any.
    pub async fn promote_next_queued(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::PromoteQueued { response_tx })
            .await
    }

    /// Steer this session with schema-native ACP content.
    pub async fn steer_content(
        &self,
        content: Vec<acp::v1::ContentBlock>,
    ) -> anyhow::Result<acp::v1::RequestId> {
        self.prompt_with_meta_and_admission(content, None, PromptAdmission::Steer)
            .await
    }

    /// Retry an already recorded prompt after a provider handoff.
    pub async fn prompt_handoff(
        &self,
        content: Vec<acp::v1::ContentBlock>,
        echo: &nori_protocol::PromptEchoId,
    ) -> anyhow::Result<acp::v1::RequestId> {
        let mut meta = echo.to_meta();
        meta.insert(
            nori_protocol::HANDOFF_RETRY_META_KEY.to_owned(),
            serde_json::Value::Bool(true),
        );
        self.prompt_with_meta(content, Some(meta)).await
    }

    pub(crate) async fn prompt_with_meta(
        &self,
        content: Vec<acp::v1::ContentBlock>,
        meta: Option<acp::v1::Meta>,
    ) -> anyhow::Result<acp::v1::RequestId> {
        self.prompt_with_meta_and_admission(content, meta, PromptAdmission::Queue)
            .await
    }

    pub(crate) async fn prompt_with_meta_if_idle(
        &self,
        content: Vec<acp::v1::ContentBlock>,
        meta: Option<acp::v1::Meta>,
    ) -> anyhow::Result<acp::v1::RequestId> {
        self.prompt_with_meta_and_admission(content, meta, PromptAdmission::RejectIfBusy)
            .await
    }

    async fn prompt_with_meta_and_admission(
        &self,
        content: Vec<acp::v1::ContentBlock>,
        meta: Option<acp::v1::Meta>,
        admission: PromptAdmission,
    ) -> anyhow::Result<acp::v1::RequestId> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::Prompt {
                content,
                meta,
                admission,
                response_tx,
            })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        let request_id = response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not accept the prompt"))??;
        if let Some(first_prompt_started) = &self.first_prompt_started
            && !first_prompt_started
                .fired
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            (first_prompt_started.callback)();
        }
        Ok(request_id)
    }

    /// Shut down the active harness session.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.shutdown_with_grace(std::time::Duration::ZERO).await
    }

    /// Shut down the active harness session after allowing the ACP child to
    /// process stdin EOF for `child_grace`.
    pub async fn shutdown_with_grace(
        &self,
        child_grace: std::time::Duration,
    ) -> anyhow::Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::Shutdown {
                child_grace,
                response_tx,
            })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not shut down"))?
    }

    /// Complete a delegated ACP request using its original request ID.
    pub async fn respond_to_agent(
        &self,
        request_id: acp::v1::RequestId,
        response: Result<acp::v1::ClientResponse, acp::v1::Error>,
    ) -> anyhow::Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::RespondToAgent {
                request_id,
                response,
                response_tx,
            })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP request response was not accepted"))?
    }

    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::Cancel { response_tx })
            .await
    }

    /// What the session still has in flight: a turn, queued prompts, delegated tasks, results
    /// owed, mail queued. A frontend that must not finish early waits until none is left.
    pub async fn pending_work(&self) -> anyhow::Result<crate::backend::PendingWork> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::PendingWork { response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent stopped before reporting its pending work"))
    }

    /// Drop a queued prompt, or one restored from the previous session, by its id.
    pub async fn drop_queued(&self, event_id: String) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::DropQueued {
            event_id,
            response_tx,
        })
        .await
    }

    /// Stop a runaway agent by tearing down its connection. The open turn ends as if the
    /// connection had died, and the session ends as `ConnectionLost` so it can be resumed.
    pub async fn stop_agent(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::StopAgent { response_tx })
            .await
    }

    /// Stop every subagent the session's agents started. Cancelling a turn never does this: the
    /// children are independent of the parent's turn, and only this command, a lane's stop or
    /// `task_cancel` ends them.
    pub async fn stop_subagents(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::StopSubagents { response_tx })
            .await
    }

    pub(crate) async fn cancel_request(
        &self,
        request_id: acp::v1::RequestId,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::CancelRequest {
            request_id,
            response_tx,
        })
        .await
    }

    pub async fn add_history(&self, text: String) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::AddHistory { text, response_tx })
            .await
    }

    pub async fn history_entry(
        &self,
        log_id: i64,
        offset: i64,
    ) -> anyhow::Result<Option<crate::HistoryEntry>> {
        self.request(|response_tx| HarnessCommand::HistoryEntry {
            log_id,
            offset,
            response_tx,
        })
        .await
    }

    pub async fn search_history(
        &self,
        max_results: i64,
    ) -> anyhow::Result<Vec<crate::HistoryEntry>> {
        self.request(|response_tx| HarnessCommand::SearchHistory {
            max_results,
            response_tx,
        })
        .await
    }

    pub async fn custom_prompts(&self) -> anyhow::Result<Vec<crate::CustomPrompt>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::CustomPrompts { response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))
    }

    pub async fn compact(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::Compact { response_tx })
            .await
    }

    /// Branch the conversation at its current head via ACP `session/fork`.
    pub async fn branch(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::Branch { response_tx })
            .await
    }

    /// Submit a follow-up to a child session.
    pub async fn send_subagent_message(
        &self,
        session_id: acp::v1::SessionId,
        text: String,
    ) -> anyhow::Result<()> {
        self.send_subagent_content(
            session_id,
            vec![acp::v1::ContentBlock::Text(acp::v1::TextContent::new(text))],
        )
        .await
    }

    /// Submit ACP content to a child session while preserving the parent task's lifecycle.
    pub async fn send_subagent_content(
        &self,
        session_id: acp::v1::SessionId,
        content: Vec<acp::v1::ContentBlock>,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::SendSubagentMessage {
            session_id,
            content,
            response_tx,
        })
        .await
    }

    /// Steer a controlled child while preserving the parent's task lifecycle.
    pub async fn steer_subagent_content(
        &self,
        session_id: acp::v1::SessionId,
        content: Vec<acp::v1::ContentBlock>,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::SteerSubagentContent {
            session_id,
            content,
            response_tx,
        })
        .await
    }

    /// Promote the oldest queued prompt in a controlled child.
    pub async fn promote_subagent_queue(
        &self,
        session_id: acp::v1::SessionId,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::PromoteSubagentQueue {
            session_id,
            response_tx,
        })
        .await
    }

    /// Cancel the current turn in a controlled child.
    pub async fn cancel_subagent(&self, session_id: acp::v1::SessionId) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::CancelSubagent {
            session_id,
            response_tx,
        })
        .await
    }

    /// Read the selected child's own recorded history.
    pub async fn child_history(
        &self,
        session_id: acp::v1::SessionId,
    ) -> anyhow::Result<Vec<SessionEvent>> {
        self.request(|response_tx| HarnessCommand::ChildHistory {
            session_id,
            response_tx,
        })
        .await
    }

    /// Fork a selected range of a child's history.
    pub async fn fork_child_history(
        &self,
        session_id: acp::v1::SessionId,
        context: String,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::ForkChildHistory {
            session_id,
            context,
            source,
            response_tx,
        })
        .await
    }

    /// Rewind a child to a selected range of its own history.
    pub async fn rewind_child_history(
        &self,
        session_id: acp::v1::SessionId,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RewindChildHistory {
            session_id,
            source,
            response_tx,
        })
        .await
    }

    pub(crate) async fn restore_seeded_native_children(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RestoreSeededNativeChildren { response_tx })
            .await
    }

    /// Start a child from a selected history prefix without replacing this session.
    pub async fn fork_history(
        &self,
        context: String,
        source: Option<(String, i64, i64)>,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::ForkHistory {
            context,
            source,
            response_tx,
        })
        .await
    }

    pub(crate) async fn record_fork_source(
        &self,
        session_id: String,
        start: i64,
        end: i64,
        context: String,
        entries: Vec<crate::transcript::TranscriptLine>,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RecordForkSource {
            session_id,
            start,
            end,
            context,
            entries,
            response_tx,
        })
        .await
    }

    /// Stop recording events from a superseded provider session.
    pub async fn retire_handoff(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RetireHandoff { response_tx })
            .await
    }

    /// Record a provider handoff attempt or outcome in the canonical journal.
    pub async fn record_handoff_attempt(&self, payload: String) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RecordHandoffAttempt {
            payload,
            response_tx,
        })
        .await
    }

    /// Record the active provider binding after activation has succeeded.
    pub async fn record_backend_bound(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RecordBackendBound { response_tx })
            .await
    }

    /// Obtain the shared recorder for a provider handoff.
    pub async fn conversation_recorder(
        &self,
    ) -> anyhow::Result<Option<Arc<crate::transcript::TranscriptRecorder>>> {
        self.request(|response_tx| HarnessCommand::ConversationRecorder { response_tx })
            .await
    }

    /// Rewind and bind a fresh ACP session to the resulting conversation.
    pub async fn rewind_and_rebind(
        &self,
        session_id: String,
        head: Option<i64>,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RewindAndRebind {
            session_id,
            head,
            response_tx,
        })
        .await
    }

    pub async fn undo(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::Undo { response_tx })
            .await
    }

    pub async fn undo_snapshots(&self) -> anyhow::Result<Vec<crate::UndoSnapshot>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::UndoSnapshots { response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))
    }

    pub async fn undo_to(&self, index: i64) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::UndoTo { index, response_tx })
            .await
    }

    pub async fn run_user_shell(&self, command: String) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::RunUserShell {
            command,
            response_tx,
        })
        .await
    }

    pub async fn set_approval_policy(
        &self,
        policy: nori_config::AskForApproval,
    ) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::SetApprovalPolicy {
            policy,
            response_tx,
        })
        .await
    }

    pub async fn goal(&self) -> anyhow::Result<Option<nori_protocol::ThreadGoal>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::Goal { response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))
    }

    pub async fn set_goal(
        &self,
        objective: String,
        status: Option<nori_protocol::ThreadGoalStatus>,
    ) -> anyhow::Result<nori_protocol::ThreadGoal> {
        self.request(|response_tx| HarnessCommand::SetGoal {
            objective,
            status,
            response_tx,
        })
        .await
    }

    pub async fn set_goal_status(
        &self,
        status: nori_protocol::ThreadGoalStatus,
    ) -> anyhow::Result<nori_protocol::ThreadGoal> {
        self.request(|response_tx| HarnessCommand::SetGoalStatus {
            status,
            response_tx,
        })
        .await
    }

    pub async fn clear_goal(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::ClearGoal { response_tx })
            .await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<anyhow::Result<T>>) -> HarnessCommand,
    ) -> anyhow::Result<T> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(command(response_tx))
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))?
    }

    /// Get the current ACP session config snapshot from the agent.
    pub async fn get_session_config(&self) -> Option<Vec<SessionConfigOption>> {
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .command_tx
            .send(HarnessCommand::GetSessionConfig { response_tx })
            .is_err()
        {
            return None;
        }
        response_rx.await.ok()
    }

    /// Set an ACP session config option value.
    pub async fn set_session_config_option(
        &self,
        config_id: String,
        value: String,
    ) -> anyhow::Result<Vec<SessionConfigOption>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::SetSessionConfigOption {
                config_id,
                value,
                response_tx,
            })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))?
    }

    /// List the agent's known sessions via ACP `session/list`.
    pub async fn list_sessions(&self, cwd: PathBuf) -> anyhow::Result<Vec<acp::v1::SessionInfo>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::ListSessions { cwd, response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))?
    }

    /// Close (release) the active session via ACP `session/close`.
    pub async fn close_session(&self) -> anyhow::Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::CloseSession { response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))?
    }

    /// Subscribe to the ordered public event stream as an additional bounded
    /// consumer. The primary receiver from [`launch_session`] is unaffected.
    /// A subscriber that falls a full queue behind is dropped and its
    /// receiver closes.
    pub async fn subscribe_events(&self) -> anyhow::Result<mpsc::Receiver<SessionEvent>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(HarnessCommand::SubscribeEvents { response_tx })
            .map_err(|_| anyhow::anyhow!("ACP agent command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("ACP agent did not respond"))
    }

    /// Flush the session transcript to disk and wait for the write barrier.
    pub async fn flush_transcript(&self) -> anyhow::Result<()> {
        self.request(|response_tx| HarnessCommand::FlushTranscript { response_tx })
            .await
    }
}

/// Resume parameters for reattaching to a previous session.
#[derive(Debug, Clone)]
pub struct SessionResume {
    /// The agent-side ACP session id, when known (enables `session/load`).
    pub acp_session_id: Option<String>,
    /// Transcript for client-side replay when the agent can't load sessions.
    pub transcript: Option<Transcript>,
}

/// Everything a frontend must supply to initialize and inspect an ACP agent.
pub struct AgentPrepareSpec {
    /// Fully resolved process configuration supplied by the frontend.
    pub config: Arc<crate::config::NoriConfig>,
    /// Frontend version recorded in transcript metadata after activation.
    pub cli_version: String,
    /// Product-level context injected into the first prompt after activation.
    pub session_context: Option<SessionContext>,
    /// Conversation history injected into the first prompt after activation.
    pub initial_context: Option<String>,
    /// Shared canonical recorder retained across a provider handoff.
    pub handoff: Option<Arc<crate::transcript::TranscriptRecorder>>,
}

/// Explicit session directive applied to a prepared ACP connection.
#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "SessionStart is a one-shot control value and keeps the public resume API direct"
)]
pub enum SessionStart {
    /// Create a fresh ACP session.
    New,
    /// Load, resume, or replay an existing session.
    Resume(SessionResume),
}

/// Everything a frontend must supply to activate a prepared connection.
pub struct SessionLaunchSpec {
    /// Uniquely owned initialized ACP connection.
    pub agent: PreparedAgent,
    /// Session action chosen after inspecting the agent.
    pub start: SessionStart,
}

/// A running session and its ordered event stream.
pub struct LaunchedSession {
    /// Typed handle for ACP and Nori session operations.
    pub handle: HarnessHandle,
    /// Session event stream; closes when the backend shuts down.
    pub events: UnboundedReceiver<SessionEvent>,
}

/// Initialize an ACP connection and inspect its session catalog without
/// creating, loading, or resuming a session.
pub async fn prepare_agent(spec: AgentPrepareSpec) -> anyhow::Result<PreparedAgent> {
    PreparedAgent::prepare(build_backend_config(spec))
        .await
        .map_err(|failure| failure.error)
}

/// Refresh session-time configuration after preparation but before activation.
///
/// The initialized transport remains unchanged. The agent identity and working
/// directory must still match the connection that was prepared.
pub fn refresh_prepared_agent(
    agent: &mut PreparedAgent,
    spec: AgentPrepareSpec,
) -> anyhow::Result<()> {
    agent.replace_activation_config(build_backend_config(spec))
}

fn build_backend_config(spec: AgentPrepareSpec) -> AcpBackendConfig {
    let AgentPrepareSpec {
        config,
        cli_version,
        session_context,
        initial_context,
        handoff,
    } = spec;

    // Detect auto-worktree repo root from the cwd path.
    // When auto_worktree is enabled, cwd is {repo_root}/.worktrees/{name},
    // so we can derive repo_root by going up two directories.
    let auto_worktree_repo_root = if config.auto_worktree.is_enabled() {
        config
            .cwd
            .parent()
            .filter(|path| path.file_name().is_some_and(|name| name == ".worktrees"))
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
    } else {
        None
    };
    // Resolve to Off if no worktree actually exists (e.g. "ask" mode
    // where the user declined).
    let auto_worktree = if auto_worktree_repo_root.is_some() {
        config.auto_worktree
    } else {
        crate::config::AutoWorktree::Off
    };
    let agent = config.active_agent.clone();
    let delegation =
        crate::backend::orchestrator::Seed::root(Arc::clone(&config), cli_version.clone());

    AcpBackendConfig {
        handoff,
        agent: agent.clone(),
        cwd: config.cwd.clone(),
        approval_policy: config.approval_policy,
        sandbox_policy: config.sandbox_policy.clone(),
        notify: config.notify.clone(),
        os_notifications: config.os_notifications,
        notify_after_idle: config.notify_after_idle,
        nori_home: config.nori_home.clone(),
        history_persistence: config.history_persistence,
        acp_proxy: config.acp_proxy.clone(),
        cli_version,
        auto_worktree,
        auto_worktree_repo_root,
        prompt_summary_enabled: config.footer_segment_config.prompt_summary,
        subagent_sessions: config.subagent_sessions,
        nudox: config.nudox,
        session_start_hooks: config.session_start_hooks.clone(),
        session_end_hooks: config.session_end_hooks.clone(),
        pre_user_prompt_hooks: config.pre_user_prompt_hooks.clone(),
        post_user_prompt_hooks: config.post_user_prompt_hooks.clone(),
        pre_tool_call_hooks: config.pre_tool_call_hooks.clone(),
        post_tool_call_hooks: config.post_tool_call_hooks.clone(),
        pre_agent_response_hooks: config.pre_agent_response_hooks.clone(),
        post_agent_response_hooks: config.post_agent_response_hooks.clone(),
        async_session_start_hooks: config.async_session_start_hooks.clone(),
        async_session_end_hooks: config.async_session_end_hooks.clone(),
        async_pre_user_prompt_hooks: config.async_pre_user_prompt_hooks.clone(),
        async_post_user_prompt_hooks: config.async_post_user_prompt_hooks.clone(),
        async_pre_tool_call_hooks: config.async_pre_tool_call_hooks.clone(),
        async_post_tool_call_hooks: config.async_post_tool_call_hooks.clone(),
        async_pre_agent_response_hooks: config.async_pre_agent_response_hooks.clone(),
        async_post_agent_response_hooks: config.async_post_agent_response_hooks.clone(),
        script_timeout: config.script_timeout.as_duration(),
        default_model: config.default_models.get(&agent).cloned(),
        initial_context,
        session_context,
        mcp_servers: config.mcp_servers.clone(),
        mcp_oauth_credentials_store_mode: OAuthCredentialsStoreMode::default(),
        delegation,
    }
}

/// Prepare the agent of a delegated task's child, one level below `parent` in its tree.
pub(crate) async fn prepare_delegated_agent(
    spec: AgentPrepareSpec,
    parent: &crate::backend::orchestrator::Seed,
) -> anyhow::Result<PreparedAgent> {
    let mut config = build_backend_config(spec);
    config.delegation = parent.child(Arc::clone(&config.delegation.config));
    PreparedAgent::prepare(config)
        .await
        .map_err(|failure| failure.error)
}

/// Activate a prepared ACP connection with the selected session directive.
///
/// Returns immediately; activation happens on a background task and handle
/// calls queue until the session is ready.
pub fn launch_session(spec: SessionLaunchSpec) -> LaunchedSession {
    launch_session_from(
        SessionAgentSource::Prepared(Box::new(spec.agent)),
        spec.start,
    )
}

/// Prepare an agent and activate the selected session on the same connection.
///
/// This convenience path is for products that have already made an explicit
/// session choice and therefore do not need to render a pre-session picker.
/// Preparation still happens separately and its connection is never respawned.
pub fn prepare_and_launch_session(spec: AgentPrepareSpec, start: SessionStart) -> LaunchedSession {
    launch_session_from(SessionAgentSource::Prepare(spec), start)
}

enum SessionAgentSource {
    Prepared(Box<PreparedAgent>),
    Prepare(AgentPrepareSpec),
}

fn launch_session_from(source: SessionAgentSource, start: SessionStart) -> LaunchedSession {
    let (agent_cmd_tx, mut agent_cmd_rx) = unbounded_channel::<HarnessCommand>();
    let (primary_event_tx, event_rx) = unbounded_channel::<SessionEvent>();
    let event_tx = SessionEventFanout::new(primary_event_tx);

    let initial_timeline = match &start {
        SessionStart::Resume(resume) => resume
            .transcript
            .as_ref()
            .map(Transcript::context_timeline_state)
            .unwrap_or_else(|| {
                let mut state = crate::context_timeline::ContextTimelineState::default();
                state.timeline.incomplete_history = true;
                state
            }),
        SessionStart::New => crate::context_timeline::ContextTimelineState::default(),
    };
    let context_timeline = Arc::new(tokio::sync::RwLock::new(initial_timeline));
    let plugin_actions = crate::backend::plugins::PluginActionsFeed::default();
    let handle = HarnessHandle {
        command_tx: agent_cmd_tx,
        plugin_actions: plugin_actions.clone(),
        first_prompt_started: None,
        context_timeline: Arc::clone(&context_timeline),
    };

    tokio::spawn(async move {
        // Single ACP backend channel for both control-plane and normalized
        // session-domain events.
        let (backend_event_tx, mut backend_event_rx) = mpsc::channel(32);
        // Every backend this session ever has reports here, so a replacement agent's events
        // join the same ordered stream.
        let handoff_events = backend_event_tx.clone();

        let connect = async move {
            let agent = match source {
                SessionAgentSource::Prepared(agent) => *agent,
                SessionAgentSource::Prepare(spec) => {
                    match PreparedAgent::prepare(build_backend_config(spec)).await {
                        Ok(agent) => agent,
                        Err(failure) => {
                            for event in failure.setup_events {
                                let _ = backend_event_tx.send(BackendEvent::Public(event)).await;
                            }
                            let failure_label = match &start {
                                SessionStart::New => "Failed to start ACP session",
                                SessionStart::Resume(_) => "Failed to resume ACP session",
                            };
                            return Err((failure_label, failure.error));
                        }
                    }
                }
            };
            let start = match start {
                SessionStart::New => agent
                    .automatic_session_id()
                    .map(|session_id| {
                        SessionStart::Resume(SessionResume {
                            acp_session_id: Some(session_id.to_string()),
                            transcript: None,
                        })
                    })
                    .unwrap_or(SessionStart::New),
                resume @ SessionStart::Resume(_) => resume,
            };
            let failure_label = match &start {
                SessionStart::New => "Failed to start ACP session",
                SessionStart::Resume(_) => "Failed to resume ACP session",
            };
            let result = match start {
                SessionStart::New => AcpBackend::spawn(agent, backend_event_tx).await,
                SessionStart::Resume(resume) => {
                    AcpBackend::resume_session(
                        agent,
                        resume.acp_session_id.as_deref(),
                        resume.transcript.as_ref(),
                        backend_event_tx,
                    )
                    .await
                }
            };
            result.map_err(|error| (failure_label, error))
        };

        // Race backend init against shutdown requests and a timeout.
        // This ensures the user can always exit even if the backend hangs.
        let mut connect = std::pin::pin!(connect);
        let mut timeout = std::pin::pin!(spawn_timeout_sequence(&event_tx));
        let mut pending_commands = VecDeque::new();
        let mut raw_acp_error_observed = false;
        let mut startup_evidence = Vec::new();
        let backend = loop {
            tokio::select! {
            result = &mut connect => {
                match result {
                    Ok(b) => break Arc::new(b),
                    Err((failure_label, e)) => {
                        tracing::error!("{failure_label}: {e}");
                        let message = format!("{failure_label}: {e}");
                        while let Ok(BackendEvent::Public(event)) = backend_event_rx.try_recv() {
                            forward_connect_event(
                                &event_tx,
                                event,
                                &mut raw_acp_error_observed,
                            );
                        }
                        if !raw_acp_error_observed {
                            let _ = event_tx.send(SessionEvent::Nori(NoriEvent::RequestFailed(
                                RequestFailure {
                                    request_id: None,
                                    message: message.clone(),
                                    kind: RequestFailureKind::Fatal,
                                },
                            )));
                        }
                        let _ = event_tx.send(SessionEvent::Nori(NoriEvent::SessionEnded(
                            SessionEnded {
                                reason: SessionEndReason::SpawnFailed,
                                message: Some(message),
                            },
                        )));
                        return;
                    }
                }
            }
            command = agent_cmd_rx.recv() => {
                match command {
                    Some(HarnessCommand::Shutdown { response_tx, .. }) => {
                        let _ = response_tx.send(Ok(()));
                        let _ = event_tx.send(SessionEvent::Nori(NoriEvent::SessionEnded(
                            SessionEnded {
                                reason: SessionEndReason::Shutdown,
                                message: None,
                            },
                        )));
                        return;
                    }
                    Some(HarnessCommand::GetSessionConfig { response_tx }) => {
                        let _ = response_tx.send(Vec::new());
                    }
                    Some(HarnessCommand::CurrentSessionId { response_tx }) => {
                        let _ = response_tx.send(Err(anyhow::anyhow!("session is still preparing")));
                    }
                    Some(HarnessCommand::ConversationRecorder { response_tx }) => {
                        let _ = response_tx.send(Ok(None));
                    }
                    Some(HarnessCommand::RewindAndRebind { response_tx, .. }) => {
                        let _ = response_tx.send(Err(anyhow::anyhow!("session is still preparing")));
                    }
                    Some(HarnessCommand::Cancel { response_tx }) => {
                        let mut remaining = VecDeque::new();
                        while let Some(command) = pending_commands.pop_front() {
                            match command {
                                HarnessCommand::Prompt { response_tx, .. } => {
                                    let _ = response_tx.send(Err(anyhow::anyhow!("prompt cancelled during session preparation")));
                                }
                                other => remaining.push_back(other),
                            }
                        }
                        pending_commands = remaining;
                        let _ = response_tx.send(Ok(()));
                    }
                    // Register immediately (not queued) so a subscriber
                    // attached right after launch cannot miss startup events
                    // such as `SessionStarted`.
                    Some(HarnessCommand::SubscribeEvents { response_tx }) => {
                        let _ = response_tx.send(event_tx.subscribe());
                    }
                    Some(command) => pending_commands.push_back(command),
                    None => return,
                }
            }
            Some(BackendEvent::Public(event)) = backend_event_rx.recv() => {
                startup_evidence.push(observe_context_event(&context_timeline, &event).await);
                forward_connect_event(&event_tx, event, &mut raw_acp_error_observed);
            }
            () = &mut timeout => {
                tracing::warn!("ACP backend connection timed out");
                let message = "Connection timed out. The agent did not respond.".to_string();
                let _ = event_tx.send(SessionEvent::Nori(NoriEvent::RequestFailed(
                    RequestFailure {
                        request_id: None,
                        message: message.clone(),
                        kind: RequestFailureKind::Retryable,
                    },
                )));
                let _ = event_tx.send(SessionEvent::Nori(NoriEvent::SessionEnded(
                    SessionEnded {
                        reason: SessionEndReason::TimedOut,
                        message: Some(message),
                    },
                )));
                return;
            }
            }
        };

        // Hold the shared recorder cell (not a one-time clone) so a branch-at-head
        // fork swap redirects recording to the new transcript; otherwise post-fork
        // events would keep landing in the frozen parent transcript.
        // The recorder cell of whichever backend runs now: a replacement agent brings its own.
        let recorder_cell = Arc::new(std::sync::Mutex::new(backend.transcript_recorder_cell()));
        let transcript_recorder_cell = backend.transcript_recorder_cell();
        let recording_enabled = Arc::new(std::sync::atomic::AtomicBool::new(!backend.is_handoff()));
        if recording_enabled.load(std::sync::atomic::Ordering::Acquire)
            && let Some(recorder) = transcript_recorder_cell.read().await.clone()
        {
            for evidence in startup_evidence {
                if let Err(error) = recorder.record_session_event(&evidence).await {
                    tracing::warn!(%error, "failed to record startup context evidence");
                }
            }
        }
        let command_recorder_cell = Arc::clone(&recorder_cell);
        let command_recording_enabled = Arc::clone(&recording_enabled);
        backend.bind_program_runtime();
        plugin_actions.follow(&backend);
        backend.publish_events_to_monitors(event_tx.clone());
        let (handoff_tx, mut handoff_rx) =
            unbounded_channel::<crate::recovery::handoff::HandoffRequest>();
        backend.bind_handoff(handoff_tx.clone());
        let mut backend_for_agent = Arc::clone(&backend);
        let mut prompt_queue = PromptQueue::start(Arc::clone(&backend));
        let config_lane = config_lane::ConfigLane::start();
        let mut child_admissions = child_admission::ChildAdmissions::default();
        let event_fanout = event_tx.clone();
        tokio::spawn(async move {
            loop {
                let cmd = match pending_commands.pop_front() {
                    Some(command) => command,
                    None => tokio::select! {
                        request = handoff_rx.recv() => {
                            if let Some(request) = request {
                                handoff::run(
                                    handoff::Live {
                                        plugin_actions: &plugin_actions,
                                        backend: &mut backend_for_agent,
                                        prompt_queue: &mut prompt_queue,
                                        requests: &handoff_tx,
                                        events: &handoff_events,
                                        fanout: &event_fanout,
                                        recorder_cell: &command_recorder_cell,
                                    },
                                    request,
                                    &mut agent_cmd_rx,
                                    &mut pending_commands,
                                )
                                .await;
                            }
                            continue;
                        }
                        command = agent_cmd_rx.recv() => match command {
                            Some(command) => command,
                            None => break,
                        },
                    },
                };
                let (command_name, command_payload) = match &cmd {
                    HarnessCommand::CurrentSessionId { .. } => ("current-session-id", "{}".to_owned()),
                    HarnessCommand::ControlledSubagentHandle { session_id, .. } => (
                        "controlled-subagent-handle",
                        serde_json::json!({"session_id": session_id}).to_string(),
                    ),
                    HarnessCommand::ChildConfigOptions { session_id, .. } => ("child-config-options", serde_json::json!({"session_id":session_id}).to_string()),
                    HarnessCommand::SetChildConfigOption { session_id, config_id, value, .. } => ("set-child-config-option", serde_json::json!({"session_id":session_id, "config_id":config_id, "value":value}).to_string()),
                    HarnessCommand::ExecuteConversationProgram { source, path, arguments, .. } => (
                        "execute-conversation-program",
                        serde_json::json!({"source": source, "path": path, "arguments": arguments}).to_string(),
                    ),
                    HarnessCommand::Prompt { content, meta, .. } => (
                        "prompt",
                        serde_json::json!({"content": content, "meta": meta}).to_string(),
                    ),
                    HarnessCommand::PromoteQueued { .. } => ("promote-queued", "{}".to_owned()),
                    HarnessCommand::Shutdown { child_grace, .. } => (
                        "shutdown",
                        serde_json::json!({"child_grace_ms": child_grace.as_millis()}).to_string(),
                    ),
                    HarnessCommand::RespondToAgent {
                        request_id,
                        response,
                        ..
                    } => (
                        "respond-to-agent",
                        serde_json::json!({"request_id": request_id, "response": response})
                            .to_string(),
                    ),
                    HarnessCommand::Cancel { .. } => ("cancel", "{}".to_owned()),
                    HarnessCommand::PendingWork { .. } => ("pending-work", "{}".to_owned()),
                    HarnessCommand::StopAgent { .. } => ("stop-agent", "{}".to_owned()),
                    HarnessCommand::StopSubagents { .. } => ("stop-subagents", "{}".to_owned()),
                    HarnessCommand::DropQueued { event_id, .. } => (
                        "drop-queued",
                        serde_json::json!({"event_id": event_id}).to_string(),
                    ),
                    HarnessCommand::CancelRequest { request_id, .. } => (
                        "cancel-request",
                        serde_json::json!({"request_id": request_id}).to_string(),
                    ),
                    HarnessCommand::AddHistory { text, .. } => {
                        ("add-history", serde_json::json!({"text": text}).to_string())
                    }
                    HarnessCommand::HistoryEntry { log_id, offset, .. } => (
                        "history-entry",
                        serde_json::json!({"log_id": log_id, "offset": offset}).to_string(),
                    ),
                    HarnessCommand::SearchHistory { max_results, .. } => (
                        "search-history",
                        serde_json::json!({"max_results": max_results}).to_string(),
                    ),
                    HarnessCommand::CustomPrompts { .. } => ("custom-prompts", "{}".to_owned()),
                    HarnessCommand::Compact { .. } => ("compact", "{}".to_owned()),
                    HarnessCommand::Branch { .. } => ("branch", "{}".to_owned()),
                    HarnessCommand::SendSubagentMessage { session_id, content, .. } => ("send-subagent-message", serde_json::json!({"session_id": session_id, "content": content}).to_string()),
                    HarnessCommand::SteerSubagentContent { session_id, content, .. } => ("steer-subagent", serde_json::json!({"session_id": session_id, "content": content}).to_string()),
                    HarnessCommand::PromoteSubagentQueue { session_id, .. } => ("promote-subagent-queue", serde_json::json!({"session_id": session_id}).to_string()),
                    HarnessCommand::CancelSubagent { session_id, .. } => ("cancel-subagent", serde_json::json!({"session_id": session_id}).to_string()),
                    HarnessCommand::ChildHistory { session_id, .. } => ("child-history", serde_json::json!({"session_id": session_id}).to_string()),
                    HarnessCommand::ForkChildHistory { session_id, context, source, .. } => ("fork-child-history", serde_json::json!({"session_id": session_id, "context": context, "source": source}).to_string()),
                    HarnessCommand::RewindChildHistory { session_id, source, .. } => ("rewind-child-history", serde_json::json!({"session_id": session_id, "source": source}).to_string()),
                    HarnessCommand::RestoreSeededNativeChildren { .. } => ("restore-seeded-native-children", "{}".to_owned()),
                    HarnessCommand::ForkHistory {
                        context, source, ..
                    } => (
                        "fork-history",
                        serde_json::json!({"context": context, "source": source}).to_string(),
                    ),
                    HarnessCommand::RecordForkSource {
                        session_id,
                        start,
                        end,
                        ..
                    } => (
                        "record-fork-source",
                        serde_json::json!({"session_id": session_id, "start": start, "end": end})
                            .to_string(),
                    ),
                    HarnessCommand::RetireHandoff { .. } => ("retire-handoff", "{}".to_owned()),
                    HarnessCommand::RecordHandoffAttempt { payload, .. } => {
                        ("handoff-attempt", payload.clone())
                    }
                    HarnessCommand::RecordBackendBound { .. } => {
                        ("record-backend-bound", "{}".to_owned())
                    }
                    HarnessCommand::ConversationRecorder { .. } => {
                        ("conversation-recorder", "{}".to_owned())
                    }
                    HarnessCommand::RewindAndRebind {
                        session_id, head, ..
                    } => (
                        "rewind-and-rebind",
                        serde_json::json!({"session_id": session_id, "head": head}).to_string(),
                    ),
                    HarnessCommand::Undo { .. } => ("undo", "{}".to_owned()),
                    HarnessCommand::UndoSnapshots { .. } => ("undo-snapshots", "{}".to_owned()),
                    HarnessCommand::UndoTo { index, .. } => {
                        ("undo-to", serde_json::json!({"index": index}).to_string())
                    }
                    HarnessCommand::RunUserShell { command, .. } => (
                        "run-user-shell",
                        serde_json::json!({"command": command}).to_string(),
                    ),
                    HarnessCommand::SetApprovalPolicy { policy, .. } => (
                        "set-approval-policy",
                        serde_json::json!({"policy": policy}).to_string(),
                    ),
                    HarnessCommand::Goal { .. } => ("goal", "{}".to_owned()),
                    HarnessCommand::SetGoal {
                        objective, status, ..
                    } => (
                        "set-goal",
                        serde_json::json!({"objective": objective, "status": status}).to_string(),
                    ),
                    HarnessCommand::SetGoalStatus { status, .. } => (
                        "set-goal-status",
                        serde_json::json!({"status": status}).to_string(),
                    ),
                    HarnessCommand::ClearGoal { .. } => ("clear-goal", "{}".to_owned()),
                    HarnessCommand::GetSessionConfig { .. } => {
                        ("get-session-config", "{}".to_owned())
                    }
                    HarnessCommand::SetSessionConfigOption {
                        config_id, value, ..
                    } => (
                        "set-session-config-option",
                        serde_json::json!({"config_id": config_id, "value": value}).to_string(),
                    ),
                    HarnessCommand::ListSessions { cwd, .. } => {
                        ("list-sessions", serde_json::json!({"cwd": cwd}).to_string())
                    }
                    HarnessCommand::CloseSession { .. } => ("close-session", "{}".to_owned()),
                    HarnessCommand::SubscribeEvents { .. } => ("subscribe-events", "{}".to_owned()),
                    HarnessCommand::FlushTranscript { .. } => ("flush-transcript", "{}".to_owned()),
                    HarnessCommand::Skills { .. } => ("skills", "{}".to_owned()),
                    HarnessCommand::Plugins { .. } => ("plugins", "{}".to_owned()),
                    HarnessCommand::RunPlugin {
                        plugin,
                        command,
                        arguments,
                        ..
                    } => (
                        "plugin",
                        serde_json::json!({
                            "plugin": plugin,
                            "command": command,
                            "arguments": arguments,
                        })
                        .to_string(),
                    ),
                };
                if command_recording_enabled.load(std::sync::atomic::Ordering::Acquire)
                    && !matches!(
                        cmd,
                        HarnessCommand::RetireHandoff { .. }
                            | HarnessCommand::RecordHandoffAttempt { .. }
                            | HarnessCommand::CurrentSessionId { .. }
                            | HarnessCommand::Skills { .. }
                            | HarnessCommand::Plugins { .. }
                    )
                    && let Some(recorder) =
                        current_cell(&command_recorder_cell).read().await.clone()
                    && let Err(error) = recorder.record_command(command_name, command_payload).await
                {
                    tracing::error!(%error, "failed to record harness command");
                    continue;
                }
                match cmd {
                    HarnessCommand::CurrentSessionId { response_tx } => {
                        let _ = response_tx.send(Ok(backend_for_agent.session_id().await));
                    }
                    HarnessCommand::ControlledSubagentHandle {
                        session_id,
                        response_tx,
                    } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx
                                .send(Ok(backend.controlled_subagent_handle(&session_id).await));
                        });
                    }
                    HarnessCommand::ChildConfigOptions {
                        session_id,
                        response_tx,
                    } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ =
                                response_tx.send(backend.child_config_options(session_id).await);
                        });
                    }
                    HarnessCommand::SetChildConfigOption {
                        session_id,
                        config_id,
                        value,
                        response_tx,
                    } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx.send(
                                backend
                                    .set_child_config_option(session_id, config_id, value)
                                    .await,
                            );
                        });
                    }
                    HarnessCommand::ExecuteConversationProgram {
                        source,
                        path,
                        arguments,
                        response_tx,
                    } => {
                        let backend = Arc::clone(&backend_for_agent);
                        let recorder = current_cell(&command_recorder_cell);
                        tokio::spawn(async move {
                            let result = program::execute_source(
                                &source, path, &arguments, &backend, &recorder,
                            )
                            .await;
                            let _ = response_tx.send(result);
                        });
                    }
                    HarnessCommand::Prompt {
                        content,
                        meta,
                        admission,
                        response_tx,
                    } => {
                        // The turn boundary is where a session goes back to the route the
                        // user chose, once the policy says it may.
                        if backend_for_agent.is_between_turns().await
                            && !prompt_queue.is_busy().await
                            && let Some(request) = backend_for_agent
                                .restore_primary(crate::backend::RestoreAt::TurnBoundary)
                                .await
                        {
                            handoff::run(
                                handoff::Live {
                                    plugin_actions: &plugin_actions,
                                    backend: &mut backend_for_agent,
                                    prompt_queue: &mut prompt_queue,
                                    requests: &handoff_tx,
                                    events: &handoff_events,
                                    fanout: &event_fanout,
                                    recorder_cell: &command_recorder_cell,
                                },
                                request,
                                &mut agent_cmd_rx,
                                &mut pending_commands,
                            )
                            .await;
                        }
                        prompt_queue
                            .enqueue(content, meta, admission, response_tx)
                            .await;
                    }
                    HarnessCommand::PromoteQueued { response_tx } => {
                        let result = backend_for_agent.promote_next_queued().await;
                        let _ = response_tx.send(result);
                    }
                    HarnessCommand::Shutdown {
                        child_grace,
                        response_tx,
                    } => {
                        prompt_queue.shutdown().await;
                        let result = backend_for_agent.shutdown(child_grace).await;
                        let shutdown_complete = result.is_ok();
                        let _ = response_tx.send(result);
                        if shutdown_complete {
                            break;
                        }
                    }
                    HarnessCommand::RespondToAgent {
                        request_id,
                        response,
                        response_tx,
                    } => {
                        let result = backend_for_agent
                            .respond_to_agent(request_id, response)
                            .await;
                        let _ = response_tx.send(result);
                    }
                    HarnessCommand::Cancel { response_tx } => {
                        prompt_queue.cancel_pending().await;
                        let _ = response_tx.send(backend_for_agent.cancel().await);
                    }
                    HarnessCommand::CancelRequest {
                        request_id,
                        response_tx,
                    } => {
                        let _ =
                            response_tx.send(backend_for_agent.cancel_request(request_id).await);
                    }
                    HarnessCommand::PendingWork { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.pending_work().await);
                    }
                    HarnessCommand::DropQueued {
                        event_id,
                        response_tx,
                    } => {
                        let _ = response_tx.send(backend_for_agent.drop_queued(event_id).await);
                    }
                    HarnessCommand::StopSubagents { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.stop_subagents().await);
                    }
                    HarnessCommand::StopAgent { response_tx } => {
                        prompt_queue.cancel_pending().await;
                        let _ = response_tx.send(backend_for_agent.stop_agent().await);
                    }
                    HarnessCommand::AddHistory { text, response_tx } => {
                        let _ = response_tx.send(backend_for_agent.add_history(text).await);
                    }
                    HarnessCommand::HistoryEntry {
                        log_id,
                        offset,
                        response_tx,
                    } => {
                        let _ =
                            response_tx.send(backend_for_agent.history_entry(log_id, offset).await);
                    }
                    HarnessCommand::SearchHistory {
                        max_results,
                        response_tx,
                    } => {
                        let _ =
                            response_tx.send(backend_for_agent.search_history(max_results).await);
                    }
                    HarnessCommand::CustomPrompts { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.custom_prompts().await);
                    }
                    HarnessCommand::Compact { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.compact().await);
                    }
                    HarnessCommand::Branch { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.branch().await);
                    }
                    HarnessCommand::SendSubagentMessage {
                        session_id,
                        content,
                        response_tx,
                    } => {
                        child_admissions.send(
                            &backend_for_agent,
                            session_id,
                            child_admission::ChildCommand::Send(content, response_tx),
                        );
                    }
                    HarnessCommand::SteerSubagentContent {
                        session_id,
                        content,
                        response_tx,
                    } => {
                        child_admissions.send(
                            &backend_for_agent,
                            session_id,
                            child_admission::ChildCommand::Steer(content, response_tx),
                        );
                    }
                    HarnessCommand::PromoteSubagentQueue {
                        session_id,
                        response_tx,
                    } => {
                        child_admissions.send(
                            &backend_for_agent,
                            session_id,
                            child_admission::ChildCommand::Promote(response_tx),
                        );
                    }
                    HarnessCommand::CancelSubagent {
                        session_id,
                        response_tx,
                    } => {
                        child_admissions.send(
                            &backend_for_agent,
                            session_id,
                            child_admission::ChildCommand::Cancel(response_tx),
                        );
                    }
                    HarnessCommand::ChildHistory {
                        session_id,
                        response_tx,
                    } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx.send(backend.child_history(session_id).await);
                        });
                    }
                    HarnessCommand::ForkChildHistory {
                        session_id,
                        context,
                        source,
                        response_tx,
                    } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx.send(
                                backend
                                    .fork_child_history(session_id, context, source)
                                    .await,
                            );
                        });
                    }
                    HarnessCommand::RewindChildHistory {
                        session_id,
                        source,
                        response_tx,
                    } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx
                                .send(backend.rewind_child_history(session_id, source).await);
                        });
                    }
                    HarnessCommand::RestoreSeededNativeChildren { response_tx } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ =
                                response_tx.send(backend.restore_seeded_native_children().await);
                        });
                    }
                    HarnessCommand::ForkHistory {
                        context,
                        source,
                        response_tx,
                    } => {
                        let _ =
                            response_tx.send(backend_for_agent.fork_history(context, source).await);
                    }
                    HarnessCommand::RecordForkSource {
                        session_id,
                        start,
                        end,
                        context,
                        entries,
                        response_tx,
                    } => {
                        let recorder = current_cell(&command_recorder_cell).read().await.clone();
                        let result = match recorder {
                            Some(recorder) => recorder
                                .record_fork_source(session_id, start, end, context, entries)
                                .await
                                .map_err(anyhow::Error::from),
                            None => Err(anyhow::anyhow!("session has no transcript recorder")),
                        };
                        let _ = response_tx.send(result);
                    }
                    HarnessCommand::RetireHandoff { response_tx } => {
                        command_recording_enabled
                            .store(false, std::sync::atomic::Ordering::Release);
                        let _ = response_tx.send(Ok(()));
                    }
                    HarnessCommand::RecordHandoffAttempt {
                        payload,
                        response_tx,
                    } => {
                        let recorder = current_cell(&command_recorder_cell).read().await.clone();
                        let result = match recorder {
                            Some(recorder) => recorder
                                .record_command("handoff-attempt", payload)
                                .await
                                .map_err(anyhow::Error::from),
                            None => Err(anyhow::anyhow!("session has no transcript recorder")),
                        };
                        let _ = response_tx.send(result);
                    }
                    HarnessCommand::RecordBackendBound { response_tx } => {
                        let recorder = current_cell(&command_recorder_cell).read().await.clone();
                        let result = match recorder {
                            Some(recorder) => {
                                let payload = serde_json::json!({"agent": backend_for_agent.agent_name(), "acp_session_id": backend_for_agent.session_id().await}).to_string();
                                recorder
                                    .record_backend_bound(payload)
                                    .await
                                    .map_err(anyhow::Error::from)
                            }
                            None => Err(anyhow::anyhow!("session has no transcript recorder")),
                        };
                        if result.is_ok() {
                            command_recording_enabled
                                .store(true, std::sync::atomic::Ordering::Release);
                        }
                        let _ = response_tx.send(result);
                    }
                    HarnessCommand::ConversationRecorder { response_tx } => {
                        let _ = response_tx.send(Ok(current_cell(&command_recorder_cell)
                            .read()
                            .await
                            .clone()));
                    }
                    HarnessCommand::RewindAndRebind {
                        session_id,
                        head,
                        response_tx,
                    } => {
                        let result = if prompt_queue.is_busy().await {
                            Err(anyhow::anyhow!(
                                "Cannot rewind while a prompt is being admitted"
                            ))
                        } else {
                            backend_for_agent.rewind_and_rebind(session_id, head).await
                        };
                        let _ = response_tx.send(result);
                    }
                    HarnessCommand::Undo { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.undo().await);
                    }
                    HarnessCommand::UndoSnapshots { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.undo_snapshots().await);
                    }
                    HarnessCommand::UndoTo { index, response_tx } => {
                        let _ = response_tx.send(backend_for_agent.undo_to(index).await);
                    }
                    HarnessCommand::RunUserShell {
                        command,
                        response_tx,
                    } => {
                        let _ = response_tx.send(backend_for_agent.run_user_shell(command).await);
                    }
                    HarnessCommand::SetApprovalPolicy {
                        policy,
                        response_tx,
                    } => {
                        backend_for_agent.set_approval_policy(policy);
                        let _ = response_tx.send(Ok(()));
                    }
                    HarnessCommand::Goal { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.current_goal().await);
                    }
                    HarnessCommand::SetGoal {
                        objective,
                        status,
                        response_tx,
                    } => {
                        let _ =
                            response_tx.send(backend_for_agent.set_goal(objective, status).await);
                    }
                    HarnessCommand::SetGoalStatus {
                        status,
                        response_tx,
                    } => {
                        let _ = response_tx.send(backend_for_agent.set_goal_status(status).await);
                    }
                    HarnessCommand::ClearGoal { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.clear_goal().await);
                    }
                    HarnessCommand::GetSessionConfig { response_tx } => {
                        let state = backend_for_agent.config_options();
                        let _ = response_tx.send(state);
                    }
                    HarnessCommand::SetSessionConfigOption {
                        config_id,
                        value,
                        response_tx,
                    } => {
                        // The agent's answer can take a while; the loop goes on meanwhile.
                        config_lane.submit(
                            Arc::clone(&backend_for_agent),
                            config_id,
                            value,
                            response_tx,
                        );
                    }
                    HarnessCommand::ListSessions { cwd, response_tx } => {
                        // A round trip to the agent that changes nothing: the loop goes on.
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ =
                                response_tx.send(backend.connection().list_sessions(&cwd).await);
                        });
                    }
                    HarnessCommand::CloseSession { response_tx } => {
                        let result = backend_for_agent.close_active_session().await;
                        let close_complete = result.is_ok();
                        let _ = response_tx.send(result);
                        if close_complete {
                            prompt_queue.shutdown().await;
                            break;
                        }
                    }
                    HarnessCommand::SubscribeEvents { response_tx } => {
                        let _ = response_tx.send(event_fanout.subscribe());
                    }
                    HarnessCommand::FlushTranscript { response_tx } => {
                        let _ = response_tx.send(backend_for_agent.flush_transcript().await);
                    }
                    HarnessCommand::Skills { response_tx } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx.send(Ok(backend.skills().await));
                        });
                    }
                    HarnessCommand::Plugins { response_tx } => {
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx.send(backend.plugins().await);
                        });
                    }
                    HarnessCommand::RunPlugin {
                        plugin,
                        command,
                        arguments,
                        response_tx,
                    } => {
                        // A run waits on what its operations wait on: never on the command loop.
                        let backend = Arc::clone(&backend_for_agent);
                        tokio::spawn(async move {
                            let _ = response_tx
                                .send(backend.run_plugin(&plugin, &command, arguments).await);
                        });
                    }
                }
            }
            prompt_queue.shutdown().await;
        });

        // Drop our Arc reference; the typed command task owns the backend.
        drop(backend);

        let mut replaying = false;
        let mut response_boundary: Option<(String, i64, i64)> = None;
        while let Some(BackendEvent::Public(event)) = backend_event_rx.recv().await {
            let session_ended = matches!(event, SessionEvent::Nori(NoriEvent::SessionEnded(_)));
            let in_replay = match &event {
                SessionEvent::Nori(NoriEvent::ReplayStarted(_)) => {
                    replaying = true;
                    true
                }
                SessionEvent::Nori(NoriEvent::ReplayFinished) => std::mem::take(&mut replaying),
                _ => replaying,
            };
            // The turn assembles messages by the same definition: see `nori_protocol::message_flow`.
            let flow = nori_protocol::message_flow(&event);
            let is_assistant_chunk =
                flow == nori_protocol::MessageFlow::Chunk(nori_protocol::MessageStream::Answer);
            let flush_response = match flow {
                nori_protocol::MessageFlow::Boundary => true,
                nori_protocol::MessageFlow::Chunk(stream) => {
                    stream != nori_protocol::MessageStream::Answer
                }
                nori_protocol::MessageFlow::Transparent => false,
            };
            if flush_response && let Some((transcript_id, start, end)) = response_boundary.take() {
                let marker = nori_protocol::ConversationResponseBoundary {
                    transcript_id,
                    start,
                    end,
                };
                if event_tx
                    .send(SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(
                        marker,
                    )))
                    .is_err()
                {
                    break;
                }
            }
            if recording_enabled.load(std::sync::atomic::Ordering::Acquire)
                && let Some(recorder) = current_cell(&recorder_cell).read().await.clone()
                && !(in_replay && recorder.continues_recorded_session())
            {
                match recorder.record_received_event(&event).await {
                    Ok(Some((start, end))) if is_assistant_chunk && !in_replay => {
                        response_boundary = Some((recorder.session_id().to_owned(), start, end));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "failed to record received session event");
                        break;
                    }
                }
            }
            let evidence = observe_context_event(&context_timeline, &event).await;
            // A transcript that already holds the history being replayed does not record it again.
            if recording_enabled.load(std::sync::atomic::Ordering::Acquire)
                && let Some(recorder) = current_cell(&recorder_cell).read().await.clone()
                && !(in_replay && recorder.continues_recorded_session())
            {
                match recorder.record_session_event(&evidence).await {
                    Ok(Some(id)) if is_assistant_chunk && !in_replay => {
                        if let Some((_, _, end)) = &mut response_boundary {
                            *end = id;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "failed to record public session event");
                        break;
                    }
                }
            }
            if event_tx.send(event).is_err() {
                break;
            }
            if session_ended {
                break;
            }
        }
        if let Some(recorder) = current_cell(&recorder_cell).write().await.take() {
            let result = if Arc::strong_count(&recorder) == 1 {
                recorder.shutdown().await
            } else {
                recorder.flush().await
            };
            if let Err(error) = result {
                tracing::warn!(%error, "failed to flush transcript recorder");
            }
        }
    });

    LaunchedSession {
        handle,
        events: event_rx,
    }
}

/// The recorder cell of the backend the session runs on now.
fn current_cell(
    cell: &std::sync::Mutex<
        Arc<tokio::sync::RwLock<Option<Arc<crate::transcript::TranscriptRecorder>>>>,
    >,
) -> Arc<tokio::sync::RwLock<Option<Arc<crate::transcript::TranscriptRecorder>>>> {
    Arc::clone(
        &cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

async fn observe_context_event(
    timeline: &tokio::sync::RwLock<crate::context_timeline::ContextTimelineState>,
    event: &SessionEvent,
) -> crate::transcript::SessionEventEntry {
    let mut evidence = crate::transcript::SessionEventEntry::new(event.clone());
    let mut state = timeline.write().await;
    if let (Some(id), Some(at)) = (&evidence.observation_id, &evidence.observed_at) {
        state.observe(event, id, at);
    }
    evidence.history_incomplete = state.timeline.incomplete_history;
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_protocol::acp::v1::SessionConfigOptionCategory;
    use nori_protocol::acp::v1::SessionConfigSelectOption;
    use pretty_assertions::assert_eq;

    fn mode_option(current_value: &str) -> SessionConfigOption {
        SessionConfigOption::select(
            "mode",
            "Mode",
            current_value.to_string(),
            vec![
                SessionConfigSelectOption::new("plan", "Plan"),
                SessionConfigSelectOption::new("build", "Build"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode)
    }

    #[tokio::test]
    async fn set_session_config_option_returns_refreshed_config_snapshot() {
        let (command_tx, mut command_rx) = unbounded_channel::<HarnessCommand>();
        tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                if let HarnessCommand::SetSessionConfigOption {
                    config_id: _,
                    value,
                    response_tx,
                } = command
                {
                    let _ = response_tx.send(Ok(vec![mode_option(&value)]));
                }
            }
        });
        let handle = HarnessHandle {
            command_tx,
            plugin_actions: Default::default(),
            first_prompt_started: None,
            context_timeline: Arc::default(),
        };

        let config_options = handle
            .set_session_config_option("mode".to_string(), "build".to_string())
            .await
            .unwrap();

        assert_eq!(config_options, vec![mode_option("build")]);
    }

    #[tokio::test]
    async fn first_prompt_callback_fires_once_across_cloned_handles() {
        let (command_tx, mut command_rx) = unbounded_channel::<HarnessCommand>();
        tokio::spawn(async move {
            let mut request_id = 0_i64;
            while let Some(command) = command_rx.recv().await {
                if let HarnessCommand::Prompt { response_tx, .. } = command {
                    request_id += 1;
                    let _ = response_tx.send(Ok(request_id.into()));
                }
            }
        });
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count_for_handle = Arc::clone(&callback_count);
        let handle = HarnessHandle {
            command_tx,
            plugin_actions: Default::default(),
            first_prompt_started: None,
            context_timeline: Arc::default(),
        }
        .with_first_prompt_started_callback(move || {
            callback_count_for_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let cloned_handle = handle.clone();

        handle.prompt(Vec::new()).await.expect("first prompt");
        cloned_handle
            .prompt(Vec::new())
            .await
            .expect("second prompt");

        assert_eq!(callback_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
