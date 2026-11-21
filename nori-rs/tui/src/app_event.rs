use std::path::PathBuf;

use codex_common::approval_presets::ApprovalPreset;
use codex_file_search::FileMatch;
use nori_protocol::acp::v1::SessionConfigOption;

use crate::bottom_pane::ApprovalRequest;
use crate::history_cell::HistoryCell;
use crate::nori::session_config_mode::AcpModeConfig;
use crate::system_info::SystemInfo;

use nori_config::AskForApproval;
use nori_config::SandboxPolicy;

pub(crate) type SessionGeneration = i64;

#[derive(Debug)]
pub(crate) enum HarnessAction {
    Cancel,
    /// Stop the subagents the agent started; cancelling a turn never does.
    StopSubagents,
    /// Tear the agent's connection down: it ignored a cancel. The session stays resumable.
    StopAgent,
    Shutdown {
        child_grace: std::time::Duration,
    },
    Compact,
    Branch,
    ForkHistory {
        context: String,
        source: Option<(String, i64, i64)>,
    },
    SendSubagentMessage {
        key: crate::subagent_switcher::SubagentKey,
        session_id: nori_protocol::acp::v1::SessionId,
        text: String,
        image_paths: Vec<PathBuf>,
        steer: bool,
    },
    UndoTo(i64),
    LoadUndoSnapshots,
    RunUserShell(String),
    AddHistory(String),
    HistoryEntry {
        log_id: i64,
        offset: i64,
    },
    SearchHistory {
        max_results: i64,
    },
    LoadCustomPrompts,
    LoadGoal,
    SetGoal {
        objective: String,
        status: Option<nori_protocol::ThreadGoalStatus>,
    },
    SetGoalStatus(nori_protocol::ThreadGoalStatus),
    ClearGoal,
    RespondToAgent {
        request_id: nori_protocol::acp::v1::RequestId,
        response:
            Box<Result<nori_protocol::acp::v1::ClientResponse, nori_protocol::acp::v1::Error>>,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, strum_macros::IntoStaticStr)]
pub(crate) enum AppEvent {
    ContextTimelineReady {
        generation: SessionGeneration,
        conversation_id: Option<nori_harness::ConversationId>,
        result: Result<crate::chatwidget::ContextSnapshot, String>,
    },

    SessionEvent {
        generation: SessionGeneration,
        event: nori_protocol::SessionEvent,
    },

    /// Start a new session.
    NewSession,

    /// A `/close` failed: surface the error and unblock the widget.
    SessionCloseFailed {
        message: String,
    },

    /// A `/close` succeeded: the session is released. Return to the agent
    /// session picker instead of auto-claiming a fresh session.
    SessionClosed,

    /// Show a brief message in the top right of the screen.
    ShowToast(String),

    /// Have an agent commit the working tree's changes. `automatic` is a turn that just ended
    /// with `[git] auto_commit` on, as opposed to the hotkey.
    CommitChanges {
        automatic: bool,
    },

    /// What changed in the working tree, ready to become a commit request.
    CommitChangesReady {
        automatic: bool,
        changes: Vec<nori_config::ChangedFile>,
    },

    /// Manage the app-owned remote ACP listener lifecycle.
    RemoteControlRequested(crate::remote_control::RemoteControlRequest),

    /// The user accepted the one-shot dangerous-exposure warning for an
    /// explicit non-loopback address.
    ConfirmRemoteControlExplicit(std::net::SocketAddr),

    /// An agent connection finished initialization and optional session
    /// listing. The successful value still owns that exact live connection.
    AgentPrepared {
        generation: SessionGeneration,
        agent: anyhow::Result<nori_harness::runtime::PreparedAgent>,
        intent: AgentPrepareIntent,
    },

    /// Activate a local transcript resume after the startup connection has
    /// completed preparation.
    ActivatePreparedResume {
        acp_session_id: Option<String>,
        title: Option<String>,
        transcript: Option<nori_harness::transcript::Transcript>,
    },

    /// Re-run pre-session preparation and reopen the session picker (e.g.
    /// /resume on a deferred widget that has no live agent connection).
    OpenAgentSessionPicker,

