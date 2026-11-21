use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ApprovalRequest;
use crate::chatwidget::ChatWidget;
use crate::client_tool_cell;
use crate::diff_render::DiffSummary;
use crate::file_search::FileSearchManager;
use crate::history_cell::HistoryCell;
use crate::pager_overlay::Overlay;
use crate::render::renderable::Renderable;
use crate::resume_picker::ResumeSelection;
use crate::tui;
use crate::tui::TuiEvent;
use crate::ui_types::TokenUsage;
use crate::update_action::UpdateAction;
use codex_ansi_escape::ansi_escape_line;
use codex_login::AuthManager;
use color_eyre::eyre::Result;
use nori_config::AskForApproval;
use nori_config::NoriConfig;
use nori_config::NoriConfigEdits as ConfigEditsBuilder;
use nori_config::SandboxPolicy;
use nori_harness::ConversationId;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use tokio::select;
use tokio::sync::mpsc::unbounded_channel;

pub(crate) mod child_navigation;
mod program_status;

#[cfg(not(debug_assertions))]
use crate::history_cell::UpdateAvailableHistoryCell;

pub const RESUME_HINT_LEAD: &str = "To continue this session, run:";

#[derive(Debug, Clone)]
pub struct AppExitInfo {
    pub immediate_exit: bool,
    pub token_usage: TokenUsage,
    pub conversation_id: Option<ConversationId>,
    pub conversation_has_activity: bool,
    pub update_action: Option<UpdateAction>,
    /// A moved session continues in this command, run in this terminal after Nori exits.
    pub handover: Option<Vec<String>>,
}

fn session_summary(
    token_usage: TokenUsage,
    conversation_id: Option<ConversationId>,
    conversation_has_activity: bool,
) -> Option<SessionSummary> {
    let usage_line = (!token_usage.is_zero()).then(|| token_usage.to_string());
    let resume_command = conversation_id
        .filter(|_| conversation_has_activity)
        .map(|conversation_id| resume_command_for_conversation(&conversation_id));

    if usage_line.is_none() && resume_command.is_none() {
        return None;
    }

    Some(SessionSummary {
        usage_line,
        resume_command,
    })
}

