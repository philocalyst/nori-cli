//! One running language server: its client, the documents it has open, and the task that answers
//! what the server asks of us and files its diagnostics.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use helix_lsp::Call;
use helix_lsp::Client;
use helix_lsp::LanguageServerId;
use helix_lsp::MethodCall;
use helix_lsp::Notification;
use helix_lsp::OffsetEncoding;
use helix_lsp::jsonrpc;
use helix_lsp::lsp;
use parking_lot::Mutex;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::diagnostics::Diagnostic;
use crate::diagnostics::DiagnosticStore;
use crate::diagnostics::Severity;
use crate::position::from_lsp_range;
use crate::position::split_lines;

struct OpenDoc {
    version: i32,
    text: Arc<str>,
}

/// What [`ServerHandle::sync_document`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocSync {
    Opened,
    Changed,
    Unchanged,
}

pub struct ServerHandle {
    id: LanguageServerId,
    name: String,
    root: PathBuf,
    client: Client,
    docs: Mutex<HashMap<PathBuf, OpenDoc>>,
    progress: Mutex<HashSet<String>>,
}

impl ServerHandle {
    pub(crate) fn new(id: LanguageServerId, name: String, root: PathBuf, client: Client) -> Self {
        Self {
            id,
            name,
            root,
            client,
            docs: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashSet::new()),
        }
    }

    pub fn id(&self) -> LanguageServerId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The raw client, for requests the manager does not wrap. Only valid after the handshake,
    /// which is the only time a handle is shared.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn encoding(&self) -> OffsetEncoding {
        if self.client.is_initialized() {
            self.client.offset_encoding()
        } else {
            OffsetEncoding::Utf16
        }
    }

    /// True while the server reports work-done progress (indexing, loading the project).
    pub fn is_busy(&self) -> bool {
        !self.progress.lock().is_empty()
    }

    pub fn document_text(&self, path: &Path) -> Option<Arc<str>> {
        self.docs.lock().get(path).map(|doc| Arc::clone(&doc.text))
    }

    fn open_version(&self, path: &Path) -> Option<i32> {
        self.docs.lock().get(path).map(|doc| doc.version)
    }

    /// Opens `path` or sends its new text, as needed. Notifications queue on a channel, so this
    /// never waits.
    pub fn sync_document(&self, path: &Path, language: &str, text: &Arc<str>) -> DocSync {
        if !self.client.is_initialized() {
            return DocSync::Unchanged;
        }
        let Ok(uri) = lsp::Url::from_file_path(path) else {
            return DocSync::Unchanged;
        };
        let mut docs = self.docs.lock();
        match docs.get_mut(path) {
            None => {
                docs.insert(
                    path.to_path_buf(),
                    OpenDoc {
                        version: 1,
                        text: Arc::clone(text),
                    },
                );
                drop(docs);
                self.client
                    .text_document_did_open(uri, 1, text, language.to_owned());
                DocSync::Opened
            }
            Some(doc) if *doc.text == **text => DocSync::Unchanged,
            Some(doc) => {
                doc.version += 1;
                doc.text = Arc::clone(text);
                let version = doc.version;
                drop(docs);
                self.client.text_document_did_change(
                    lsp::VersionedTextDocumentIdentifier::new(uri, version),
                    text,
                );
                DocSync::Changed
            }
        }
    }

    pub fn save_document(&self, path: &Path, text: &str) {
        if let Ok(uri) = lsp::Url::from_file_path(path) {
            self.client
                .text_document_did_save(lsp::TextDocumentIdentifier::new(uri), text);
        }
    }

    pub fn close_document(&self, path: &Path) {
        let was_open = self.docs.lock().remove(path).is_some();
        if was_open && let Ok(uri) = lsp::Url::from_file_path(path) {
            self.client
                .text_document_did_close(lsp::TextDocumentIdentifier::new(uri));
        }
    }

    pub fn identifier(path: &Path) -> Option<lsp::TextDocumentIdentifier> {
        lsp::Url::from_file_path(path)
            .ok()
            .map(lsp::TextDocumentIdentifier::new)
    }

    /// Asks the server to shut down and then exit, waiting at most `deadline` for it.
    pub async fn shutdown(&self, deadline: Duration) {
        if !self.client.is_initialized() {
            return;
        }
        if tokio::time::timeout(deadline, self.client.shutdown_and_exit())
            .await
            .is_err()
        {
            self.client.force_shutdown();
        }
    }

    pub fn stderr_tail(&self) -> Vec<String> {
        self.client.stderr_tail()
    }

    async fn convert(&self, path: &Path, items: Vec<lsp::Diagnostic>) -> Vec<Diagnostic> {
        if items.is_empty() {
            return Vec::new();
        }
        let text = match self.document_text(path) {
            Some(text) => text.to_string(),
            None => tokio::fs::read_to_string(path).await.unwrap_or_default(),
        };
        let lines = split_lines(&text);
        let encoding = self.encoding();
        items
            .into_iter()
            .map(|item| Diagnostic {
                severity: severity(item.severity),
                range: from_lsp_range(&lines, item.range, encoding),
                message: item.message,
                source: item.source,
                code: item.code.map(|code| match code {
                    lsp::NumberOrString::Number(number) => number.to_string(),
                    lsp::NumberOrString::String(text) => text,
                }),
                server: self.name.clone(),
            })
            .collect()
    }

    fn reply_to(&self, id: jsonrpc::Id, call: MethodCall) {
        let result: Value = match call {
            MethodCall::WorkspaceFolders => {
                let folder = lsp::Url::from_directory_path(&self.root)
                    .ok()
                    .map(|uri| json!({ "uri": uri, "name": self.name }));
                Value::Array(folder.into_iter().collect())
            }
            MethodCall::WorkspaceConfiguration(params) => {
                Value::Array(vec![Value::Null; params.items.len()])
            }
            MethodCall::ApplyWorkspaceEdit(_) => json!({
                "applied": false,
                "failureReason": "Nori applies edits itself; language servers may not",
            }),
            MethodCall::ShowDocument(_) => json!({ "success": false }),
            MethodCall::WorkDoneProgressCreate(_)
            | MethodCall::RegisterCapability(_)
            | MethodCall::UnregisterCapability(_)
            | MethodCall::WorkspaceDiagnosticRefresh
            | MethodCall::ShowMessageRequest(_) => Value::Null,
        };
        if let Err(error) = self.client.reply(id, Ok(result)) {
            tracing::debug!(server = %self.name, %error, "could not answer a server request");
        }
    }

    fn track_progress(&self, params: lsp::ProgressParams) {
        let token = match params.token {
            lsp::NumberOrString::Number(number) => number.to_string(),
            lsp::NumberOrString::String(text) => text,
        };
        let lsp::ProgressParamsValue::WorkDone(progress) = params.value;
        match progress {
            lsp::WorkDoneProgress::Begin(_) => {
                self.progress.lock().insert(token);
            }
            lsp::WorkDoneProgress::End(_) => {
                self.progress.lock().remove(&token);
            }
            lsp::WorkDoneProgress::Report(_) => {}
        }
    }

    async fn handle_notification(&self, notification: Notification, store: &DiagnosticStore) {
        match notification {
            Notification::PublishDiagnostics(params) => {
                let Ok(path) = params.uri.to_file_path() else {
                    return;
                };
                // A publish for text older than what we last sent describes a file that no
                // longer exists; waiting on it would end too early.
                if let (Some(version), Some(open)) = (params.version, self.open_version(&path))
                    && version < open
                {
                    return;
                }
                let diagnostics = self.convert(&path, params.diagnostics).await;
                store.publish(self.id.0, path, diagnostics);
            }
            Notification::ProgressMessage(params) => self.track_progress(params),
            Notification::ShowMessage(params) => {
                tracing::debug!(server = %self.name, message = %params.message, "window/showMessage");
            }
            Notification::LogMessage(params) => {
                tracing::trace!(server = %self.name, message = %params.message, "window/logMessage");
            }
            Notification::Initialized | Notification::Exit => {}
        }
    }
}