    /// Begin the quit flow (feedback, input freeze, bounded cleanup) on the chat
    /// widget — used by input surfaces that don't own the widget (Ctrl+D).
    BeginExit,

    /// Request to exit the application gracefully.
    ExitRequest,

    HarnessAction(HarnessAction),
    HistoryEntryLoaded {
        log_id: i64,
        offset: i64,
        entry: Option<nori_harness::HistoryEntry>,
    },
    HistorySearchLoaded(Vec<nori_harness::HistoryEntry>),
    CustomPromptsLoaded(Vec<nori_harness::CustomPrompt>),
    UndoSnapshotsLoaded(Vec<nori_harness::UndoSnapshot>),
    GoalLoaded(Option<nori_protocol::ThreadGoal>),
    HarnessActionFailed(String),
    /// The prompt never reached the harness (encoding an attachment failed or
    /// the harness is gone). The prompt was already painted optimistically.
    PromptSubmitFailed(String),

    /// Kick off an asynchronous file search for the given query (text after
    /// the `@`). Previous searches may be cancelled by the app layer so there
    /// is at most one in-flight search.
    StartFileSearch(String),

    /// Result of a completed asynchronous file search. The `query` echoes the
    /// original search term so the UI can decide whether the results are
    /// still relevant.
    FileSearchResult {
        query: String,
        matches: Vec<FileMatch>,
    },

    /// Result of background system info collection for the footer.
    SystemInfoRefreshed(SystemInfo),

    /// Request to refresh system info for a specific directory.
    /// This is triggered when the effective CWD changes during agent operations.
    ///
    /// The optional agent name is used to determine which agent's transcripts to search for.
    RefreshSystemInfoForDirectory {
        /// The directory to collect system info for
        dir: PathBuf,
        /// Optional agent name (e.g., "claude-code", "gemini") to determine agent kind
        agent: Option<String>,
    },

    /// What `/diff` found.
    DiffResult(crate::get_git_diff::GitChanges),

    /// A click on one of Nori's own links (`nori://`), routed back by the terminal.
    OpenNoriLink(String),

    /// The agent died under the session: restart it and continue the conversation.
    RecoverConversation,

    /// `/move`: this session to the machine named.
    MoveSession {
        machine: String,
    },
    /// How a move went.
    MoveFinished(Result<crate::app::session_move::Moved, String>),

    InsertHistoryCell(Box<dyn HistoryCell>),
    RevealUserQuestion,
    UpdateHistoryToolSnapshot {
        generation: SessionGeneration,
        snapshot: Box<crate::presentation::ToolSnapshot>,
    },
    FinalizeHistoryTools {
        generation: SessionGeneration,
        call_ids: Vec<String>,
    },
    /// Associate a completed rendered answer with its canonical log records.
    ResponseBoundary(nori_protocol::ConversationResponseBoundary),
    ResponseRewindFinished {
        generation: SessionGeneration,
        cell: std::sync::Arc<dyn HistoryCell>,
        result: Result<(), String>,
    },
    /// The edit journal answered: what restoring the files of the focused prompt would undo.
    FocusSnapshotsLoaded {
        generation: SessionGeneration,
        snapshots: Vec<nori_harness::UndoSnapshot>,
    },
    /// The composer walked back to a prompt of the main conversation (↑ at its top), or left the
    /// walk (`None`). `cell` is the prompt's index in the transcript when the walk stepped.
    FocusChanged {
        generation: SessionGeneration,
        cell: Option<usize>,
    },
    /// The clipboard's text, read for the composer's `space p` / `space P` / `space R`.
    EditorClipboard {
        command: nori_editor::ClipboardCommand,
        text: String,
    },
    /// Enter on a walked-back prompt: rewind the conversation to before it and send `text`.
    RewindSelection(RewindSelection),
    /// The files of a rewind are restored (or could not be): the conversation forks next.
    RewindFilesRestored {
        generation: SessionGeneration,
        cell_index: usize,
        after: ForkAfter,
        result: Result<(), String>,
    },
    ChildHistoryReady {
        request_id: i64,
        key: crate::subagent_switcher::SubagentKey,
        focus_responses: bool,
        result: Result<Vec<nori_protocol::SessionEvent>, String>,
    },
    ChildHistoryActionFinished {
        key: crate::subagent_switcher::SubagentKey,
        rewound: bool,
        result: Result<Vec<nori_protocol::SessionEvent>, String>,
    },

