use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
#[allow(unused_imports)]
use std::time::Duration;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use nori_config::NoriConfig as Config;
use nori_harness::ConversationId;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ApprovalRequest;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::InputResult;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::client_tool_cell::ClientToolCell;
use crate::clipboard_paste::paste_image_to_temp_png;
use crate::effective_cwd_tracker::EffectiveCwdTracker;
use crate::get_git_diff::get_git_changes;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::login_handler::AgentLoginSupport;
use crate::login_handler::LoginHandler;
#[allow(unused_imports)]
use crate::login_handler::LoginMethod;
use crate::render::Insets;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::FlexRenderable;
use crate::render::renderable::Renderable;
use crate::render::renderable::RenderableExt;
use crate::render::renderable::RenderableItem;
use crate::session_stats::SessionStats;
use crate::slash_command::SlashCommand;
use crate::text_formatting::truncate_text;
use crate::tui::FrameRequester;
mod actions;
pub(crate) mod agent;
mod agent_status;
mod conflate;
mod layout_cache;
mod local_echo;
mod model_cycle;
pub(crate) use model_cycle::ModelStep;
pub(crate) use model_cycle::WaitingModelPresses;
mod owned_transcript;
mod quit_keys;
mod session_transcript;
mod shell_completion;
mod stream_frame;
pub(crate) mod subagents;
pub(crate) mod tool_projection;
pub(crate) use self::agent::HarnessHandle;
use self::agent::spawn_acp_agent_resume;
use self::agent::spawn_agent;
pub(crate) use quit_keys::QuitKeyAction;
pub(crate) use rewind::Rewind;

mod approvals;
mod constructors;
mod context;
mod continue_turn;
pub(crate) use context::ContextSnapshot;
mod event_handlers;
mod goal;
mod helpers;
mod key_handling;
mod login;
mod login_input;
mod move_popup;
mod sign_in;
pub(crate) use sign_in::SignInTarget;
mod pickers;
mod remote_control;
mod replay;
pub(crate) mod rewind;
mod session_config_mode;
mod user_input;
mod user_shell_cell;
use crate::nori::session_header::CloudSessionInfo;
use crate::streaming::controller::StreamController;
use codex_common::approval_presets::ApprovalPreset;
use codex_common::approval_presets::builtin_approval_presets;

use crate::ui_types::PlanUpdate;
use codex_file_search::FileMatch;
use codex_login::AuthManager;
#[allow(unused_imports)]
use codex_login::CodexAuth;
use nori_config::AskForApproval;
use nori_config::SandboxPolicy;

const USER_SHELL_COMMAND_HELP_TITLE: &str = "Prefix a command with ! to run it locally";
const DEFAULT_PROJECT_DOC_FILENAME: &str = "AGENTS.md";
const USER_SHELL_COMMAND_HELP_HINT: &str = "Example: !ls";
/// Strip ANSI escape codes from a string.
/// Uses a simple state machine approach to handle common escape sequences.
#[cfg(feature = "login")]
fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip escape sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Skip until we hit a letter (the terminator)
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                // OSC sequence (Operating System Command)
                chars.next(); // consume ']'
                // Skip until BEL (\x07) or ST (ESC \)
                while let Some(&next) = chars.peek() {
                    if next == '\x07' {
                        chars.next();
                        break;
                    } else if next == '\x1b' {
                        chars.next();
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    chars.next();
                }
            }
        } else if c == '\r' {
            // Skip carriage return (handle Windows line endings)
            continue;
        } else {
            result.push(c);
        }
    }

    result
}

