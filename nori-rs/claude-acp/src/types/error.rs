//! Errors: this crate reports with the kit's [`AgentError`], which the host categorizes.

use claude_code_agent_sdk::ClaudeError;
pub use nori_acp_kit::AgentError;

/// Result type for the agent.
pub type Result<T> = std::result::Result<T, AgentError>;

/// A Claude SDK failure as the error the host understands.
///
/// - No `claude` to run is a missing executable, which the host reports with the install hint.
/// - A CLI that exited mid-conversation says how it ended and that the next message carries on:
///   the next query starts a CLI that resumes the conversation.
/// - A CLI that cannot be reached or stays silent is an unreachable backend.
/// - Anything else is classified by what the CLI said (a login problem, a rate limit, an
///   overload, a provider's JSON body read for its sentence), and stays an internal error when
///   it says nothing the host's policy can act on.
pub fn from_sdk(error: ClaudeError) -> AgentError {
    match error {
        ClaudeError::CliNotFound(missing) => AgentError::ExecutableNotFound(missing.message),
        ClaudeError::CliExited(exit) => AgentError::Internal(format!(
            "{exit}. Your next message restarts it and continues this conversation."
        )),
        ClaudeError::Connection(_) | ClaudeError::Transport(_) | ClaudeError::Timeout(_) => {
            AgentError::Unreachable(error.to_string())
        }
        ClaudeError::Io(error) => error.into(),
        other => AgentError::provider(None, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_code_agent_sdk::CliExitError;
    use claude_code_agent_sdk::CliNotFoundError;
    use nori_acp_kit::acp;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn data(error: AgentError) -> serde_json::Value {
        acp::Error::from(error).data.unwrap_or_default()
    }

    /// Fault `cli-missing`: a missing `claude` is not an unreachable service.
    #[test]
    fn a_missing_cli_is_an_executable_that_is_not_installed() {
        let error = from_sdk(ClaudeError::CliNotFound(CliNotFoundError::new(
            "Claude Code CLI not found at /nowhere/claude.",
            None,
        )));
        assert_eq!(
            data(error),
            json!({
                "detail": "Claude Code CLI not found at /nowhere/claude.",
                "code": "executable_not_found",
            })
        );
    }

    /// Fault `stream-eof`: the person reads how the CLI ended and what happens next.
    #[test]
    fn a_cli_that_exited_says_how_and_that_the_conversation_goes_on() {
        let error = from_sdk(ClaudeError::CliExited(CliExitError {
            exit_code: Some(1),
            stderr_tail: vec!["warming up".to_owned(), "Error: out of memory".to_owned()],
        }));
        assert_eq!(
            data(error),
            json!({
                "detail": "Claude Code stopped unexpectedly (exit status 1): Error: out of memory. Your next message restarts it and continues this conversation.",
            })
        );
    }
}