fn severity(severity: Option<lsp::DiagnosticSeverity>) -> Severity {
    match severity {
        Some(lsp::DiagnosticSeverity::ERROR) => Severity::Error,
        Some(lsp::DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(lsp::DiagnosticSeverity::HINT) => Severity::Hint,
        Some(_) | None => Severity::Information,
    }
}

/// Reads everything the server sends until it exits: files diagnostics, answers its requests and
/// then calls `on_exit` once with the reason. Runs as its own task.
pub(crate) async fn pump(
    handle: Weak<ServerHandle>,
    mut incoming: UnboundedReceiver<(LanguageServerId, Call)>,
    store: DiagnosticStore,
    on_exit: impl FnOnce(String) + Send,
) {
    // Only a weak reference is held while waiting: when the manager drops the server, the client
    // goes with it, the process is killed, the stream closes and this task ends.
    while let Some((_, call)) = incoming.recv().await {
        let Some(handle) = handle.upgrade() else {
            return;
        };
        match call {
            Call::Notification(notification) => {
                match Notification::parse(&notification.method, notification.params) {
                    Ok(Notification::Exit) => break,
                    Ok(parsed) => handle.handle_notification(parsed, &store).await,
                    Err(_) => {}
                }
            }
            Call::MethodCall(request) => match MethodCall::parse(&request.method, request.params) {
                Ok(parsed) => handle.reply_to(request.id, parsed),
                Err(_) => {
                    let error = jsonrpc::Error {
                        code: jsonrpc::ErrorCode::MethodNotFound,
                        message: "method not found".to_owned(),
                        data: None,
                    };
                    let _ = handle.client.reply(request.id, Err(error));
                }
            },
            Call::Invalid { .. } => {}
        }
    }
    let reason = match handle.upgrade().map(|handle| handle.stderr_tail()) {
        Some(tail) => match tail.iter().rev().find(|line| !line.trim().is_empty()) {
            Some(line) => format!("the server exited; last stderr line: {line}"),
            None => "the server exited".to_owned(),
        },
        None => "the server exited".to_owned(),
    };
    on_exit(reason);
}