/// Common initialization parameters shared by all `ChatWidget` constructors.
pub(crate) struct ChatWidgetInit {
    pub(crate) config: Config,
    pub(crate) frame_requester: FrameRequester,
    pub(crate) app_event_tx: AppEventSender,
    pub(crate) initial_prompt: Option<String>,
    pub(crate) initial_images: Vec<PathBuf>,
    pub(crate) enhanced_keys_supported: bool,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) vertical_footer: bool,
    pub(crate) footer_segment_config: nori_config::FooterSegmentConfig,
    pub(crate) footer_layout_config: nori_config::FooterLayoutConfig,
    /// Whether the top-level CLI launched the handroll-backed cloud mode.
    pub(crate) cloud_mode: bool,
    /// When true, build a sessionless widget. The app prepares the agent
    /// separately and later supplies that connection when a session activates.
    pub(crate) deferred_spawn: bool,
    /// Optional conversation context to inject into the first prompt.
    /// Used by `/fork` to pass prior conversation history to the new session.
    pub(crate) fork_context: Option<String>,
    /// A live initialized connection to consume instead of spawning one.
    pub(crate) prepared_agent: Option<nori_harness::runtime::PreparedAgent>,
    /// Emits one authenticated activity after this session's first prompt reaches ACP.
    pub(crate) analytics: Option<nori_installed::AnalyticsReporter>,
}

/// Controls the pinned plan drawer visibility and display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanDrawerMode {
    /// Drawer is hidden; plan updates go to history cells.
    Off,
    /// Drawer shows a single-line progress summary.
    Collapsed,
    /// Drawer shows the full plan checklist.
    Expanded,
}

pub(crate) struct ChatWidget {
    app_event_tx: AppEventSender,
    bottom_pane: BottomPane,
    /// The cell still being built (a tool call, the user's prompt), shown above the composer.
    active_cell: layout_cache::CachedCell,
    /// The rendered but not yet final tail of the streaming answer, shown above the composer.
    live_tail: layout_cache::CachedCell,
    /// Rows of the terminal, so the viewport can leave room for history to flush above it.
    screen_rows: std::cell::Cell<u16>,
    subagent_card_hits: std::cell::RefCell<Vec<(Rect, crate::subagent_switcher::SubagentKey)>>,
    pinned_plan_area: std::cell::Cell<Option<Rect>>,
    active_tool_hits: std::cell::RefCell<Vec<(Rect, String)>>,
    bottom_pane_area: std::cell::Cell<Option<ratatui::layout::Rect>>,
    /// Whether the live tail starts the message, so it carries the `• ` header.
    live_tail_has_header: bool,
    /// Subagent sessions of this conversation.
    subagents: subagents::SubagentTree,
    focused_subagent: Option<crate::subagent_switcher::SubagentKey>,
    main_queued_prompts: Vec<String>,
    child_queued_prompts: HashMap<nori_protocol::acp::v1::SessionId, Vec<String>>,
    focused_transcript: Option<session_transcript::SessionTranscript>,
    quit_keys: quit_keys::QuitKeys,
    focused_session_config: Option<crate::nori::agent_config_state::AgentConfigState>,
    /// How many running subagents the status line currently names.
    subagents_running_shown: usize,
    /// A rewind waiting for the edit journal (see `rewind`).
    pending_rewind: Option<rewind::Rewind>,
    /// The prompt of the turn running now, and whether the agent has answered it yet.
    turn_prompt: Option<String>,
    turn_reached_agent: bool,
    /// The last turn, when it did not finish (see `continue_turn`).
    unfinished: Option<continue_turn::Unfinished>,
    active_user_message_id: Option<String>,
    /// ACP message id for the answer currently being streamed. A changed id
    /// starts a new history cell even when no tool or lifecycle event separates
    /// the two messages.
    active_answer_message_id: Option<String>,
    config: Config,
    auth_manager: Arc<AuthManager>,
    initial_user_message: Option<UserMessage>,
    /// True once `initial_user_message` was painted before its session existed.
    initial_echo_painted: bool,
    /// Prompts painted locally ahead of the harness echo.
    pending_echoes: local_echo::PendingEchoes,
    // Stream lifecycle controller
    stream_controller: Option<StreamController>,
    /// The turn ended while the stream still had a burst to render, which the frames that follow
    /// finish before the answer is finalized (see `stream_frame`).
    stream_ending: bool,
    session_generation: crate::app_event::SessionGeneration,
    owned_prompt_request_id: Option<nori_protocol::acp::v1::RequestId>,
    proactive_turn_active: bool,
    unpaired_prompt_error_ids: HashSet<nori_protocol::acp::v1::RequestId>,
    completed_client_tool_calls: HashSet<String>,
    /// Calls committed in turns that have ended. An update can still arrive for one (an agent
    /// closes a call after its turn's response), and must update its cell, not make a second.
    settled_client_tool_calls: HashSet<String>,
    client_event_normalizer: crate::presentation::ClientEventNormalizer,
    replay_source: Option<nori_protocol::ReplaySource>,
    /// How long the replayed turn had been running, as its latest update says.
    replay_turn_elapsed: Option<Duration>,
    // Accumulates the current reasoning block text to extract a header
    reasoning_buffer: String,
    // Accumulates full reasoning content for transcript-only recording
    full_reasoning_buffer: String,
    // Current status header shown in the status indicator.
    current_status_header: String,
    // Previous status header to restore after a transient stream retry.
    conversation_id: Option<ConversationId>,
    // The parent conversation id after a branch-at-head fork, shown as the
    // `forked from:` row on the status card.
    forked_from: Option<ConversationId>,
    frame_requester: FrameRequester,
    // Whether to include the initial welcome banner on session configured
    show_welcome_banner: bool,
    // When resuming an existing session (selected via resume picker), avoid an
    // immediate redraw on SessionConfigured to prevent a gratuitous UI flicker.
    suppress_session_configured_redraw: bool,
    // Pending notification to show when unfocused on next Draw
    pending_notification: Option<Notification>,
    // Whether to add a final message separator after the last message
    needs_final_message_separator: bool,

