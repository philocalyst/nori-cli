//! Error types for the Claude Agent SDK

use std::path::PathBuf;
use thiserror::Error;

/// Main error type for the Claude Agent SDK
#[derive(Debug, Error)]
pub enum ClaudeError {
    /// CLI connection error
    #[error("CLI connection error: {0}")]
    Connection(#[from] ConnectionError),

    /// Process error
    #[error("Process error: {0}")]
    Process(#[from] ProcessError),

    /// JSON decode error
    #[error("JSON decode error: {0}")]
    JsonDecode(#[from] JsonDecodeError),

    /// Message parse error
    #[error("Message parse error: {0}")]
    MessageParse(#[from] MessageParseError),

    /// Transport error
    #[error("Transport error: {0}")]
    Transport(String),

    /// Control protocol error
    #[error("Control protocol error: {0}")]
    ControlProtocol(String),

    /// Timeout error
    #[error("Timeout: {0}")]
    Timeout(String),

    /// Invalid configuration
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// CLI not found error
    #[error("CLI not found: {0}")]
    CliNotFound(#[from] CliNotFoundError),

    /// The CLI process ended (or closed its output) while the SDK was still talking to it.
    /// Requests to it fail at once with this instead of waiting for a timeout.
    #[error("{0}")]
    CliExited(CliExitError),

    /// Image validation error
    #[error("Image validation error: {0}")]
    ImageValidation(#[from] ImageValidationError),

    /// MCP error with JSONRPC error code
    #[error("MCP error: {0}")]
    Mcp(#[from] McpError),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Other errors
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Error when Claude Code CLI cannot be found
#[derive(Debug, Error)]
#[error("CLI not found: {message}")]
pub struct CliNotFoundError {
    /// Error message
    pub message: String,
    /// Path that was checked
    pub cli_path: Option<PathBuf>,
}

impl CliNotFoundError {
    /// Create a new CLI not found error
    pub fn new(message: impl Into<String>, cli_path: Option<PathBuf>) -> Self {
        Self {
            message: message.into(),
            cli_path,
        }
    }
}

/// How a Claude Code CLI process ended: its exit status, when it could be collected, and the last
/// lines it wrote to standard error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliExitError {
    /// The process's exit code; `None` when it was killed by a signal or is still unreaped.
    pub exit_code: Option<i32>,
    /// The last lines of its standard error, oldest first.
    pub stderr_tail: Vec<String>,
}

impl std::fmt::Display for CliExitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Claude Code stopped unexpectedly")?;
        if let Some(code) = self.exit_code {
            write!(formatter, " (exit status {code})")?;
        }
        // The last thing it said is usually why.
        if let Some(line) = self
            .stderr_tail
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
        {
            write!(formatter, ": {line}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CliExitError {}

/// Error when connecting to Claude Code CLI
#[derive(Debug, Error)]
#[error("Connection error: {message}")]
pub struct ConnectionError {
    /// Error message
    pub message: String,
}

impl ConnectionError {
    /// Create a new connection error
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Error when the CLI process fails
#[derive(Debug, Error)]
#[error("Process error (exit code {exit_code:?}): {message}")]
pub struct ProcessError {
    /// Error message
    pub message: String,
    /// Process exit code
    pub exit_code: Option<i32>,
    /// stderr output
    pub stderr: Option<String>,
}

impl ProcessError {
    /// Create a new process error
    pub fn new(message: impl Into<String>, exit_code: Option<i32>, stderr: Option<String>) -> Self {
        Self {
            message: message.into(),
            exit_code,
            stderr,
        }
    }
}

/// Error when JSON decoding fails
#[derive(Debug, Error)]
#[error("JSON decode error: {message}")]
pub struct JsonDecodeError {
    /// Error message
    pub message: String,
    /// The line that failed to decode
    pub line: String,
}

impl JsonDecodeError {
    /// Create a new JSON decode error
    pub fn new(message: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: line.into(),
        }
    }
}

/// Error when message parsing fails
#[derive(Debug, Error)]
#[error("Message parse error: {message}")]
pub struct MessageParseError {
    /// Error message
    pub message: String,
    /// The data that failed to parse
    pub data: Option<serde_json::Value>,
}

impl MessageParseError {
    /// Create a new message parse error
    pub fn new(message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        Self {
            message: message.into(),
            data,
        }
    }
}

/// Image validation error
#[derive(Debug, Error)]
#[error("Image validation error: {message}")]
pub struct ImageValidationError {
    /// Error message
    pub message: String,
}

impl ImageValidationError {
    /// Create a new image validation error
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// MCP (Model Context Protocol) error with JSONRPC error code
#[derive(Debug, Error)]
#[error("MCP error ({code}): {message}")]
pub struct McpError {
    /// JSONRPC error code
    pub code: i32,
    /// Error message
    pub message: String,
}

impl McpError {
    /// Create a new MCP error
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Method not found error (-32601)
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("Method not found: {}", method))
    }

    /// Internal error (-32603)
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    /// Invalid params error (-32602)
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }
}

/// Result type for the Claude Agent SDK
pub type Result<T> = std::result::Result<T, ClaudeError>;
