//! Session state and management
//!
//! Each session represents an active Claude conversation with its own
//! ClaudeClient instance, usage tracking, and permission state.

mod build;
mod cache_key;
mod lifecycle;

use dashmap::DashMap;
use nori_acp_kit::Cx;
use nori_acp_kit::Turn;
use nori_acp_kit::TurnGate;
use nori_acp_kit::TurnPolicy;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use claude_code_agent_sdk::ClaudeAgentOptions;
use claude_code_agent_sdk::ClaudeClient;
use claude_code_agent_sdk::HookEvent;
use claude_code_agent_sdk::HookMatcher;
use claude_code_agent_sdk::McpServerConfig;
use claude_code_agent_sdk::McpServers;
use claude_code_agent_sdk::SettingSource;
use claude_code_agent_sdk::SystemPrompt;
use claude_code_agent_sdk::SystemPromptPreset;
use claude_code_agent_sdk::types::config::PermissionMode as SdkPermissionMode;
use claude_code_agent_sdk::types::mcp::McpSdkServerConfig;
use nori_acp_kit::schema::CurrentModeUpdate;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionModeId;
use nori_acp_kit::schema::SessionNotification;
use nori_acp_kit::schema::SessionUpdate;
use tokio::sync::RwLock;
use tracing::instrument;

use crate::converter::EditSnapshots;
use crate::converter::NotificationConverter;
use crate::hooks::create_pre_tool_use_hook;
use crate::mcp::AcpMcpServer;
use crate::mcp::HostToolsBinding;
use crate::mcp::SessionTools;
use crate::permissions::create_can_use_tool_callback;
use crate::settings::PermissionChecker;
use crate::settings::SettingsManager;
use crate::terminal::TerminalClient;
use crate::types::AgentConfig;
use crate::types::NewSessionMeta;
use crate::types::Result;

pub use cache_key::stable_cache_key;

use super::BackgroundProcessManager;
use super::permission::PermissionHandler;
use super::permission::PermissionMode;
use super::usage::UsageLedger;

/// How long a closing session's CLI has to exit by itself before what is left is stopped.
const CLI_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Get the list of tools that should be replaced by ACP MCP server tools.
///
/// Only tools that interact with the terminal or filesystem should be replaced:
/// - Terminal tools: Bash, BashOutput, KillShell
/// - File tools: Read, Write, Edit
///
/// Other tools like Glob, Grep, Task, etc. should remain as CLI built-in tools.
pub(crate) fn get_acp_replacement_tools() -> Vec<&'static str> {
    vec![
        // Terminal tools - must be replaced to use ACP Terminal API
        "Bash",
        "BashOutput",
        "KillShell",
        // File tools - must be replaced for ACP file synchronization
        "Read",
        "Write",
        "Edit",
    ]
}

/// Fast mode as a session has it: what was chosen, and whether the CLI has yet to be told.
#[derive(Default)]
struct FastModeState {
    chosen: AtomicBool,
    pending: AtomicBool,
}

/// An active Claude session
///
/// Each session holds its own ClaudeClient instance and maintains
/// independent state for usage tracking, permissions, and message conversion.
pub struct Session {
    /// Unique session identifier
    pub session_id: String,
    /// Working directory for this session
    pub cwd: PathBuf,
    /// The Claude client for this session
    client: RwLock<ClaudeClient>,
    /// Permission handler for tool execution (wrapped in Arc for can_use_tool callback)
    permission: Arc<RwLock<PermissionHandler>>,
    /// Token usage tracker
    usage: UsageLedger,
    /// Notification converter with tool use cache (wrapped for interior mutability)
    converter: RwLock<NotificationConverter>,
    /// Whether the client is connected
    connected: AtomicBool,

