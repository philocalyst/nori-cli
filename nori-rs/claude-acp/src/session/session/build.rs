//! Creating a session: the CLI's options, hooks, permission state and tool servers.

use super::*;

impl Session {
    /// Create a new session and wrap in Arc
    ///
    /// Returns Arc<Self> because the can_use_tool callback needs Arc<Session>.
    /// We use OnceLock to break the circular dependency between Session and callback.
    ///
    /// # Arguments
    ///
    /// * `session_id` - Unique identifier for this session
    /// * `cwd` - Working directory
    /// * `config` - Agent configuration from environment
    /// * `meta` - Session metadata from the new session request
    #[instrument(
        name = "session_create",
        skip(config, meta),
        fields(
            session_id = %session_id,
            cwd = ?cwd,
            has_meta = meta.is_some(),
        )
    )]
    pub fn new(
        session_id: String,
        cwd: PathBuf,
        config: &AgentConfig,
        meta: Option<&NewSessionMeta>,
        tools: &SessionTools,
    ) -> Result<Arc<Self>> {
        let start_time = Instant::now();

        tracing::info!(
            session_id = %session_id,
            cwd = ?cwd,
            "Creating new session"
        );

        // Create permission checker for hooks
        // Load settings from the login's settings.json (an account's CLAUDE_CONFIG_DIR, else
        // ~/.claude), then .claude/settings.json and .claude/settings.local.json in the project.
        let config_dir = crate::utils::claude_config_dir(&config.environment);
        let settings_manager = SettingsManager::new(&cwd, config_dir.as_deref())
            .unwrap_or_else(|e| {
                tracing::warn!("Failed to load settings manager from cwd: {}. Using default settings.", e);
                // Fallback: try to load settings from home directory
                if let Some(home) = dirs::home_dir() {
                    tracing::info!("Attempting to load settings from home directory");
                    SettingsManager::new(&home, config_dir.as_deref()).unwrap_or_else(|e2| {
                        tracing::error!("Failed to load settings from home directory: {}. Using minimal default settings.", e2);
                        // Last resort: create a manager with minimal settings
                        SettingsManager::new_with_settings(crate::settings::Settings::default())
                    })
                } else {
                    tracing::error!("No home directory found. Using minimal default settings.");
                    SettingsManager::new_with_settings(crate::settings::Settings::default())
                }
            });
        // Create shared permission checker that will be used by both hook and permission handler
        // This ensures that runtime rule changes (e.g., "Always Allow") are reflected in both places
        let permission_checker = Arc::new(RwLock::new(PermissionChecker::new(
            settings_manager.settings().clone(),
            &cwd,
        )));

        // Create PermissionHandler with shared PermissionChecker
        // This ensures both pre_tool_use_hook and can_use_tool callback use the same rules
        // PermissionHandler uses AcceptEdits mode (compatible with root, allows all tools)
        let permission_handler = Arc::new(RwLock::new(PermissionHandler::with_checker(
            permission_checker.clone(),
            config_dir.map(|dir| dir.join("plans")).unwrap_or_default(),
        )));

        // Create shared connection_cx_lock for hook permission requests
        let connection_cx_lock: Arc<OnceLock<Cx>> = Arc::new(OnceLock::new());

        // Create shared tool_use_id_cache for hook-to-callback tool_use_id passing
        // PreToolUse hook caches tool_use_id when Ask decision is made
        // can_use_tool callback uses this when CLI doesn't provide tool_use_id
        let tool_use_id_cache: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        // What the CLI's own editing tools are about to change, shared by the hook that reads it
        // and the converter that reports the diff.
        let edit_snapshots = Arc::new(EditSnapshots::new(cwd.clone()));

        // Create hooks with shared permission checker and handler
        let pre_tool_use_hook = create_pre_tool_use_hook(
            connection_cx_lock.clone(),
            session_id.clone(),
            Some(permission_checker.clone()),
            permission_handler.clone(),
            tool_use_id_cache.clone(),
            edit_snapshots.clone(),
        );

        // Build hooks map
        let mut hooks_map: HashMap<HookEvent, Vec<HookMatcher>> = HashMap::new();
        hooks_map.insert(
            HookEvent::PreToolUse,
            vec![
                HookMatcher::builder()
                    .hooks(vec![pre_tool_use_hook])
                    .build(),
            ],
        );

        // Create OnceLock for storing Arc<Session> (needed for callback)
        let session_lock: Arc<OnceLock<Arc<Session>>> = Arc::new(OnceLock::new());

        // Create ACP MCP server
        let acp_mcp_server = Arc::new(AcpMcpServer::new("acp", env!("CARGO_PKG_VERSION")));

        // Create background process manager
        let background_processes = Arc::new(BackgroundProcessManager::new());

        // The client's servers and Nori's host tools, then our ACP server, which no client server
        // may replace.
        let mut mcp_servers_dict = tools.cli_servers(&["acp"]);
        mcp_servers_dict.insert(
            "acp".to_string(),
            McpServerConfig::Sdk(McpSdkServerConfig {
                name: "acp".to_string(),
                instance: acp_mcp_server.clone(),
            }),
        );

        tracing::info!(
            session_id = %session_id,
            mcp_server_count = mcp_servers_dict.len(),
            "MCP servers configured"
        );

        // Create can_use_tool callback with OnceLock<Session>
        let can_use_tool_callback = create_can_use_tool_callback(session_lock.clone());

        // Nori never asks permission, so Claude bypasses its checks. The CLI refuses that mode as
        // root; there it accepts edits, and the permission hooks allow the rest.
        #[cfg(unix)]
        // SAFETY: geteuid has no preconditions and cannot fail.
        let as_root = unsafe { libc::geteuid() } == 0;
        #[cfg(not(unix))]
        let as_root = false;
        let permission_mode = if as_root {
            SdkPermissionMode::AcceptEdits
        } else {
            SdkPermissionMode::BypassPermissions
        };
        let mut options = ClaudeAgentOptions::builder()
            .cwd(cwd.clone())
            .hooks(hooks_map)
            .mcp_servers(McpServers::Dict(mcp_servers_dict))
            .can_use_tool(can_use_tool_callback)
            .permission_mode(permission_mode)
            // Using circular buffer (ringbuf) - auto-recycles old data, no need for large buffer
            .max_buffer_size(20 * 1024 * 1024) // 20MB 缓冲区
            // 允许读取用户级 settings (for API keys, etc.)
            // Agent's env variables (via options.env) will still take precedence
            .setting_sources(vec![SettingSource::User])
            // Fetching a CLI behind the user's back is surprising and makes a run
            // depend on the network, so it is opt-in.
            .auto_download_cli(env_flag("CLAUDE_ACP_AUTO_DOWNLOAD_CLI"))
            // The version probe is another launch of the CLI before the real one; the user's
            // `claude` is what they chose to run, so trust it.
            .skip_version_check(true)
            .build();

        // Which Claude is used is how an account is chosen: a wrapper points
        // `CLAUDE_CODE_EXECUTABLE` at a `claude` that carries its own config
        // directory. The SDK only reads `CLAUDE_CLI_PATH`, so honour the name
        // clients already use.
        options.cli_path = config.cli_path.clone().or_else(configured_cli_path);

        // Debug: Verify can_use_tool is set
        tracing::info!(
            session_id = %session_id,
            has_can_use_tool = options.can_use_tool.is_some(),
            has_hooks = options.hooks.is_some(),
            "Options configured after build"
        );

        // Verify mcp_servers is set correctly
        match &options.mcp_servers {
            McpServers::Dict(dict) => {
                tracing::debug!(
                    session_id = %session_id,
                    servers = ?dict.keys().collect::<Vec<_>>(),
                    "MCP servers registered"
                );
            }
            McpServers::Empty => {
                tracing::warn!(
                    session_id = %session_id,
                    "MCP servers is Empty - this is unexpected!"
                );
            }
            McpServers::Path(p) => {
                tracing::warn!(
                    session_id = %session_id,
                    path = ?p,
                    "MCP servers is Path - this is unexpected!"
                );
            }
        }

        // Configure ACP tools to replace CLI built-in tools
        // This disables CLI's built-in tools and enables our MCP tools with mcp__acp__ prefix
        let acp_tools = get_acp_replacement_tools();
        options.use_acp_tools(&acp_tools);

        // Enable streaming to receive incremental content updates
        // This allows SDK to send StreamEvent messages with content_block_delta
        options.include_partial_messages = true;

        tracing::debug!(
            session_id = %session_id,
            acp_tools = ?acp_tools,
            disallowed_tools = ?options.disallowed_tools,
            allowed_tools = ?options.allowed_tools,
            "ACP tools configured"
        );

        // Apply config from environment
        config.apply_to_options(&mut options);

        tracing::debug!(
            session_id = %session_id,
            model = ?options.model,
            fallback_model = ?options.fallback_model,
            max_thinking_tokens = ?options.max_thinking_tokens,
            base_url = ?config.base_url,
            api_key = ?config.masked_api_key(),
            env_vars_count = options.env.len(),
            "Agent config applied"
        );

        // Apply meta options if provided
        if let Some(meta) = meta {
            // Set system prompt: replace takes priority over append
            if let Some(replace) = meta.get_system_prompt_replace() {
                // Complete replacement of system prompt
                options.system_prompt = Some(SystemPrompt::Text(replace.to_string()));
                tracing::info!(
                    session_id = %session_id,
                    prompt_len = replace.len(),
                    "Using custom system prompt from meta (replace)"
                );
            } else if let Some(append) = meta.get_system_prompt_append() {
                // Append to default claude_code preset
                let preset = SystemPromptPreset::with_append("claude_code", append);
                options.system_prompt = Some(SystemPrompt::Preset(preset));
                tracing::info!(
                    session_id = %session_id,
                    append_len = append.len(),
                    "Appending to system prompt from meta"
                );
            }

            // Set resume session if provided
            if let Some(resume_id) = meta.get_resume_session_id() {
                options.resume = Some(resume_id.to_string());
                tracing::info!(
                    session_id = %session_id,
                    resume_session_id = %resume_id,
                    "Resuming from previous session"
                );
            }

            // Set fork_session flag if this is a fork operation
            if meta.fork_session {
                options.fork_session = true;
                tracing::info!(
                    session_id = %session_id,
                    "Forking session (fork_session=true)"
                );
            }

            // Set max thinking tokens if provided (enables extended thinking mode)
            if let Some(tokens) = meta.get_max_thinking_tokens() {
                options.max_thinking_tokens = Some(tokens);
                tracing::info!(
                    session_id = %session_id,
                    max_thinking_tokens = tokens,
                    "Extended thinking mode enabled via meta"
                );
            }
        }

        // A new conversation takes the session's id, so loading or resuming it by that id later
        // finds it, and so does a fork: the CLI saves the copy under the id it is given. A
        // resumed one keeps the id it was saved under.
        if options.resume.is_none() || options.fork_session {
            options
                .extra_args
                .insert("session-id".to_string(), Some(session_id.clone()));
        }

        // Create the client
        let launch_model = options.model.clone();
        let client = ClaudeClient::new(options);

        let elapsed = start_time.elapsed();
        tracing::info!(
            session_id = %session_id,
            elapsed_ms = elapsed.as_millis(),
            "Session created successfully"
        );

        // Clone cwd for converter before moving cwd into the struct
        let cwd_for_converter = cwd.clone();

        // Build the Session struct
        let session = Self {
            session_id,
            cwd,
            client: RwLock::new(client),
            permission: permission_handler,
            usage: UsageLedger::default(),
            converter: RwLock::new(
                NotificationConverter::with_cwd(cwd_for_converter)
                    .with_edit_snapshots(edit_snapshots),
            ),
            connected: AtomicBool::new(false),
            ready: tokio::sync::Mutex::new(false),
            permission_checker,
            current_model: tokio::sync::RwLock::new(launch_model),
            reported_models: std::sync::RwLock::new(Vec::new()),
            model_pending: std::sync::atomic::AtomicBool::new(false),
            fast_mode: FastModeState::default(),
            acp_mcp_server,
            host_tools: tools.host_tools.clone(),
            background_processes,
            connection_cx_lock,
            tool_use_id_cache,
            gate: Arc::new(TurnGate::new(TurnPolicy::Steerable)),
            turn_order: Arc::default(),
        };

        // Wrap in Arc
        let session_arc = Arc::new(session);

        // Set the OnceLock so the callback can access the Session
        drop(session_lock.set(session_arc.clone()));

        Ok(session_arc)
    }
}