pub fn resume_command_for_conversation(conversation_id: &ConversationId) -> String {
    format!("nori resume {conversation_id}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSummary {
    usage_line: Option<String>,
    resume_command: Option<String>,
}

pub(crate) struct App {
    pub(crate) app_event_tx: AppEventSender,
    pub(crate) chat_widget: ChatWidget,
    pub(crate) auth_manager: Arc<AuthManager>,
    remote_control: crate::remote_control::RemoteControlManager,
    program_status: program_status::ProgramStatus,

    /// Config is stored here so we can recreate ChatWidgets as needed.
    pub(crate) config: NoriConfig,
    pub(crate) vertical_footer: bool,
    pub(crate) cloud_mode: bool,
    pub(crate) analytics: Option<nori_installed::AnalyticsReporter>,
    pub(crate) footer_layout_config: nori_config::FooterLayoutConfig,

    pub(crate) file_search: FileSearchManager,

    pub(crate) transcript_cells: Vec<Arc<dyn HistoryCell>>,
    /// Canonical journal identity, independent of the provider session binding.
    pub(crate) active_transcript_id: Option<String>,
    pub(crate) response_boundaries: Vec<(
        Arc<dyn HistoryCell>,
        nori_protocol::ConversationResponseBoundary,
    )>,
    /// Number of transcript cells present when Ctrl+L last hid the transcript. A resize may
    /// preserve that hidden view only while the transcript is unchanged.
    pub(crate) hidden_transcript_len: Option<i64>,

    // Pager overlay state (Transcript or Static like Diff)
    pub(crate) overlay: Option<Overlay>,
    /// The subagent picker survives a close so reopening keeps its query and selection.
    /// It is scoped to the canonical transcript and discarded for a new root session.
    subagent_switcher_state: Option<Box<crate::subagent_switcher::SubagentOverlay>>,
    subagent_switcher_transcript: Option<String>,
    pub(crate) session_view: session_view::SessionView,
    tool_gesture: tool_gesture::ToolGesture,
    /// Where the cells of the history being replayed start in `transcript_cells`, while it is.
    replay_painted_from: Option<(usize, nori_protocol::ReplaySource)>,

    pub(crate) enhanced_keys_supported: bool,

    /// The prompt a rewind is focused on, if any.
    pub(crate) focus: Option<focus::Focus>,
    /// A child's journal was asked for so that its prompts can be focused; keys typed meanwhile
    /// wait here.
    pub(crate) pending_child_focus: Option<child_navigation::PendingChildFocus>,
    pub(crate) child_focus_sequence: i64,
    /// Set when the user confirms an update; propagated on exit.
    pub(crate) pending_update_action: Option<UpdateAction>,
    /// A `/move` that waits for the running turn to end.
    pending_move: Option<String>,
    /// A move whose bundle is being shipped.
    move_in_progress: bool,
    /// When the agent last died under the session and was restarted: a loop of deaths stops.
    recoveries: Vec<std::time::Instant>,
    /// What this terminal runs once Nori exits, to continue a moved session.
    pending_handover: Option<Vec<String>>,

    // One-shot suppression of the next world-writable scan after user confirmation.
    skip_world_writable_scan_once: bool,

    /// Prepared startup/post-close connection waiting for an explicit session choice.
    prepared_agent: Option<nori_harness::runtime::PreparedAgent>,
    /// Fork/backtrack context captured with the prepared connection and reused
    /// when refreshing session-time configuration before activation.
    prepared_agent_initial_context: Option<String>,

    /// Owned startup/post-close preparation. Its generation prevents a late
    /// result from replacing a newer session decision.
    primary_agent_preparation: Option<PrimaryAgentPreparation>,

    /// A session directive chosen while the startup connection is still being
    /// prepared. The directive consumes that exact connection when ready.
    pub(crate) pending_session_activation: Option<PendingSessionActivation>,

    /// Live agent-switch candidate. The active widget remains usable until an
    /// activating candidate publishes `SessionStarted`.
    candidate_agent: Option<CandidateAgent>,
    pending_candidate_model: Option<(String, String)>,
    model_route_fallback: Option<model_routes::ModelRouteFallback>,
    next_model_route_token: i64,
    /// The low-disk worktree notice is shown once per run, not on every system info refresh.
    worktree_warning_shown: bool,
    manual_model_handoff: Option<session_setup::ManualModelHandoff>,
    /// Model presses made before the agent listed its models; they apply when it does.
    waiting_model_presses: crate::chatwidget::WaitingModelPresses,
    /// Whether the welcome card for the current conversation has already been
    /// written, ahead of the session that conversation will run on.
    welcome_card_written: bool,
    /// A conversation's header card has been written in this run. Later conversations (a switch
    /// of agent, `/new`) do not write it again: the footer names the model and mode, and the
    /// line announcing the conversation names the agent.
    session_card_shown: bool,

    /// Ephemeral per-session loop count override (set via /settings menu).
    /// Outer Option: whether overridden; inner Option<i32>: the value.
    loop_count_override: Option<Option<i32>>,

    /// Configurable hotkey bindings loaded from NoriConfig.
    pub(crate) hotkey_config: nori_config::HotkeyConfig,

    /// Vim mode and Enter key behavior loaded from NoriConfig.
    vim_mode: nori_config::VimEnterBehavior,

    /// Current footer segment visibility loaded from NoriConfig.
    footer_segment_config: nori_config::FooterSegmentConfig,

    /// Plan drawer visibility mode.
    plan_drawer_mode: crate::chatwidget::PlanDrawerMode,

    /// A commit request is running, so the turn that ends next is its own and must not trigger
    /// another one.
    commit_turn_pending: bool,

    system_info_tx: mpsc::Sender<SystemInfoRefreshRequest>,

    /// True when the initial agent spawn was deferred (waiting for a skillset
    /// switch). Cleared on the first successful skillset switch or picker
    /// dismissal. Guards against re-spawning the agent on later switches.
    deferred_spawn_pending: bool,
    /// Cancel sender for an in-progress MCP OAuth login flow.
    mcp_oauth_cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

struct PrimaryAgentPreparation {
    generation: crate::app_event::SessionGeneration,
    abort: tokio::task::AbortHandle,
    intent: crate::app_event::AgentPrepareIntent,
    initial_context: Option<String>,
}

pub(crate) enum PendingSessionActivation {
    New,
    /// A resume chosen at startup whose transcript is still loading. The agent starts in the
    /// meantime, and a prepared agent waits for the transcript instead of starting a session.
    ResumeLoading,
    Resume {
        acp_session_id: Option<String>,
        title: Option<String>,
        transcript: Option<Box<nori_harness::transcript::Transcript>>,
    },
}

enum CandidateAgent {
    Preparing {
        generation: crate::app_event::SessionGeneration,
        agent_name: String,
        abort: tokio::task::AbortHandle,
    },
    Prepared {
        agent_name: String,
        display_name: String,
        agent: Box<nori_harness::runtime::PreparedAgent>,
    },
    Activating {
        agent_name: String,
        widget: Box<ChatWidget>,
        activation_error: Option<String>,
        activation_route_eligible: bool,
    },
}

#[derive(Clone, Debug)]
struct SystemInfoRefreshRequest {
    dir: PathBuf,
    model: Option<String>,
    first_message: Option<String>,
}

mod commit_changes;
mod config_persistence;
mod events;
mod exit;
pub(crate) mod focus;
mod frame;
mod history_replay;
mod input;
mod lens;
mod message_actions;
pub(crate) mod model_routes;
mod overlay;
mod recover;
mod reference_session;
pub(crate) mod rewind;
pub(crate) mod session_move;
mod session_setup;
mod session_view;
mod storage_notice;
mod subagent_overlay;
mod tool_gesture;
mod tool_history;
mod transcript;
mod transcript_window;

impl App {
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        tui: &mut tui::Tui,
        auth_manager: Arc<AuthManager>,
        config: NoriConfig,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        resume_selection: ResumeSelection,
        vertical_footer: bool,
        cloud_mode: bool,
        cloud_onboard: bool,
        startup_remote_addr: Option<std::net::SocketAddr>,
        analytics: Option<nori_installed::AnalyticsReporter>,
    ) -> Result<AppExitInfo> {
        use tokio_stream::StreamExt;

        let (app_event_tx, mut app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);

        let enhanced_keys_supported = tui.enhanced_keys_supported();

        // Every subprocess starts in the prepared state. Session activation is
        // a later user/session decision, after any per-session skillset setup.
        let needs_deferred_spawn = true;
        let mut pending_session_activation = None;
        let mut chat_widget = {
            let init = crate::chatwidget::ChatWidgetInit {
                config: config.clone(),
                frame_requester: tui.frame_requester(),
                app_event_tx: app_event_tx.clone(),
                initial_prompt: initial_prompt.clone(),
                initial_images: initial_images.clone(),
                enhanced_keys_supported,
                auth_manager: auth_manager.clone(),
                vertical_footer,
                footer_segment_config: config.footer_segment_config.clone(),
                footer_layout_config: config.footer_layout_config.clone(),
                cloud_mode,
                deferred_spawn: needs_deferred_spawn,
                fork_context: None,
                prepared_agent: None,
                analytics: analytics.clone(),
            };
            match resume_selection {
                ResumeSelection::Resume(target) => {
                    // The transcript loads while the agent starts, never before the first frame.
                    let tx = app_event_tx.clone();
                    tokio::spawn(async move {
                        let loader =
                            nori_harness::transcript::TranscriptLoader::new(target.nori_home);
                        let transcript = loader
                            .load_transcript(&target.project_id, &target.session_id)
                            .await
                            .map(Box::new)
                            .map_err(|error| error.to_string());
                        tx.send(AppEvent::ResumeTranscriptLoaded { transcript });
                    });
                    pending_session_activation = Some(PendingSessionActivation::ResumeLoading);
                    ChatWidget::new(init)
                }
                ResumeSelection::StartFresh | ResumeSelection::Exit => {
                    if !cloud_mode && (initial_prompt.is_some() || !initial_images.is_empty()) {
                        pending_session_activation = Some(PendingSessionActivation::New);
                    }
                    ChatWidget::new(init)
                }
            }
        };

        crate::startup_trace::mark("chat_widget");
        chat_widget.maybe_prompt_windows_sandbox_enable();

        let file_search = FileSearchManager::new(config.cwd.clone(), app_event_tx.clone());
        crate::terminal_status::rex_links::follow(app_event_tx.clone());
        storage_notice::follow(app_event_tx.clone(), config.nori_home.clone());
        #[cfg(not(debug_assertions))]
        let upgrade_version = crate::updates::get_upgrade_version(&config);

        crate::startup_trace::mark("file_search");
        let (system_info_tx, system_info_rx) = mpsc::channel();
        let _system_info_worker =
            Self::spawn_system_info_worker(system_info_rx, app_event_tx.clone());
        let footer_segment_config = config.footer_segment_config.clone();
        let footer_layout_config = config.footer_layout_config.clone();
        let skillset_per_session = !cloud_mode && config.skillset_per_session;

        let mut app = Self {
            app_event_tx,
            chat_widget,
            auth_manager: auth_manager.clone(),
            remote_control: crate::remote_control::RemoteControlManager::new(),
            config,
            vertical_footer,
            cloud_mode,
            analytics,
            file_search,
            enhanced_keys_supported,
            transcript_cells: Vec::new(),
            response_boundaries: Vec::new(),
            active_transcript_id: None,
            hidden_transcript_len: None,
            overlay: None,
            subagent_switcher_state: None,
            subagent_switcher_transcript: None,
            session_view: Default::default(),
            tool_gesture: Default::default(),
            program_status: Default::default(),
            replay_painted_from: None,
            focus: None,
            pending_move: None,
            move_in_progress: false,
            recoveries: Vec::new(),
            pending_handover: None,
            pending_child_focus: None,
            child_focus_sequence: 0,
            pending_update_action: None,
            skip_world_writable_scan_once: false,
            prepared_agent: None,
            prepared_agent_initial_context: None,
            primary_agent_preparation: None,
            pending_session_activation,
            candidate_agent: None,
            pending_candidate_model: None,
            model_route_fallback: None,
            next_model_route_token: 0,
            worktree_warning_shown: false,
            manual_model_handoff: None,
            waiting_model_presses: Default::default(),
            welcome_card_written: false,
            session_card_shown: false,
            loop_count_override: None,
            hotkey_config: nori_config::HotkeyConfig::default(),
            vim_mode: nori_config::VimEnterBehavior::Off,
            footer_segment_config,
            footer_layout_config,
            plan_drawer_mode: crate::chatwidget::PlanDrawerMode::Off,
            commit_turn_pending: false,
            system_info_tx,
            deferred_spawn_pending: skillset_per_session,
            mcp_oauth_cancel_tx: None,
        };

        crate::startup_trace::mark("app_struct");
        if let Some(addr) = startup_remote_addr {
            let report = app
                .remote_control
                .enable_startup(addr, false)
                .await
                .map_err(|error| color_eyre::eyre::eyre!(error))?;
            for url in report.urls() {
                tracing::info!("remote ACP transport listening on {url}");
            }
            app.chat_widget.add_boxed_history(Box::new(
                crate::history_cell::new_remote_control_event(report.lines()),
            ));
        }

        // Propagate NoriConfig settings to the textarea.
        app.hotkey_config = app.config.hotkeys.clone();
        app.vim_mode = app.config.vim_mode;

        // Propagate initial hotkey config to the textarea so editing bindings
        // (ctrl+a, ctrl+e, etc.) respect user overrides from config.toml.
        app.chat_widget.set_hotkey_config(app.hotkey_config.clone());
        app.chat_widget.set_keymap(app.config.keymap.clone());
        app.chat_widget.update_approval_mode_label();
        // Propagate initial vim mode setting.
        app.chat_widget.set_vim_mode(app.vim_mode);
        app.chat_widget.set_kakoune_mode(app.config.kakoune_mode);
        // Propagate initial pinned plan drawer setting.
        let plan_mode = if app.config.pinned_plan_drawer {
            crate::chatwidget::PlanDrawerMode::Expanded
        } else {
            crate::chatwidget::PlanDrawerMode::Off
        };
        app.plan_drawer_mode = plan_mode;
        app.chat_widget.set_plan_drawer_mode(plan_mode);

        // Fill the footer with the git facts we can read in a few milliseconds.
        // The background refresh below replaces this with the full snapshot,
        // but it spawns Node for the skillset version and walks the filesystem
        // for transcripts, so without a seed every footer segment stays blank
        // for as long as that takes.
        crate::startup_trace::mark("widget_config");
        app.chat_widget.seed_system_info();
        crate::startup_trace::mark("system_info_seeded");

        // Write the welcome card before the agent session exists, so a session
        // that has not been prompted yet still says where it is running and
        // which agent it will run. Two entries opt out. A cloud session's card
        // names the broker session, which it only learns once attached. And a
        // pending activation — an initial prompt, or a resume — replaces this
        // widget before the user can read anything, so its card belongs to the
        // widget that takes over.
        if !cloud_mode && app.pending_session_activation.is_none() {
            app.chat_widget.emit_welcome_card();
            app.welcome_card_written = true;
            app.session_card_shown = true;
        }

        // If skillset_per_session is enabled, show the skillset picker. The
        // agent preparation was deferred so that `nori-skillsets switch` can
        // write `.claude/CLAUDE.md` before the agent reads it. Applying or
        // dismissing the picker starts preparation without activating a
        // session.
        crate::startup_trace::mark("welcome_card");
        if skillset_per_session {
            app.chat_widget.handle_switch_skillset_command();
        } else if app.pending_session_activation.is_some() {
            app.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
        } else if cloud_mode {
            if cloud_onboard {
                app.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Onboarding);
            } else {
                // Picker-first entry: list live sessions before anything can
                // claim one; "start new" is an explicit row in the picker.
                app.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Picker {
                    fallback_to_spawn: true,
                });
            }
        } else {
            // Plain local startup activates the session on its own so
            // session-scoped features (model/config pickers, prompt history)
            // work before the first prompt. The welcome card above already ran,
            // so it still shows while the agent resolves its configuration.
            app.pending_session_activation = Some(PendingSessionActivation::New);
            app.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
        }

        // On startup, if Agent mode (workspace-write) or ReadOnly is active, warn about world-writable dirs on Windows.
        #[cfg(target_os = "windows")]
        {
            let should_check = codex_sandbox::get_platform_sandbox().is_some()
                && matches!(
                    app.config.sandbox_policy,
                    nori_config::SandboxPolicy::WorkspaceWrite { .. }
                        | nori_config::SandboxPolicy::ReadOnly
                )
                && !app
                    .config
                    .notices
                    .hide_world_writable_warning
                    .unwrap_or(false);
            if should_check {
                let cwd = app.config.cwd.clone();
                let env_map: std::collections::HashMap<String, String> = std::env::vars().collect();
                let tx = app.app_event_tx.clone();
                let logs_base_dir = app.config.nori_home.clone();
                let sandbox_policy = app.config.sandbox_policy.clone();
                Self::spawn_world_writable_scan(cwd, env_map, logs_base_dir, sandbox_policy, tx);
            }
        }

        #[cfg(not(debug_assertions))]
        if let Some(latest_version) = upgrade_version {
            app.handle_event(
                tui,
                AppEvent::InsertHistoryCell(Box::new(UpdateAvailableHistoryCell::new(
                    latest_version,
                    crate::update_action::get_update_action(),
                ))),
            )
            .await?;
        }

        app.program_status.idle();
        crate::startup_trace::mark("app_ready");
        let tui_events = tui.event_stream();
        tokio::pin!(tui_events);

        app.request_system_info_refresh(
            app.config.cwd.clone(),
            Some(app.config.active_agent.clone()),
            app.chat_widget.first_prompt_text(),
        );

        // The first frame is drawn here rather than requested: under load the scheduler task,
        // the broadcast and the event stream between a request and its draw can each wait to be
        // scheduled, and nothing the frame shows depends on them.
        let keep_running = app
            .handle_tui_event_batched(tui, &mut app_event_rx, TuiEvent::Draw)
            .await?;

        // Every handler is timed (see `crate::latency`): the loop does one thing at a time, so a
        // slow handler is a stall of the whole interface.
        while keep_running
            && select! {
                Some(event) = app_event_rx.recv() => {
                    let kind: &'static str = (&event).into();
                    let started = std::time::Instant::now();
                    let keep_running = app.handle_event(tui, event).await?;
                    crate::latency::handled(kind, started.elapsed());
                    keep_running
                }
                Some(event) = tui_events.next() => {
                    let kind = match &event {
                        TuiEvent::Key(key) => {
                            if key.kind == crossterm::event::KeyEventKind::Press {
                                crate::latency::begin(crate::latency::Interaction::Key, true);
                            }
                            "terminal: key"
                        }
                        TuiEvent::Paste(_) => "terminal: paste",
                        TuiEvent::Mouse(_) => "terminal: mouse",
                        TuiEvent::Draw => "terminal: draw (events applied + frame)",
                    };
                    let draw = matches!(event, TuiEvent::Draw);
                    let started = std::time::Instant::now();
                    let keep_running =
                        app.handle_tui_event_batched(tui, &mut app_event_rx, event).await?;
                    crate::latency::handled(kind, started.elapsed());
                    if draw {
                        crate::latency::frame_drawn();
                    }
                    keep_running
                }
            }
        {
            app.sync_program_status(tui);
        }
        crate::latency::write_report();

        app.program_status.clear();
        tui.flush_status();
        app.cancel_primary_agent_preparation();
        app.prepared_agent_initial_context = None;
        app.discard_candidate();
        let immediate_exit = app.chat_widget.immediate_exit_requested();
        let cleanup = async {
            if let Some(agent) = app.prepared_agent.take() {
                agent.shutdown().await;
            }
            app.remote_control.disable().await;
        };
        if immediate_exit {
            // Terminal restoration must not wait for an unresponsive provider.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(100), cleanup).await;
        } else {
            cleanup.await;
        }

        // Don't clear terminal to allow exit message to remain visible
        // tui.terminal.clear()?;

        Ok(AppExitInfo {
            immediate_exit,
            token_usage: app.token_usage(),
            conversation_id: app.chat_widget.conversation_id(),
            conversation_has_activity: app.chat_widget.session_stats().has_activity(),
            update_action: app.pending_update_action,
            handover: app.pending_handover,
        })
    }

    pub(super) fn chat_widget_init(
        &self,
        frame_requester: crate::tui::FrameRequester,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        _expected_agent: Option<String>,
        deferred_spawn: bool,
        fork_context: Option<String>,
    ) -> crate::chatwidget::ChatWidgetInit {
        crate::chatwidget::ChatWidgetInit {
            config: self.config.clone(),
            frame_requester,
            app_event_tx: self.app_event_tx.clone(),
            initial_prompt,
            initial_images,
            enhanced_keys_supported: self.enhanced_keys_supported,
            auth_manager: self.auth_manager.clone(),
            vertical_footer: self.vertical_footer,
            footer_segment_config: self.footer_segment_config.clone(),
            footer_layout_config: self.footer_layout_config.clone(),
            cloud_mode: self.cloud_mode,
            deferred_spawn,
            fork_context,
            prepared_agent: None,
            analytics: self.analytics.clone(),
        }
    }

    pub(super) fn configure_new_chat_widget(&mut self) {
        // Lazy activation swaps in a fresh widget the first time the user
        // submits a prompt. That widget continues the conversation the startup
        // card already described, so it must not write a second one. Taking the
        // flag scopes this to that one handover: a later `/new` is a different
        // conversation and writes its own card when its session starts.
        if std::mem::take(&mut self.welcome_card_written) {
            self.chat_widget.suppress_welcome_card();
        }
        if self.session_card_shown {
            self.chat_widget.suppress_welcome_card();
        }
        // The replacement carries its own empty footer, so re-seed it rather
        // than leaving every segment blank until the next background refresh.
        self.chat_widget.seed_system_info();
        self.chat_widget.update_approval_mode_label();
        self.chat_widget
            .set_hotkey_config(self.hotkey_config.clone());
        self.chat_widget.set_keymap(self.config.keymap.clone());
        self.chat_widget.set_vim_mode(self.vim_mode);
        self.chat_widget.set_kakoune_mode(self.config.kakoune_mode);
        self.chat_widget.set_plan_drawer_mode(self.plan_drawer_mode);
        self.chat_widget
            .set_loop_count_override(self.loop_count_override);
    }

    pub(crate) fn token_usage(&self) -> TokenUsage {
        self.chat_widget.token_usage()
    }

    fn request_system_info_refresh(
        &self,
        dir: PathBuf,
        model: Option<String>,
        first_message: Option<String>,
    ) {
        let request = SystemInfoRefreshRequest {
            dir,
            model,
            first_message,
        };
        if self.system_info_tx.send(request).is_err() {
            tracing::error!("system info refresh channel is closed");
        }
    }

    fn spawn_system_info_worker(
        system_info_rx: mpsc::Receiver<SystemInfoRefreshRequest>,
        app_event_tx: AppEventSender,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            // Refresh only when a request arrives. The initial refresh is
            // scheduled explicitly via `request_system_info_refresh` at
            // startup, and subsequent refreshes are driven by user actions
            // (message submit, task completion, tool-call cwd changes,
            // skillset install/switch). No periodic polling.
            while let Ok(request) = system_info_rx.recv() {
                let agent_kind = request
                    .model
                    .as_ref()
                    .and_then(|model| nori_harness::AgentKind::from_slug(model));
                let info = crate::system_info::SystemInfo::collect_for_directory_with_message(
                    &request.dir,
                    agent_kind,
                    request.first_message.as_deref(),
                );
                app_event_tx.send(AppEvent::SystemInfoRefreshed(info));
            }
        })
    }
}