    last_rendered_width: std::cell::Cell<Option<usize>>,
    // Current session rollout path (if known)
    current_rollout_path: Option<PathBuf>,
    // Buffers incomplete Execute ClientToolCells displaced from active_cell
    // by subsequent tool snapshots. Completions check here before discarding.
    pending_client_tool_cells: HashMap<String, ClientToolCell>,
    /// Cells that arrived while an answer was streaming (a subagent that finished, a notice).
    /// They wait for the message to be whole and are committed after it, never inside it.
    deferred_cells: Vec<Box<dyn HistoryCell>>,
    // Tracks the effective CWD based on tool call locations for footer updates.
    effective_cwd_tracker: EffectiveCwdTracker,
    // Whether SessionConfigured has been received for this widget.
    session_configured_received: bool,
    // Typed handle for the active harness session.
    harness_handle: Option<HarnessHandle>,
    // True while /close awaits the agent's session/close response. Blocks
    // session-switching commands so the deferred NewSession can't clobber a
    // conversation the user switched to mid-close.
    session_close_in_flight: bool,
    /// True once the user asked to quit: input is refused, and a hard-exit
    /// watchdog guarantees the process leaves within ~a second even if the
    /// backend teardown stalls. On cloud agents this exit is a detach — the
    /// session keeps running server-side.
    exiting: bool,
    /// The agent's advertised session configuration, in advertised order.
    /// Authoritative for `/config`, the mode footer, the config history lines,
    /// and the status card.
    agent_config: crate::nori::agent_config_state::AgentConfigState,
    /// Model presses (Ctrl+N / Ctrl+P) still being applied.
    model_cycle: model_cycle::ModelCycle,
    /// The agent identity and configuration as rendered by the status views.
    /// Shared with the welcome card so it fills in when the agent advertises
    /// its configuration after the card was already written to history.
    agent_status: crate::nori::session_header::AgentStatusHandle,
    acp_mode_config: Option<crate::nori::session_config_mode::AcpModeConfig>,
    acp_mode_config_generation: i64,
    // Session statistics tracking
    session_stats: SessionStats,
    assistant_stream_seen_for_stats: bool,
    // Login handler for /login command
    login_handler: Option<LoginHandler>,
    /// A provider's sign-in program while it runs, drawn above the prompt.
    login_terminal: Option<crate::login_terminal::LoginTerminal>,
    /// The provider a sign-in through its agent is for, while it runs.
    sign_in_target: Option<SignInTarget>,
    // Failed/selected switch target used only to route a bare /login. This
    // does not imply a pending agent switch.
    login_agent_override: Option<String>,
    login_child_target: Option<crate::subagent_switcher::SubagentKey>,
    // The first user prompt text, preserved for /first-prompt command
    first_prompt_text: Option<String>,
    // Latest ACP-owned goal snapshot for this session.
    current_goal: Option<nori_protocol::ThreadGoal>,
    // Latest ACP capability snapshot projected into Nori client concepts.
    session_agent_capabilities: crate::presentation::AgentCapabilitiesView,
    /// Whether the top-level CLI launched the handroll-backed cloud mode.
    cloud_mode: bool,
    // Identity reported by the active agent during ACP initialization.
    session_agent_info: Option<nori_protocol::acp::v1::Implementation>,
    /// Whether the live ACP attachment accepts observation but not prompts.
    follow_only_attachment: bool,
    // Latest structured session metadata for status/footer consumers.
    session_info_state: crate::nori::session_info::SessionInfoState,
    // How much session-info metadata this build writes to the transcript.
    session_info_detail: crate::nori::session_info::SessionInfoDetail,
    /// The agent-assigned ACP session id of the active session, when known
    /// (seeded by the resume path, refreshed by SessionConfigured).
    acp_session_id: Option<String>,
    /// Broker-reported title for the resumed cloud session, when known.
    cloud_session_title: Option<String>,
    builtin_command_availability: HashMap<String, nori_protocol::CommandAvailability>,
    // Whether `/goal` is waiting for the backend to return a goal snapshot.
    pending_goal_status: bool,
    // Whether `/goal edit` is waiting for the backend to return a goal snapshot.
    pending_goal_edit: bool,
    // Loop mode state: remaining iterations (None = not looping)
    loop_remaining: Option<i32>,
    // Loop mode state: total iterations configured
    loop_total: Option<i32>,
    // Ephemeral per-session override for loop_count (set via /settings menu).
    // Outer Option: whether overridden; inner Option<i32>: the value.
    loop_count_override: Option<Option<i32>>,
    acp_session_phase: Option<crate::presentation::session_runtime::SessionPhaseView>,
    /// Whether and how plan updates are rendered in a pinned drawer instead of
    /// history cells.
    plan_drawer_mode: PlanDrawerMode,
    /// Latest plan state, always updated on every plan event. Used by the
    /// pinned plan drawer when enabled; retained when disabled so toggling
    /// the drawer on shows the most recent plan immediately.
    pinned_plan: Option<PlanUpdate>,

