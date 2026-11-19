//! Errors: this crate reports with the kit's [`AgentError`], which the host categorizes.

use claude_code_agent_sdk::ClaudeError;
pub use nori_acp_kit::AgentError;

/// Result type for the agent.
pub type Result<T> = std::result::Result<T, AgentError>;

/// A Claude SDK failure as the error the host understands. A failure to reach the CLI is an
/// unreachable backend; anything else is classified by what the CLI said (a login problem,
/// a rate limit, an overload), and stays an internal error when it says nothing the host's
/// policy can act on.
pub fn from_sdk(error: ClaudeError) -> AgentError {
    match error {
        ClaudeError::Connection(_) | ClaudeError::Transport(_) | ClaudeError::Timeout(_) => {
            AgentError::Unreachable(error.to_string())
        }
        ClaudeError::Io(error) => error.into(),
        other => AgentError::provider(None, other.to_string()),
    }
}