    /// Whether the session is ready for its first query. Holding the lock serializes the work
    /// that gets it there.
    ready: tokio::sync::Mutex<bool>,
    /// Permission checker for hooks
    permission_checker: Arc<RwLock<PermissionChecker>>,
    /// Current model ID for this session: the one it launched with, then whatever it was
    /// switched to.
    current_model: tokio::sync::RwLock<Option<String>>,
    /// The models the session's Claude CLI said this account can use (its initialize
    /// `models`), as it reported them; empty until the CLI has started.
    reported_models: std::sync::RwLock<Vec<serde_json::Value>>,
    /// A chosen model the running CLI has not been told about yet. The CLI can only be told
    /// once a query exists, so a switch made between prompts waits for the next one.
    model_pending: std::sync::atomic::AtomicBool,
    /// The chosen Fast mode, and whether the running CLI has been told about it yet.
    fast_mode: FastModeState,
    /// ACP MCP server for tool execution with notifications
    acp_mcp_server: Arc<AcpMcpServer>,
    /// Nori's tools, which a session started ahead of time gets when it is claimed.
    host_tools: Option<Arc<HostToolsBinding>>,
    /// Background process manager
    background_processes: Arc<BackgroundProcessManager>,
    /// Connection context OnceLock for ACP requests (shared with hooks)
    /// Used by pre_tool_use_hook for permission requests
    connection_cx_lock: Arc<OnceLock<Cx>>,
    /// Cache for tool_use_id by tool_input
    /// PreToolUse hook caches this when Ask decision is made
    /// can_use_tool callback uses this to get tool_use_id when CLI doesn't provide it
    /// Key: stable cache key of tool_input, Value: tool_use_id
    tool_use_id_cache: Arc<DashMap<String, String>>,
    /// The session's turns. A prompt that arrives while a turn runs is admitted and waits its
    /// place in [`Self::turn_order`]: it never cancels the work under way. A cancel (from the
    /// client or an MCP cancellation) cancels the running turn and every one waiting.
    gate: Arc<TurnGate>,
    /// Prompts reach the CLI one turn at a time, in the order they arrived.
    turn_order: Arc<tokio::sync::Mutex<()>>,
}

/// A prompt's turn, once it is this prompt's turn to talk to the CLI. Dropping it lets the next
/// waiting prompt go.
pub struct PromptTurn {
    turn: Turn,
    _order: tokio::sync::OwnedMutexGuard<()>,
}

impl PromptTurn {
    pub fn is_cancelled(&self) -> bool {
        self.turn.is_cancelled()
    }
}

impl Session {
    /// Begin a turn: a prompt that arrives while one runs waits for it to end, then runs. The
    /// turn ends when the returned value is dropped; [`Session::cancel`] cancels it whether it
    /// runs or waits, and a cancelled turn that was waiting ends without reaching the CLI.
    pub async fn begin_turn(&self) -> Result<PromptTurn> {
        let turn = self.gate.begin().await?;
        let order = Arc::clone(&self.turn_order).lock_owned().await;
        if !turn.is_cancelled()
            && let Some(tools) = &self.host_tools
        {
            tools.reset_cancelled();
        }
        Ok(PromptTurn {
            turn,
            _order: order,
        })
    }

    /// Wait until no turn is running and none has started for `quiet`: a moment when background
    /// work (such as starting the next session's CLI) slows nobody down.
    pub async fn wait_until_idle(&self, quiet: std::time::Duration) {
        loop {
            tokio::time::sleep(quiet).await;
            if !self.gate.is_running() {
                return;
            }
            self.gate.idle().await;
        }
    }

    /// Give a session started ahead of time the tools its request carried.
    pub fn adopt_tools(&self, tools: &SessionTools) {
        if let (Some(mine), Some(theirs)) = (&self.host_tools, &tools.host_tools)
            && let Some(theirs) = theirs.tools()
        {
            mine.bind(theirs);
        }
    }

    /// Set the connection context for ACP requests
    ///
    /// This is called once during handle_prompt to enable permission requests.
    /// The OnceLock ensures it's only set once even if called multiple times.
    pub fn set_connection_cx(&self, cx: Cx) {
        if self.connection_cx_lock.get().is_none() {
            drop(self.connection_cx_lock.set(cx));
        }
    }

    /// Get the connection context if available
    ///
    /// Returns None if called before handle_prompt sets the connection.
    pub fn get_connection_cx(&self) -> Option<&Cx> {
        self.connection_cx_lock.get()
    }

    /// Get cached tool_use_id for a tool_input
    ///
    /// Called by can_use_tool callback to get tool_use_id when CLI doesn't provide it.
    /// Returns the tool_use_id if cached, None otherwise.
    /// Removes the entry from cache after retrieval (one-time use).
    pub fn get_cached_tool_use_id(&self, tool_input: &serde_json::Value) -> Option<String> {
        let key = stable_cache_key(tool_input);
        self.tool_use_id_cache.remove(&key).map(|(_, v)| v)
    }