    // Terminal title state: cache to avoid redundant OSC writes.
    last_terminal_title: Option<String>,
    user_shell_state: user_shell_cell::UserShellState,
}

struct UserMessage {
    text: String,
    image_paths: Vec<PathBuf>,
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            image_paths: Vec::new(),
        }
    }
}

impl From<&str> for UserMessage {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            image_paths: Vec::new(),
        }
    }
}

fn create_initial_user_message(text: String, image_paths: Vec<PathBuf>) -> Option<UserMessage> {
    if text.is_empty() && image_paths.is_empty() {
        None
    } else {
        Some(UserMessage { text, image_paths })
    }
}

impl ChatWidget {}

impl Drop for ChatWidget {
    fn drop(&mut self) {
        if let Err(err) = self.clear_managed_terminal_title() {
            tracing::debug!(error = %err, "failed to clear terminal title on drop");
        }
    }
}

impl ChatWidget {
    pub(crate) fn subagent_parent(
        &self,
        key: crate::subagent_switcher::SubagentKey,
    ) -> Option<crate::subagent_switcher::SubagentKey> {
        self.subagents.parent_for_key(key)
    }

    pub(crate) fn subagent_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::subagent_switcher::SubagentKey> {
        let pos = ratatui::layout::Position::new(column, row);
        self.subagent_card_hits
            .borrow()
            .iter()
            .find(|(rect, _)| rect.contains(pos))
            .map(|(_, key)| *key)
    }