    /// The session's history starts or stops being replayed into the scrollback.
    HistoryReplay(HistoryReplay),

    /// Replace the just-finished assistant stream cells with one raw-Markdown cell.
    ConsolidateAgentMessage {
        source: String,
        cwd: PathBuf,
    },

    /// Open the confirmation prompt before enabling full access mode.
    OpenFullAccessConfirmation {
        preset: ApprovalPreset,
    },

    /// Open the Windows world-writable directories warning.
    /// If `preset` is `Some`, the confirmation will apply the provided
    /// approval/sandbox configuration on Continue; if `None`, it performs no
    /// policy change and only acknowledges/dismisses the warning.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    OpenWorldWritableWarningConfirmation {
        preset: Option<ApprovalPreset>,
        /// Up to 3 sample world-writable directories to display in the warning.
        sample_paths: Vec<String>,
        /// If there are more than `sample_paths`, this carries the remaining count.
        extra_count: usize,
        /// True when the scan failed (e.g. ACL query error) and protections could not be verified.
        failed_scan: bool,
    },

    /// Prompt to enable the Windows sandbox feature before using Agent mode.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    OpenWindowsSandboxEnablePrompt {
        preset: ApprovalPreset,
    },

    /// Enable the Windows sandbox feature and switch to Agent mode.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    EnableWindowsSandboxForAgentMode {
        preset: ApprovalPreset,
    },

    /// Apply an approval preset atomically across app state, widget state,
    /// and the backend's turn context.
    ApplyApprovalPreset {
        approval: AskForApproval,
        sandbox: SandboxPolicy,
    },

    /// Update whether the full access warning prompt has been acknowledged.
    UpdateFullAccessWarningAcknowledged(bool),

    /// Update whether the world-writable directories warning has been acknowledged.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    UpdateWorldWritableWarningAcknowledged(bool),

    /// Persist the acknowledgement flag for the full access warning prompt.
    PersistFullAccessWarningAcknowledged,

    /// Persist the acknowledgement flag for the world-writable directories warning.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    PersistWorldWritableWarningAcknowledged,

    /// Skip the next world-writable scan (one-shot) after a user-confirmed continue.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    SkipNextWorldWritableScan,

    /// Re-open the approval presets popup.
    OpenApprovalsPopup,

    /// Open the approval popup.
    FullScreenApprovalRequest(ApprovalRequest),

    /// Start the existing sign-in flow for a selected provider.
    LoginProvider {
        agent_name: String,
    },

    /// Select a model while retaining alternative routes for that same model.
    SelectModelRoutes {
        routes: Vec<crate::nori::unified_model_picker::ModelRoute>,
        value_name: String,
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
    },

    /// Abandon the prepared candidate while keeping the active session.
    CancelAgentCandidate,

    /// Agent failed to spawn (ACP or HTTP backend). Show error and prompt user
    /// to select a different agent.
    AgentSpawnFailed {
        /// The agent name of the agent that failed to spawn
        agent_name: String,
        /// The error message describing the failure
        error: String,
    },

    /// Agent is connecting (spawning subprocess). Show "Connecting to [Agent]" status.
    /// Sent before AcpBackend::spawn() and cleared when SessionConfigured is received.
    AgentConnecting {
        /// The display name of the agent being connected to
        display_name: String,
    },

    /// Open the model picker placeholder shown when the active agent does not
    /// expose a Model session config option.
    OpenAcpModelPickerUnsupported,

    /// Open the generic ACP session config picker. `focus_config_id` optionally
    /// names the option whose row should be selected when the panel opens (used
    /// to return the cursor to the just-edited option).
    OpenAcpSessionConfigPicker {
        config_options: Vec<SessionConfigOption>,
        focus_config_id: Option<String>,
    },

    /// Open the value picker for a specific ACP session config option.
    OpenAcpSessionConfigValuePicker {
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
        option: SessionConfigOption,
    },

    /// Open the custom model text input so the user can type any model ID.
    OpenCustomModelInput {
        config_id: String,
        option_name: String,
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
    },

    /// Step to the next or previous model the agent offers.
    CycleModel(crate::chatwidget::ModelStep),

    /// Step to the next or previous effort level the agent offers (Ctrl+Shift+N / Ctrl+Shift+P
    /// by default).
    CycleEffort(crate::chatwidget::ModelStep),

    /// Step to the next or previous model mode the agent offers (Alt+. / Alt+, by default).
    CycleModelMode(crate::chatwidget::ModelStep),

    /// The independent control plane for a focused subagent, when it has one.
    SubagentControlsReady {
        key: crate::subagent_switcher::SubagentKey,
        revision: i64,
        handle: Option<nori_harness::runtime::HarnessHandle>,
        config_options: Vec<SessionConfigOption>,
        error: Option<String>,
    },

    SubagentConfigSetResult {
        key: crate::subagent_switcher::SubagentKey,
        result: Result<Vec<SessionConfigOption>, String>,
        config_id: String,
        value: String,
        route_fallback_eligible: bool,
        route_token: Option<i64>,
    },

    /// Set an ACP session config option value.
    SetAcpSessionConfigOption {
        config_id: String,
        value: String,
        option_name: String,
        value_name: String,
        /// True when the value came from the free-text "custom model" input
        /// rather than an advertised picker choice. A custom model the agent
        /// rejects can still be run by persisting it and restarting with the
        /// model injected at spawn.
        is_custom_model: bool,
        route_token: Option<i64>,
        /// Whether the `/config` panel reopens once the value is set.
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
    },

    /// Result of setting an ACP session config option.
    AcpSessionConfigSetResult {
        success: bool,
        /// Agent slug active when the option was set; selections are persisted
        /// under this key even if the user switches agents mid-flight.
        agent: String,
        config_id: String,
        value: String,
        option_name: String,
        /// Whether the value came from the free-text custom model input.
        is_custom_model: bool,
        /// Where the choice came from.
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
        config_options: Option<Vec<SessionConfigOption>>,
        error: Option<String>,
        route_fallback_eligible: bool,
        route_token: Option<i64>,
    },

    /// Latest raw ACP session config for silently seeding local UI state.
    AcpSessionConfigSnapshot {
        generation: i64,
        config_options: Vec<SessionConfigOption>,
    },

    /// Latest derived ACP mode config for the active session.
    AcpModeConfigSnapshot {
        generation: i64,
        mode: Option<AcpModeConfig>,
    },

    /// A provider login began for the root or focused child session.
    LoginStarted {
        target: Option<crate::subagent_switcher::SubagentKey>,
    },
    /// A provider login was cancelled before its completion event.
    LoginCancelled,

    /// Result of OAuth login flow completion.
    LoginComplete {
        /// Whether the login was successful
        success: bool,
    },

    /// How an agent offers to sign in, or why it could not be asked.
    SignInMethods {
        target: crate::chatwidget::SignInTarget,
        methods: Result<Vec<nori_acp_host::sign_in::SignInMethod>, String>,
    },
    /// Sign in with one of the ways an agent offered.
    SignInWith {
        target: crate::chatwidget::SignInTarget,
        method: nori_acp_host::sign_in::SignInMethod,
    },
    /// Whether a sign-in took: a fresh agent could open a session, or why not.
    SignInVerified {
        target: crate::chatwidget::SignInTarget,
        result: Result<(), String>,
    },
    /// Create the account `name` of `provider`, then sign it in.
    AddAccount {
        provider: &'static nori_config::ProviderAccounts,
        name: String,
    },
    /// A check of whether an account's own login is signed in has landed (its agent's slug); the
    /// answer is in `nori_acp_host::accounts::status`.
    AccountChecked {
        slug: String,
    },
    /// An account was created (its agent's slug), or why not.
    AccountAdded {
        provider: &'static nori_config::ProviderAccounts,
        name: String,
        result: Result<String, String>,
    },

    /// Output from external CLI login process (e.g., gemini login)
    ExternalCliLoginOutput {
        /// The bytes the CLI wrote to its terminal, escape sequences and all.
        data: Vec<u8>,
    },

    /// External CLI login process completed
    ExternalCliLoginComplete {
        /// Whether the process exited successfully (exit code 0)
        success: bool,
        /// The agent name for display purposes
        agent_name: String,
    },

    /// Set the TUI vertical footer config setting.
    SetConfigVerticalFooter(bool),

    /// Set the TUI terminal notifications config setting.
    SetConfigTerminalNotifications(bool),

    /// Set a hotkey binding for a specific action.
    SetConfigHotkey {
        action: nori_config::HotkeyAction,
        binding: nori_config::HotkeyBinding,
    },

    /// Set the TUI OS notifications config setting.
    SetConfigOsNotifications(bool),

    /// Open the vim mode sub-picker.
    OpenVimModePicker,

    /// Set the TUI vim mode config setting. `from_settings` is true when the
    /// change originated from the `/settings` panel (so the panel should be
    /// reopened afterward) and false when it came from the standalone `/vim`
    /// command.
    SetConfigVimMode {
        value: nori_config::VimEnterBehavior,
        from_settings: bool,
    },

    /// Open the notify-after-idle sub-picker.
    OpenNotifyAfterIdlePicker,

    /// Open the script timeout sub-picker.
    OpenScriptTimeoutPicker,

    /// Open the hotkey picker sub-view.
    OpenHotkeyPicker,

    /// Set the TUI notify-after-idle config setting.
    SetConfigNotifyAfterIdle(nori_config::NotifyAfterIdle),

    /// Set the TUI script timeout config setting.
    SetConfigScriptTimeout(nori_config::ScriptTimeout),

    /// Open the loop count sub-picker.
    OpenLoopCountPicker,

    /// Set the loop count config setting. `None` means disabled.
    SetConfigLoopCount(Option<i32>),

    /// Open the auto worktree sub-picker.
    OpenAutoWorktreePicker,

    /// Set the TUI auto worktree config setting.
    SetConfigAutoWorktree(nori_config::AutoWorktree),

    /// Set the TUI skillset per session config setting.
    SetConfigSkillsetPerSession(bool),

    /// Set the TUI pinned plan drawer config setting.
    SetConfigPinnedPlanDrawer(bool),

    /// Set the TUI custom working messages config setting.
    SetConfigCustomWorkingMessages(bool),

    /// Open the worktree choice modal when enabling per-session skillsets.
    OpenSkillsetPerSessionWorktreeChoice,

    /// Open the footer segments sub-picker.
    OpenFooterSegmentsPicker,

    /// Toggle a footer segment's enabled state.
    SetConfigFooterSegment(nori_config::FooterSegment, bool),

    /// Start the next loop iteration with a fresh conversation.
    /// Sent by ChatWidget::on_task_complete when loop mode is active.
    LoopIteration {
        /// The prompt text to replay.
        prompt: String,
        /// Remaining iterations after this one.
        remaining: i32,
        /// Total iterations configured.
        total: i32,
    },

    /// Result of listing available skillsets via nori-skillsets CLI.
    SkillsetListResult {
        /// List of skillset names on success (exit code 0), None if command not found.
        names: Option<Vec<String>>,
        /// Error message if command failed (non-zero exit) or not found.
        error: Option<String>,
        /// When in a worktree, the directory to install skillsets into.
        install_dir: Option<PathBuf>,
    },

    /// Request to install a skillset by name.
    InstallSkillset {
        /// The name of the skillset to install.
        name: String,
    },

    /// Request to switch to a skillset in a specific directory.
    SwitchSkillset {
        /// The name of the skillset to switch to.
        name: String,
        /// The directory to install the skillset into.
        install_dir: PathBuf,
    },

    /// Result of installing a skillset.
    SkillsetInstallResult {
        /// The name of the skillset that was installed.
        name: String,
        /// Whether the installation succeeded (exit code 0).
        success: bool,
        /// Filtered install output on success, or error message on failure.
        message: String,
    },

    /// Result of switching a skillset.
    SkillsetSwitchResult {
        /// The name of the skillset that was switched to.
        name: String,
        /// Whether the switch succeeded.
        success: bool,
        /// Filtered output on success, or error message on failure.
        message: String,
    },

    /// The skillset picker was dismissed without selection. When agent spawn was
    /// deferred for skillset_per_session, this triggers spawning the agent
    /// without a skillset (behaves as if the feature is disabled).
    SkillsetPickerDismissed,

    /// Execute a custom prompt script asynchronously.
    ExecuteScript {
        /// The custom prompt to execute.
        prompt: nori_harness::custom_prompts::CustomPrompt,
        /// Positional arguments from the command line.
        args: Vec<String>,
    },

    /// Result of executing a custom prompt script.
    ScriptExecutionComplete {
        /// Name of the script that was executed.
        name: String,
        /// Ok(stdout) on success, Err(message) on failure.
        result: Result<String, String>,
    },

    /// Show searchable past conversations for insertion into the draft.
    ShowReferenceSessionPicker {
        sessions: Vec<crate::resume_picker::SessionPickerInfo>,
        nori_home: PathBuf,
        target: crate::subagent_switcher::Target,
    },

    /// Load the selected source conversation before inserting its reference.
    LoadSessionReference {
        project_id: String,
        session_id: String,
        agent: String,
        target: crate::subagent_switcher::Target,
    },

    /// Insert a selected conversation reference at the draft cursor.
    InsertSessionReference {
        text: String,
        target: crate::subagent_switcher::Target,
    },

    /// Show the resume picker: one list of the recorded sessions and the sessions the agent
    /// lists that Nori has no record of. The picker fills its rows and searches full text from
    /// the on-disk index by itself.
    ShowResumeSessionPicker(Box<crate::resume_picker::helix_overlay::ResumeSetup>),
    /// A candidate agent's model, effort and binding were put in place (or were not): take it.
    CandidateCommitted {
        generation: SessionGeneration,
        started: Box<nori_protocol::SessionStarted>,
        outcome: crate::app::candidate_commit::CommitOutcome,
    },
    /// The agent's own sessions, listed after the resume picker opened without them.
    ResumePickerAgentSessions(Vec<nori_protocol::acp::v1::SessionInfo>),

    /// Resume a previous session via ACP session/load or client-side replay.
    ResumeSession {
        /// The NORI_HOME path
        nori_home: PathBuf,
        /// Project identifier (needed to load transcript)
        project_id: String,
        /// Session identifier to resume
        session_id: String,
    },

    /// The transcript of a session being resumed finished loading.
    ResumeTranscriptLoaded {
        transcript: Result<Box<nori_harness::transcript::Transcript>, String>,
    },

    /// Resume a session reported by the agent's `session/list`, via
    /// `session/load` with no local transcript (the agent replays history).
    ResumeAcpSession {
        /// The agent's session identifier to load.
        acp_session_id: String,
        /// The broker-reported session title, when known, so the reattach
        /// message and cloud surfaces can name the session.
        title: Option<String>,
    },

    /// Launch a terminal file manager to browse and optionally edit files.
    BrowseFiles(nori_config::FileManager),

    /// Set the configured file manager for the `/browse` command.
    SetConfigFileManager(nori_config::FileManager),

    /// Open the file manager sub-picker.
    OpenFileManagerPicker,

    /// Persist the full MCP servers map to config.toml.
    SaveMcpServers(std::collections::BTreeMap<String, nori_config::McpServerConfig>),

    /// Trigger an MCP OAuth login flow for a server.
    McpOAuthLogin {
        server_name: String,
        server_url: String,
        http_headers: Option<std::collections::HashMap<String, String>>,
        env_http_headers: Option<std::collections::HashMap<String, String>>,
        /// Pre-registered OAuth client ID (for servers without dynamic registration).
        client_id: Option<String>,
        /// Environment variable name holding the OAuth client secret.
        client_secret_env_var: Option<String>,
    },

    /// Cancel an in-progress MCP OAuth login flow.
    McpOAuthLoginCancel {
        server_name: String,
    },

    /// MCP OAuth login flow completed (success or failure).
    McpOAuthLoginComplete {
        server_name: String,
        success: bool,
        error: Option<String>,
    },

    /// Request async computation of MCP server auth statuses.
    ComputeMcpAuthStatuses,

    /// Deliver computed MCP auth statuses to the active picker view.
    McpAuthStatusesReady(std::collections::HashMap<String, codex_rmcp_client::McpAuthStatus>),

    /// Browser launched successfully with CDP details.
    BrowserLaunched {
        ws_url: String,
        cdp_port: i32,
    },

    /// Browser launch failed with an error message.
    BrowserLaunchFailed(String),

    /// Persist the chosen `/browser` profile as the default and launch with it.
    SetBrowserProfile(nori_config::BrowserProfileMode),

    /// Open the fork picker modal showing previous user messages.
    OpenForkPicker,

    // usage: open the `/usage` overlay.
    OpenUsage,

    /// Show a list of choices described as data.
    ShowChoicePicker(Box<crate::choice_picker::ChoiceSpec>),

    /// Branch the conversation at its current head via ACP `session/fork`,
    /// swapping the active session to the forked one.
    BranchFromCurrent,

    /// Ctrl+F on a prompt walked back to: continue the conversation through that turn's response
    /// in a subagent of its own.
    ForkWalkedTurn {
        generation: SessionGeneration,
        /// The walked prompt's index in the transcript.
        cell_index: usize,
    },

    /// Continue a selected history prefix in a separate child session.
    ForkResponseSubagent {
        context: String,
        source: Option<(String, i64, i64)>,
    },
    SendSubagentMessage {
        key: crate::subagent_switcher::SubagentKey,
        text: String,
        image_paths: Vec<PathBuf>,
        steer: bool,
    },
    SubagentMessageFailed {
        key: crate::subagent_switcher::SubagentKey,
        text: String,
        image_paths: Vec<PathBuf>,
        error: String,
    },
    /// Show a transient notice without adding it to the transcript.
    TransientNotice(String),
    ChildSessionOutput {
        key: crate::subagent_switcher::SubagentKey,
        text: String,
    },

    /// Fork the conversation to just before the selected user message.
    ForkToMessage {
        /// Index of the target user message cell in `transcript_cells`.
        cell_index: usize,
        /// The text of the selected message, to prefill the composer.
        prefill: String,
    },
    /// Open the panel of the conversation's actors (its subagents).
    OpenActorPanel,
    /// Tab in the shell-command composer: complete `text` at byte `cursor`.
    ShellComplete {
        text: String,
        cursor: usize,
    },
    /// The shell's answer to a [`AppEvent::ShellComplete`] for `text`.
    ShellCompleted {
        text: String,
        candidates: Vec<nori_bash::CompletionCandidate>,
    },
}

