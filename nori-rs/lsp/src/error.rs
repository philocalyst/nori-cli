//! Errors of the language-server layer. Every variant renders as a sentence a model can act on.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum LspError {
    #[error(
        "no language server is configured for {path}{hint}",
        path = path.display()
    )]
    NoServer { path: PathBuf, hint: String },
    #[error("language server {server} is not available: {reason}")]
    ServerUnavailable { server: String, reason: String },
    #[error("language server {server} is still starting; try again in a few seconds")]
    ServerStarting { server: String },
    #[error("language server {server} does not support {feature}")]
    Unsupported { server: String, feature: String },
    #[error("language server {server} did not answer {request} within {seconds} s")]
    Timeout {
        server: String,
        request: String,
        seconds: i64,
    },
    #[error("language server {server} failed {request}: {message}")]
    Request {
        server: String,
        request: String,
        message: String,
    },
    #[error("{message}")]
    InvalidPosition { message: String },
    #[error("cannot read {path}: {message}", path = path.display())]
    Io { path: PathBuf, message: String },
    #[error("the language-server manager was shut down")]
    ShutDown,
}

pub type LspResult<T> = Result<T, LspError>;