    pub(crate) fn plan_drawer_contains(&self, column: u16, row: u16) -> bool {
        let pos = ratatui::layout::Position::new(column, row);
        self.pinned_plan_area
            .get()
            .is_some_and(|rect| rect.contains(pos))
    }

    pub(crate) fn is_chat_content_row(&self, row: u16) -> bool {
        self.bottom_pane_area
            .get()
            .is_some_and(|area| row < area.top())
    }

    pub(crate) fn place_composer_cursor(&mut self, column: u16, row: u16) -> bool {
        let Some(area) = self.bottom_pane_area.get() else {
            return false;
        };
        self.bottom_pane.place_cursor_at(column, row, area)
    }

    pub(crate) fn composer_cursor_style(&self) -> crossterm::cursor::SetCursorStyle {
        self.bottom_pane.composer_cursor_style()
    }

    /// The rows the chat keeps at rest: the composer and its status line, without any popup or
    /// view, working indicator, running tool, streaming answer or live subagent lanes above it. Only these decide
    /// what the conversation hands to the scrollback; everything else on top of them is
    /// transient and clips the conversation instead, so nothing leaves the screen for good just
    /// because something live was tall for a while.
    pub(crate) fn resting_height(&self, width: u16) -> u16 {
        self.bottom_pane.resting_height(width).max(1)
    }

    pub(crate) fn primary_composer_height(&self, width: u16) -> u16 {
        self.bottom_pane.desired_height(width).saturating_add(1)
    }
}

impl Renderable for ChatWidget {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_renderable().render(area, buf);
        self.last_rendered_width.set(Some(area.width as usize));
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.as_renderable().desired_height(width)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.as_renderable().cursor_pos(area)
    }
}

enum Notification {
    AgentTurnComplete { response: String },
    ExecApprovalRequested { command: String },
}

impl Notification {
    fn display(&self) -> String {
        match self {
            Notification::AgentTurnComplete { response } => {
                Notification::agent_turn_preview(response)
                    .unwrap_or_else(|| "Agent turn complete".to_string())
            }
            Notification::ExecApprovalRequested { command } => {
                format!("Approval requested: {}", truncate_text(command, 30))
            }
        }
    }

    fn agent_turn_preview(response: &str) -> Option<String> {
        let mut normalized = String::new();
        for part in response.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str(part);
        }
        let trimmed = normalized.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_text(trimmed, AGENT_NOTIFICATION_PREVIEW_GRAPHEMES))
        }
    }
}

const AGENT_NOTIFICATION_PREVIEW_GRAPHEMES: usize = 200;

/// Keep the cold-start prompt quiet and stable; shortcuts live in the docs.
fn session_placeholder() -> String {
    "Ask Nori to do anything".to_owned()
}

// Extract the first bold (Markdown) element in the form **...** from `s`.
// Returns the inner text if found; otherwise `None`.
/// Truncate a string for logging purposes.
#[allow(dead_code)]
fn truncate_for_log(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.replace('\n', "\\n")
    } else {
        format!("{}...", s[..max_len].replace('\n', "\\n"))
    }
}

fn extract_first_bold(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'*' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'*' {
                    // Found closing **
                    let inner = &s[start..j];
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    } else {
                        return None;
                    }
                }
                j += 1;
            }
            // No closing; stop searching (wait for more deltas)
            return None;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
pub(crate) mod tests;