/// A prompt of the main conversation the composer walked back to, sent again from there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RewindSelection {
    pub(crate) generation: SessionGeneration,
    /// The prompt's index in the transcript when Enter was pressed.
    pub(crate) cell_index: usize,
    /// Which prompt of the conversation it is, from 1: a check on `cell_index`.
    pub(crate) turn: usize,
    /// The prompt as edited in the composer, trimmed.
    pub(crate) text: String,
    pub(crate) files: RewindFiles,
}

/// What a rewind does with the files the rewound turns changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RewindFiles {
    /// Restore them from the edit journal (Enter).
    Restore,
    /// Leave them as they are (Alt+Enter).
    Keep,
}


/// What becomes of the prompt a conversation was forked before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ForkAfter {
    /// It goes back into the composer, to be edited.
    Prefill(String),
    /// It is sent at once, as it was edited in place.
    Submit(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentPrepareIntent {
    /// Keep the initialized connection idle until input or a session command
    /// chooses how to activate it.
    Idle,
    Picker {
        fallback_to_spawn: bool,
    },
    /// Open the agent catalog when supported, otherwise use local transcripts.
    ResumePicker,
    Onboarding,
    Candidate {
        agent_name: String,
        display_name: String,
    },
}

/// The bounds of a replayed history among the cells inserted into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryReplay {
    Started(nori_protocol::ReplaySource),
    Finished,
}
