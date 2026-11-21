use strum::IntoEnumIterator;
use strum_macros::AsRefStr;
use strum_macros::EnumIter;
use strum_macros::EnumString;
use strum_macros::IntoStaticStr;

/// Where a builtin command can run: on the local machine only, only against a
/// cloud (remote-VM) session, or anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    /// Operates on the local machine; meaningless when the agent runs on a
    /// remote VM (cloud session).
    LocalOnly,
    /// Only meaningful against a cloud session (needs `session/close`).
    CloudOnly,
    /// Meaningful for every session type.
    Universal,
}

/// Commands that can be invoked by starting a message with a leading slash.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, EnumIter, AsRefStr, IntoStaticStr,
)]
#[strum(serialize_all = "kebab-case")]
pub enum SlashCommand {
    // DO NOT ALPHA-SORT! Enum order is presentation order in the popup, so
    // more frequently used commands should be listed first.
    Model,
    Providers,
    Config,
    Approvals,
    Settings,
    Vim,
    Goal,
    New,
    Resume,
    Reference,
    Close,
    Init,
    Compact,
    Undo,
    Continue,
    Browse,
    Diff,
    Mention,
    Status,
    Context,
    Usage,
    RemoteControl,
    Memory,
    FirstPrompt,
    Mcp,
    Login,
    Logout,
    Quit,
    Exit,
    SwitchSkillset,
    Fork,
    Browser,
    Move,
}

impl SlashCommand {
    /// User-visible description shown in the popup.
    pub fn description(self) -> &'static str {
        match self {
            SlashCommand::Providers => "connect model providers",
            SlashCommand::New => "start a new chat during a conversation",
            SlashCommand::Resume => "resume a previous session",
            SlashCommand::Reference => "search past chats and reference one in your draft",
            SlashCommand::Close => "close (release) the current session and start a fresh chat",
            SlashCommand::Init => "create an AGENTS.md file with instructions for Nori",
            SlashCommand::Compact => "summarize conversation to prevent hitting the context limit",
            SlashCommand::Undo => "ask Nori to undo a turn",
            SlashCommand::Continue => "continue the last turn where it stopped",
            SlashCommand::Quit | SlashCommand::Exit => "exit Nori",
            SlashCommand::Browse => "open a file manager to browse and edit files",
            SlashCommand::Diff => "show git diff (including untracked files)",
            SlashCommand::Mention => "mention a file",
            SlashCommand::Status => "show current session configuration and context window usage",
            SlashCommand::Context => "visualize context usage and inspect local instructions",
            SlashCommand::Usage => "token and cost totals by provider and model, and quota left",
            SlashCommand::RemoteControl => "manage remote ACP access to this running session",
            SlashCommand::Memory => "show the contents of all active instruction files",
            SlashCommand::FirstPrompt => "show the first prompt from this session",
            SlashCommand::Model => "choose a model",
            SlashCommand::Config => "configure ACP agent settings (if exposed by the agent)",
            SlashCommand::Approvals => "choose what Nori can do without approval",
            SlashCommand::Settings => "configure Nori CLI settings (theme, hotkeys, layout, …)",
            SlashCommand::Vim => "configure Vim mode and Enter key behavior",
            SlashCommand::Goal => "set or view the goal for a long-running task",
            SlashCommand::Mcp => "manage MCP server connections",
            SlashCommand::Login => "log in to the current agent",
            SlashCommand::Logout => "show logout instructions",
            SlashCommand::SwitchSkillset => "switch between available skillsets",
            SlashCommand::Fork => "rewind conversation to a previous message",
            SlashCommand::Browser => "open a Chrome browser the agent can control via CDP",
            SlashCommand::Move => "move this whole session to another of your machines",
        }
    }

    /// Command string without the leading '/'. Provided for compatibility with
    /// existing code that expects a method named `command()`.
    pub fn command(self) -> &'static str {
        self.into()
    }

    /// Whether this command can be run while a task is in progress. Changing how the session
    /// runs (its model, effort, mode, settings, approvals, servers, the agent or account it runs
    /// on) never waits for a turn: the change is accepted at once and applied at the next point
    /// where it can be (an agent switch when the response ends). What replaces or rewrites the
    /// conversation itself waits for the turn.
    pub fn available_during_task(self) -> bool {
        match self {
            SlashCommand::New
            | SlashCommand::Resume
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Undo
            | SlashCommand::Continue
            | SlashCommand::Login
            | SlashCommand::Logout
            | SlashCommand::SwitchSkillset
            | SlashCommand::Fork
            | SlashCommand::Close
            | SlashCommand::Browser => false,
            SlashCommand::Providers
            | SlashCommand::Model
            | SlashCommand::Config
            | SlashCommand::Approvals
            | SlashCommand::Settings
            | SlashCommand::Vim
            | SlashCommand::Mcp
            | SlashCommand::Browse
            | SlashCommand::Reference
            | SlashCommand::Diff
            | SlashCommand::Mention
            | SlashCommand::Status
            | SlashCommand::Context
            | SlashCommand::Usage
            | SlashCommand::RemoteControl
            | SlashCommand::Memory
            | SlashCommand::FirstPrompt
            | SlashCommand::Goal
            | SlashCommand::Quit
            | SlashCommand::Exit
            | SlashCommand::Move => true,
        }
    }

    /// Session-type scope of this command. LocalOnly commands are unavailable
    /// on cloud sessions; CloudOnly commands are unavailable elsewhere.
    /// `/quit` and `/exit` must stay Universal — they are never disabled.
    pub fn scope(self) -> CommandScope {
        match self {
            SlashCommand::Close => CommandScope::CloudOnly,
            SlashCommand::SwitchSkillset
            | SlashCommand::Browse
            | SlashCommand::Diff
            | SlashCommand::Browser
            | SlashCommand::Move => CommandScope::LocalOnly,
            SlashCommand::Providers
            | SlashCommand::Model
            | SlashCommand::Config
            | SlashCommand::Approvals
            | SlashCommand::Settings
            | SlashCommand::Vim
            | SlashCommand::Goal
            | SlashCommand::New
            | SlashCommand::Resume
            | SlashCommand::Reference
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Undo
            | SlashCommand::Continue
            | SlashCommand::Mention
            | SlashCommand::Status
            | SlashCommand::Context
            | SlashCommand::Usage
            | SlashCommand::RemoteControl
            | SlashCommand::Memory
            | SlashCommand::FirstPrompt
            | SlashCommand::Mcp
            | SlashCommand::Login
            | SlashCommand::Logout
            | SlashCommand::Quit
            | SlashCommand::Exit
            | SlashCommand::Fork => CommandScope::Universal,
        }
    }

    fn is_visible(self) -> bool {
        match self {
            SlashCommand::Approvals => false,
            #[cfg(not(feature = "login"))]
            SlashCommand::Logout => false,
            _ => true,
        }
    }
}