    /// Connect to Claude CLI
    ///
    /// This spawns the Claude CLI process and establishes JSON-RPC communication.
    #[instrument(
        name = "session_connect",
        skip(self),
        fields(session_id = %self.session_id)
    )]
    /// The readiness lock; see [`Session::ready`].
    pub fn ready(&self) -> &tokio::sync::Mutex<bool> {
        &self.ready
    }

    /// Check if the session is connected
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Get read access to the client
    pub async fn client(&self) -> tokio::sync::RwLockReadGuard<'_, ClaudeClient> {
        self.client.read().await
    }

    /// Get write access to the client
    pub async fn client_mut(&self) -> tokio::sync::RwLockWriteGuard<'_, ClaudeClient> {
        self.client.write().await
    }

    /// Cancel this session and interrupt the Claude CLI
    ///
    /// This sends an interrupt signal to the Claude CLI to stop the current operation.
    /// Also cancels the running turn, so that its stop reason is `Cancelled`.
    #[instrument(
        name = "session_cancel",
        skip(self),
        fields(session_id = %self.session_id)
    )]
    pub async fn cancel(&self) {
        self.gate.cancel();
        if let Some(tools) = &self.host_tools {
            tools.cancel_all();
        }

        tracing::info!(
            session_id = %self.session_id,
            "Sending interrupt signal to Claude CLI"
        );

        // Send interrupt signal to Claude CLI to stop current operation
        if let Ok(client) = self.client.try_read() {
            if let Err(e) = client.interrupt().await {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to send interrupt signal to Claude CLI"
                );
            } else {
                tracing::info!(
                    session_id = %self.session_id,
                    "Interrupt signal sent to Claude CLI"
                );
            }
        } else {
            tracing::warn!(
                session_id = %self.session_id,
                "Could not acquire client lock for interrupt"
            );
        }
    }

    /// Get the permission handler
    pub async fn permission(&self) -> tokio::sync::RwLockReadGuard<'_, PermissionHandler> {
        self.permission.read().await
    }

    /// Get the current permission mode
    pub async fn permission_mode(&self) -> PermissionMode {
        self.permission.read().await.mode()
    }

    /// Set the permission mode
    ///
    /// Updates the PermissionHandler. The hook will read the mode
    /// from the same PermissionHandler, ensuring consistency.
    pub async fn set_permission_mode(&self, mode: PermissionMode) {
        // Update the permission handler (single source of truth)
        self.permission.write().await.set_mode(mode);

        tracing::info!(
            session_id = %self.session_id,
            mode = mode.as_str(),
            "Permission mode updated"
        );
    }

    /// Send session/update notification for permission mode change
    ///
    /// This sends a CurrentModeUpdate notification to the client to inform it
    /// that the permission mode has changed. This is used for ExitPlanMode to
    /// notify the UI that the mode has been switched.
    pub fn send_mode_update(&self, mode: &str) {
        let Some(connection_cx) = self.get_connection_cx() else {
            tracing::warn!(
                session_id = %self.session_id,
                mode = %mode,
                "Connection not ready for mode update notification"
            );
            return;
        };

        let mode_update = CurrentModeUpdate::new(SessionModeId::new(mode));
        let notification = SessionNotification::new(
            SessionId::new(self.session_id.clone()),
            SessionUpdate::CurrentModeUpdate(mode_update),
        );

        if let Err(e) = connection_cx.send_notification(notification) {
            tracing::warn!(
                session_id = %self.session_id,
                mode = %mode,
                error = %e,
                "Failed to send CurrentModeUpdate notification"
            );
        } else {
            tracing::info!(
                session_id = %self.session_id,
                mode = %mode,
                "Sent CurrentModeUpdate notification"
            );
        }
    }

    /// The models the session's CLI reported, once it has started.
    pub fn reported_models(&self) -> Vec<serde_json::Value> {
        self.reported_models
            .read()
            .map(|models| models.clone())
            .unwrap_or_default()
    }

    /// Keep what the CLI reported about the account's models.
    pub fn set_reported_models(&self, models: Vec<serde_json::Value>) {
        if let Ok(mut reported) = self.reported_models.write() {
            *reported = models;
        }
    }

    /// Get the current model ID
    pub async fn current_model(&self) -> Option<String> {
        self.current_model.read().await.clone()
    }

    /// Choose the session's model. It is the session's model at once (the options say so, and
    /// every CLI started from now on launches with it); the running CLI is told in the background,
    /// so the choice never waits on a busy or silent CLI.
    pub async fn set_model(self: &Arc<Self>, model_id: String) {
        // Update session model
        {
            let mut model = self.current_model.write().await;
            *model = Some(model_id.clone());
        } // Release write lock before accessing client

        tracing::info!(model_id = %model_id, "Session model updated");

        // Tell the running CLI now if it can be told, otherwise on the next prompt.
        self.model_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let session = Arc::clone(self);
        tokio::spawn(async move { session.apply_pending_model().await });
    }

    /// Send a chosen model to the CLI if it has not had it yet.
    ///
    /// Called right after a query is sent, because the CLI only takes a model change once a query
    /// exists; a failure leaves it pending for the next prompt.
    pub async fn apply_pending_model(&self) {
        if !self
            .model_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let Some(model_id) = self.current_model().await else {
            return;
        };
        let client = self.client.read().await;
        // "default" is the CLI's own choice, which is expressed by not naming one.
        let requested = (model_id != "default").then_some(model_id.as_str());
        match client.set_model(requested).await {
            Ok(()) => tracing::info!(model_id = %model_id, "Model applied to the CLI"),
            Err(error) => {
                tracing::debug!(%error, "Model not applied yet; it waits for the next prompt");
                self.model_pending
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Whether Fast mode is chosen for this session.
    pub fn fast_mode(&self) -> bool {
        self.fast_mode.chosen.load(Ordering::SeqCst)
    }

    /// Choose Fast mode, telling the running CLI in the background if it can be told, otherwise
    /// on the next prompt.
    pub fn set_fast_mode(self: &Arc<Self>, enabled: bool) {
        self.fast_mode.chosen.store(enabled, Ordering::SeqCst);
        self.fast_mode.pending.store(true, Ordering::SeqCst);
        let session = Arc::clone(self);
        tokio::spawn(async move { session.apply_pending_fast_mode().await });
    }

    /// Send the chosen Fast mode to the CLI if it has not had it yet.
    ///
    /// Called right after a query is sent, like the model; a failure leaves it pending.
    pub async fn apply_pending_fast_mode(&self) {
        if !self.fast_mode.pending.swap(false, Ordering::SeqCst) {
            return;
        }
        let enabled = self.fast_mode();
        let client = self.client.read().await;
        match client
            .apply_flag_settings(serde_json::json!({ "fastMode": enabled }))
            .await
        {
            Ok(()) => tracing::info!(enabled, "Fast mode applied to the CLI"),
            Err(error) => {
                tracing::debug!(%error, "Fast mode not applied yet; it waits for the next prompt");
                self.fast_mode.pending.store(true, Ordering::SeqCst);
            }
        }
    }

    /// What this session has used and what its rate-limit windows say.
    pub fn usage(&self) -> &UsageLedger {
        &self.usage
    }

    /// Get the notification converter (read-only access)
    pub async fn converter(&self) -> tokio::sync::RwLockReadGuard<'_, NotificationConverter> {
        self.converter.read().await
    }

    /// Set the request_id on the notification converter
    ///
    /// This will attach the request_id to all SessionNotification instances
    /// created by this session's converter, allowing clients to track which
    /// responses correspond to which requests.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique request identifier
    pub async fn set_converter_request_id(&self, request_id: String) {
        let mut converter = self.converter.write().await;
        converter.set_request_id(request_id);
    }

    /// Clear the request_id from the notification converter
    pub async fn clear_converter_request_id(&self) {
        let mut converter = self.converter.write().await;
        converter.clear_request_id();
    }

    /// Clear the tool use cache from the notification converter
    ///
    /// Should be called at the end of each prompt to prevent unbounded
    /// memory growth from accumulated tool use entries.
    pub async fn clear_converter_cache(&self) {
        let converter = self.converter.write().await;
        converter.clear_cache();
    }

    /// Configure the ACP MCP server with connection and terminal client
    ///
    /// This should be called after creating the session to enable Terminal API
    /// integration for Bash commands.
    pub async fn configure_acp_server(
        &self,
        connection_cx: Cx,
        terminal_client: Option<Arc<TerminalClient>>,
    ) {
        self.acp_mcp_server.set_session_id(&self.session_id);
        self.acp_mcp_server.set_connection(connection_cx);
        self.acp_mcp_server.set_cwd(self.cwd.clone());
        self.acp_mcp_server
            .set_background_processes(self.background_processes.clone());
        self.acp_mcp_server
            .set_permission_checker(self.permission_checker.clone());

        if let Some(client) = terminal_client {
            self.acp_mcp_server.set_terminal_client(client);
        }

        // Set up cancel callback to interrupt Claude CLI when MCP cancellation is received
        let session_id = self.session_id.clone();
        let gate = Arc::clone(&self.gate);
        let host_tools = self.host_tools.clone();

        self.acp_mcp_server
            .set_cancel_callback(move || {
                tracing::info!(
                    session_id = %session_id,
                    "MCP cancel callback invoked, cancelling the running turn"
                );
                if let Some(tools) = &host_tools {
                    tools.cancel_all();
                }
                // The prompt loop sees the cancelled turn and interrupts the CLI.
                gate.cancel();
            })
            .await;
    }
}

/// The `claude` binary named by `CLAUDE_CODE_EXECUTABLE`, if one is set.
fn configured_cli_path() -> Option<PathBuf> {
    cli_path_from(std::env::var_os("CLAUDE_CODE_EXECUTABLE"))
}

fn cli_path_from(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|path| !path.is_empty()).map(PathBuf::from)
}

pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("connected", &self.connected.load(Ordering::Relaxed))
            .finish()
    }
}

#[cfg(test)]
mod tests;
