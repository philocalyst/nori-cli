// Modified for Nori. Derived from helix-lsp/src/lib.rs of github.com/helix-editor/helix (commit
// 9de242bfac2aac9dfd0e4496eb25190e85c24e69), Mozilla Public License 2.0 (see ../../LICENSE).
// Only the pieces the client and transport need are kept: the error type, offset encodings,
// server-to-client message parsing and a plain `LanguageServerId`. Helix's registry, progress map,
// rope-based edit conversion and workspace discovery are dropped. See ../../NORI.md.

mod client;
pub mod jsonrpc;
mod transport;

pub use client::Client;
pub use helix_lsp_types as lsp;
pub use jsonrpc::Call;
pub use lsp::{Position, Url};

use std::fmt;

use thiserror::Error;

pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Identifies one running language server process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LanguageServerId(pub i64);

impl fmt::Display for LanguageServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The server capabilities a request helper may check (what Helix called
/// `LanguageServerFeature`, minus the features Nori does not request).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LanguageServerFeature {
    Format,
    GotoDeclaration,
    GotoDefinition,
    GotoTypeDefinition,
    GotoReference,
    GotoImplementation,
    SignatureHelp,
    Hover,
    DocumentHighlight,
    Completion,
    CodeAction,
    DocumentLinks,
    WorkspaceCommand,
    DocumentSymbols,
    WorkspaceSymbols,
    Diagnostics,
    PullDiagnostics,
    RenameSymbol,
    InlayHints,
    DocumentColors,
    CallHierarchy,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("protocol error: {0}")]
    Rpc(#[from] jsonrpc::Error),
    #[error("failed to parse: {0}")]
    Parse(Box<dyn std::error::Error + Send + Sync>),
    #[error("IO Error: {0}")]
    IO(#[from] std::io::Error),
    #[error("request {0} timed out")]
    Timeout(jsonrpc::Id),
    #[error("server closed the stream")]
    StreamClosed,
    #[error("Unhandled")]
    Unhandled,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Parse(Box::new(value))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OffsetEncoding {
    /// UTF-8 code units aka bytes
    Utf8,
    /// UTF-32 code units aka chars
    Utf32,
    /// UTF-16 code units
    #[default]
    Utf16,
}

#[derive(Debug, PartialEq, Clone)]
pub enum MethodCall {
    WorkDoneProgressCreate(lsp::WorkDoneProgressCreateParams),
    ApplyWorkspaceEdit(lsp::ApplyWorkspaceEditParams),
    WorkspaceFolders,
    WorkspaceConfiguration(lsp::ConfigurationParams),
    RegisterCapability(lsp::RegistrationParams),
    UnregisterCapability(lsp::UnregistrationParams),
    ShowDocument(lsp::ShowDocumentParams),
    WorkspaceDiagnosticRefresh,
    ShowMessageRequest(lsp::ShowMessageRequestParams),
}

impl MethodCall {
    pub fn parse(method: &str, params: jsonrpc::Params) -> Result<MethodCall> {
        use lsp::request::Request;
        let request = match method {
            lsp::request::WorkDoneProgressCreate::METHOD => {
                let params: lsp::WorkDoneProgressCreateParams = params.parse()?;
                Self::WorkDoneProgressCreate(params)
            }
            lsp::request::ApplyWorkspaceEdit::METHOD => {
                let params: lsp::ApplyWorkspaceEditParams = params.parse()?;
                Self::ApplyWorkspaceEdit(params)
            }
            lsp::request::WorkspaceFoldersRequest::METHOD => Self::WorkspaceFolders,
            lsp::request::WorkspaceConfiguration::METHOD => {
                let params: lsp::ConfigurationParams = params.parse()?;
                Self::WorkspaceConfiguration(params)
            }
            lsp::request::RegisterCapability::METHOD => {
                let params: lsp::RegistrationParams = params.parse()?;
                Self::RegisterCapability(params)
            }
            lsp::request::UnregisterCapability::METHOD => {
                let params: lsp::UnregistrationParams = params.parse()?;
                Self::UnregisterCapability(params)
            }
            lsp::request::ShowDocument::METHOD => {
                let params: lsp::ShowDocumentParams = params.parse()?;
                Self::ShowDocument(params)
            }
            lsp::request::WorkspaceDiagnosticRefresh::METHOD => Self::WorkspaceDiagnosticRefresh,
            lsp::request::ShowMessageRequest::METHOD => {
                let params: lsp::ShowMessageRequestParams = params.parse()?;
                Self::ShowMessageRequest(params)
            }
            _ => {
                return Err(Error::Unhandled);
            }
        };
        Ok(request)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Notification {
    // we inject this notification to signal the LSP is ready
    Initialized,
    // and this notification to signal that the LSP exited
    Exit,
    PublishDiagnostics(lsp::PublishDiagnosticsParams),
    ShowMessage(lsp::ShowMessageParams),
    LogMessage(lsp::LogMessageParams),
    ProgressMessage(lsp::ProgressParams),
}

impl Notification {
    pub fn parse(method: &str, params: jsonrpc::Params) -> Result<Notification> {
        use lsp::notification::Notification as _;

        let notification = match method {
            lsp::notification::Initialized::METHOD => Self::Initialized,
            lsp::notification::Exit::METHOD => Self::Exit,
            lsp::notification::PublishDiagnostics::METHOD => {
                let params: lsp::PublishDiagnosticsParams = params.parse()?;
                Self::PublishDiagnostics(params)
            }

            lsp::notification::ShowMessage::METHOD => {
                let params: lsp::ShowMessageParams = params.parse()?;
                Self::ShowMessage(params)
            }
            lsp::notification::LogMessage::METHOD => {
                let params: lsp::LogMessageParams = params.parse()?;
                Self::LogMessage(params)
            }
            lsp::notification::Progress::METHOD => {
                let params: lsp::ProgressParams = params.parse()?;
                Self::ProgressMessage(params)
            }
            _ => {
                return Err(Error::Unhandled);
            }
        };

        Ok(notification)
    }
}