/// Return all built-in commands in a Vec paired with their command string.
pub fn built_in_slash_commands() -> Vec<(&'static str, SlashCommand)> {
    SlashCommand::iter()
        .filter(|command| command.is_visible())
        .map(|c| (c.command(), c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    #[cfg(not(feature = "login"))]
    fn logout_hidden_when_login_feature_disabled() {
        let commands = built_in_slash_commands();
        let has_logout = commands.iter().any(|(_, cmd)| *cmd == SlashCommand::Logout);
        assert!(
            !has_logout,
            "/logout should be hidden when login feature is disabled"
        );
    }

    #[test]
    #[cfg(feature = "login")]
    fn logout_visible_when_login_feature_enabled() {
        let commands = built_in_slash_commands();
        let has_logout = commands.iter().any(|(_, cmd)| *cmd == SlashCommand::Logout);
        assert!(
            has_logout,
            "/logout should be visible when login feature is enabled"
        );
    }

    #[test]
    fn agent_and_approval_commands_are_unavailable() {
        let commands = built_in_slash_commands();
        assert!(
            !commands
                .iter()
                .any(|(_, command)| *command == SlashCommand::Approvals)
        );
        assert!("agent".parse::<SlashCommand>().is_err());
        assert!(
            commands
                .iter()
                .any(|(_, command)| *command == SlashCommand::Providers)
        );
        assert!(
            commands
                .iter()
                .any(|(_, command)| *command == SlashCommand::Model)
        );
    }

    #[test]
    fn login_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_login = commands.iter().any(|(_, cmd)| *cmd == SlashCommand::Login);
        assert!(has_login, "/login should be visible in commands list");
    }

    #[test]
    fn login_has_description() {
        let desc = SlashCommand::Login.description();
        assert!(!desc.is_empty(), "/login should have a description");
    }

    #[test]
    fn login_not_available_during_task() {
        assert!(
            !SlashCommand::Login.available_during_task(),
            "/login should not be available while task is running"
        );
    }

    #[test]
    fn config_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_config = commands.iter().any(|(_, cmd)| *cmd == SlashCommand::Config);
        assert!(has_config, "/config should be visible in commands list");
    }

    #[test]
    fn config_has_description() {
        let desc = SlashCommand::Config.description();
        assert!(!desc.is_empty(), "/config should have a description");
    }

    #[test]
    fn changes_to_how_the_session_runs_never_wait_for_a_turn() {
        let mid_turn: Vec<SlashCommand> = [
            SlashCommand::Model,
            SlashCommand::Config,
            SlashCommand::Approvals,
            SlashCommand::Settings,
            SlashCommand::Vim,
            SlashCommand::Mcp,
            SlashCommand::Providers,
        ]
        .into_iter()
        .filter(|command| command.available_during_task())
        .collect();
        assert_eq!(mid_turn.len(), 7, "{mid_turn:?}");
    }

    #[test]
    fn settings_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_settings = commands
            .iter()
            .any(|(_, cmd)| *cmd == SlashCommand::Settings);
        assert!(has_settings, "/settings should be visible in commands list");
    }

    #[test]
    fn settings_has_description() {
        let desc = SlashCommand::Settings.description();
        assert!(!desc.is_empty(), "/settings should have a description");
    }

    #[test]
    fn config_serializes_to_kebab_config() {
        assert_eq!(SlashCommand::Config.command(), "config");
    }

    #[test]
    fn settings_serializes_to_kebab_settings() {
        assert_eq!(SlashCommand::Settings.command(), "settings");
    }

    #[test]
    fn vim_command_is_available_as_a_settings_shortcut() {
        assert_eq!(SlashCommand::Vim.command(), "vim");
        assert!(
            built_in_slash_commands()
                .iter()
                .any(|(_, command)| *command == SlashCommand::Vim)
        );
        assert_eq!(SlashCommand::Vim.scope(), CommandScope::Universal);
    }

    #[test]
    fn legacy_session_config_string_is_not_a_command() {
        // Guards the rename: the live ACP picker is now `/config`, not `/session-config`.
        assert!("session-config".parse::<SlashCommand>().is_err());
    }

    #[test]
    fn first_prompt_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_first_prompt = commands
            .iter()
            .any(|(_, cmd)| *cmd == SlashCommand::FirstPrompt);
        assert!(
            has_first_prompt,
            "/first-prompt should be visible in commands list"
        );
    }

    #[test]
    fn first_prompt_has_description() {
        let desc = SlashCommand::FirstPrompt.description();
        assert!(!desc.is_empty(), "/first-prompt should have a description");
    }

    #[test]
    fn first_prompt_available_during_task() {
        assert!(
            SlashCommand::FirstPrompt.available_during_task(),
            "/first-prompt should be available while task is running"
        );
    }

    #[test]
    fn switch_skillset_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_switch_skillset = commands
            .iter()
            .any(|(_, cmd)| *cmd == SlashCommand::SwitchSkillset);
        assert!(
            has_switch_skillset,
            "/switch-skillset should be visible in commands list"
        );
    }

    #[test]
    fn switch_skillset_has_description() {
        let desc = SlashCommand::SwitchSkillset.description();
        assert!(
            !desc.is_empty(),
            "/switch-skillset should have a description"
        );
    }

    #[test]
    fn switch_skillset_not_available_during_task() {
        assert!(
            !SlashCommand::SwitchSkillset.available_during_task(),
            "/switch-skillset should not be available while task is running"
        );
    }

    #[test]
    fn resume_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_resume = commands.iter().any(|(_, cmd)| *cmd == SlashCommand::Resume);
        assert!(has_resume, "/resume should be visible in commands list");
    }

    #[test]
    fn resume_has_description() {
        let desc = SlashCommand::Resume.description();
        assert!(!desc.is_empty(), "/resume should have a description");
    }

    #[test]
    fn resume_not_available_during_task() {
        assert!(
            !SlashCommand::Resume.available_during_task(),
            "/resume should not be available while task is running"
        );
    }

    #[test]
    fn browse_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_browse = commands.iter().any(|(_, cmd)| *cmd == SlashCommand::Browse);
        assert!(has_browse, "/browse should be visible in commands list");
    }

    #[test]
    fn browse_has_description() {
        let desc = SlashCommand::Browse.description();
        assert!(!desc.is_empty(), "/browse should have a description");
    }

    #[test]
    fn browse_available_during_task() {
        assert!(
            SlashCommand::Browse.available_during_task(),
            "/browse should be available while task is running"
        );
    }

    #[test]
    fn legacy_debug_commands_are_not_exposed() {
        let commands = built_in_slash_commands();

        assert!(
            commands.iter().all(|(name, _)| *name != "rollout"),
            "/rollout should not be visible in commands list"
        );
        assert!(
            commands.iter().all(|(name, _)| *name != "test-approval"),
            "/test-approval should not be visible in commands list"
        );
        assert!(
            "rollout".parse::<SlashCommand>().is_err(),
            "/rollout should not parse as a slash command"
        );
        assert!(
            "test-approval".parse::<SlashCommand>().is_err(),
            "/test-approval should not parse as a slash command"
        );
    }

    #[test]
    fn browse_parses_from_string() {
        let cmd: SlashCommand = "browse".parse().expect("/browse should parse from string");
        assert_eq!(cmd, SlashCommand::Browse);
    }

    #[test]
    fn resume_parses_from_string() {
        let cmd: SlashCommand = "resume".parse().expect("/resume should parse from string");
        assert_eq!(cmd, SlashCommand::Resume);
    }

    #[test]
    fn browser_parses_from_string() {
        let cmd: SlashCommand = "browser"
            .parse()
            .expect("/browser should parse from string");
        assert_eq!(cmd, SlashCommand::Browser);
    }

    #[test]
    fn browser_visible_in_commands() {
        let commands = built_in_slash_commands();
        let has_browser = commands
            .iter()
            .any(|(_, cmd)| *cmd == SlashCommand::Browser);
        assert!(has_browser, "/browser should be visible in commands list");
    }

    #[test]
    fn browser_not_available_during_task() {
        assert!(
            !SlashCommand::Browser.available_during_task(),
            "/browser should not be available while task is running"
        );
    }
}
