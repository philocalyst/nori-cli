// Modified for Nori. Derived from helix-lsp/src/client.rs of github.com/helix-editor/helix
// (commit 9de242bfac2aac9dfd0e4496eb25190e85c24e69), Mozilla Public License 2.0
// (see ../../LICENSE). Changes: decoupled from helix-core/helix-loader/helix-stdx (no ropes,
// change sets, language configuration or workspace discovery), full-text document sync only,
// file-operation requests and multi-root bookkeeping removed, generic transport I/O, and the
// handshake/initialization is driven by `Client::handshake`. See ../../NORI.md.

use crate::{
    jsonrpc,
    transport::{Payload, Transport, TransportIo},
    Call, Error, LanguageServerFeature, LanguageServerId, OffsetEncoding, Result,
};
use log::info;

use crate::lsp::{
    self, notification::DidChangeWorkspaceFolders, CodeActionCapabilityResolveSupport,
    DidChangeWorkspaceFoldersParams, PositionEncodingKind, SignatureHelp,
    WorkspaceFolder, WorkspaceFoldersChangeEvent,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf};
use std::{
    ffi::OsStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use std::{future::Future, time::Duration};
use std::process::Stdio;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::{
    io::{BufReader, BufWriter},
    process::{Child, Command},
    sync::{
        mpsc::{channel, UnboundedSender},
        Notify, OnceCell,
    },
};

fn workspace_for_uri(uri: lsp::Url) -> WorkspaceFolder {
    lsp::WorkspaceFolder {
        name: uri
            .path()
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .map(|basename| basename.to_string())
            .unwrap_or_default(),
        uri,
    }
}

#[derive(Debug)]
pub struct Client {
    id: LanguageServerId,
    name: String,
    _process: Option<Child>,
    server_tx: UnboundedSender<Payload>,
    request_counter: AtomicU64,
    pub(crate) capabilities: OnceCell<lsp::ServerCapabilities>,
    config: Option<Value>,
    root_path: std::path::PathBuf,
    root_uri: Option<lsp::Url>,
    workspace_folders: Mutex<Vec<lsp::WorkspaceFolder>>,
    initialize_notify: Arc<Notify>,
    /// Notified by the transport once `exit` has been flushed to the server's stdin.
    shutdown_flushed: Arc<Notify>,
    stderr_tail: crate::transport::StderrTail,
    req_timeout: u64,
}

impl Client {
    /// Merge FormattingOptions with 'config.format' and return it
    fn get_merged_formatting_options(
        &self,
        options: lsp::FormattingOptions,
    ) -> lsp::FormattingOptions {
        let config_format = self
            .config
            .as_ref()
            .and_then(|cfg| cfg.get("format"))
            .and_then(|fmt| HashMap::<String, lsp::FormattingProperty>::deserialize(fmt).ok());

        if let Some(mut properties) = config_format {
            // passed in options take precedence over 'config.format'
            properties.extend(options.properties);
            lsp::FormattingOptions {
                properties,
                ..options
            }
        } else {
            options
        }
    }

    /// Spawns `cmd` as a language server speaking LSP over its stdio. The caller resolves `cmd`
    /// on `PATH` first. Must be called inside a tokio runtime. The returned client is not yet
    /// initialized: drive [`Client::handshake`] with the returned `Notify`.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn start(
        cmd: &OsStr,
        args: &[String],
        config: Option<Value>,
        server_environment: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
        root_path: PathBuf,
        root_uri: Option<lsp::Url>,
        id: LanguageServerId,
        name: String,
        req_timeout: u64,
    ) -> Result<(
        Self,
        tokio::sync::mpsc::UnboundedReceiver<(LanguageServerId, Call)>,
        Arc<Notify>,
    )> {
        info!("Starting lsp {name:?} in root {root_path:?}");

        let mut process = Command::new(cmd)
            .envs(server_environment)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&root_path)
            // make sure the process is reaped on drop
            .kill_on_drop(true)
            .spawn()?;

        let missing_pipe = || Error::Other(anyhow::anyhow!("language server pipe unavailable"));
        let writer = BufWriter::new(process.stdin.take().ok_or_else(missing_pipe)?);
        let reader = BufReader::new(process.stdout.take().ok_or_else(missing_pipe)?);
        let stderr = BufReader::new(process.stderr.take().ok_or_else(missing_pipe)?);

        let (mut client, incoming, initialize_notify) = Self::start_with_io(
            reader,
            writer,
            stderr,
            config,
            root_path,
            root_uri,
            id,
            name,
            req_timeout,
        );
        client._process = Some(process);
        Ok((client, incoming, initialize_notify))
    }

    /// Like [`Client::start`] over arbitrary byte streams (an in-process fake server in tests, a
    /// socket, ...). Nothing is reaped on drop: the owner of the streams owns the server.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn start_with_io<R, W, E>(
        server_stdout: R,
        server_stdin: W,
        server_stderr: E,
        config: Option<Value>,
        root_path: PathBuf,
        root_uri: Option<lsp::Url>,
        id: LanguageServerId,
        name: String,
        req_timeout: u64,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<(LanguageServerId, Call)>,
        Arc<Notify>,
    )
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        E: AsyncBufRead + Unpin + Send + 'static,
    {
        let TransportIo {
            incoming,
            server_tx,
            initialize_notify,
            shutdown_flushed,
            stderr_tail,
        } = Transport::start(server_stdout, server_stdin, server_stderr, id, name.clone());

        let workspace_folders = root_uri
            .clone()
            .map(|root| vec![workspace_for_uri(root)])
            .unwrap_or_default();

        let client = Self {
            id,
            name,
            _process: None,
            server_tx,
            request_counter: AtomicU64::new(0),
            capabilities: OnceCell::new(),
            config,
            req_timeout,
            root_path,
            root_uri,
            workspace_folders: Mutex::new(workspace_folders),
            initialize_notify: initialize_notify.clone(),
            shutdown_flushed,
            stderr_tail,
        };

        (client, incoming, initialize_notify)
    }

    /// The last lines the server wrote to stderr (oldest first), for crash diagnostics.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().iter().cloned().collect()
    }

    /// Runs the `initialize` handshake: sends `initialize`, stores the capabilities, sends
    /// `initialized` and releases requests queued behind it. `deadline` bounds the whole exchange.
    pub async fn handshake(&self, deadline: Duration) -> Result<&lsp::ServerCapabilities> {
        let init = self.capabilities.get_or_try_init(|| async {
            self.initialize(false).await.map(|response| response.capabilities)
        });
        let capabilities = match tokio::time::timeout(deadline, init).await {
            Ok(result) => result?,
            Err(_) => return Err(Error::Timeout(jsonrpc::Id::Num(0))),
        };
        self.notify::<lsp::notification::Initialized>(lsp::InitializedParams {});
        self.initialize_notify.notify_one();
        Ok(capabilities)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> LanguageServerId {
        self.id
    }

    fn next_request_id(&self) -> jsonrpc::Id {
        let id = self.request_counter.fetch_add(1, Ordering::Relaxed);
        jsonrpc::Id::Num(id)
    }

    fn value_into_params(value: Value) -> jsonrpc::Params {
        use jsonrpc::Params;

        match value {
            Value::Null => Params::None,
            Value::Bool(_) | Value::Number(_) | Value::String(_) => Params::Array(vec![value]),
            Value::Array(vec) => Params::Array(vec),
            Value::Object(map) => Params::Map(map),
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.capabilities.get().is_some()
    }

    pub fn capabilities(&self) -> &lsp::ServerCapabilities {
        self.capabilities
            .get()
            .expect("language server not yet initialized!")
    }

    #[inline]
    pub fn supports_feature(&self, feature: LanguageServerFeature) -> bool {
        let capabilities = self.capabilities();

        use lsp::*;
        match feature {
            LanguageServerFeature::Format => matches!(
                capabilities.document_formatting_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoDeclaration => matches!(
                capabilities.declaration_provider,
                Some(
                    DeclarationCapability::Simple(true)
                        | DeclarationCapability::RegistrationOptions(_)
                        | DeclarationCapability::Options(_),
                )
            ),
            LanguageServerFeature::GotoDefinition => matches!(
                capabilities.definition_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoTypeDefinition => matches!(
                capabilities.type_definition_provider,
                Some(
                    TypeDefinitionProviderCapability::Simple(true)
                        | TypeDefinitionProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::GotoReference => matches!(
                capabilities.references_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoImplementation => matches!(
                capabilities.implementation_provider,
                Some(
                    ImplementationProviderCapability::Simple(true)
                        | ImplementationProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::SignatureHelp => capabilities.signature_help_provider.is_some(),
            LanguageServerFeature::Hover => matches!(
                capabilities.hover_provider,
                Some(HoverProviderCapability::Simple(true) | HoverProviderCapability::Options(_),)
            ),
            LanguageServerFeature::DocumentHighlight => matches!(
                capabilities.document_highlight_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::Completion => capabilities.completion_provider.is_some(),
            LanguageServerFeature::CodeAction => matches!(
                capabilities.code_action_provider,
                Some(
                    CodeActionProviderCapability::Simple(true)
                        | CodeActionProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::DocumentLinks => capabilities.document_link_provider.is_some(),
            LanguageServerFeature::WorkspaceCommand => {
                capabilities.execute_command_provider.is_some()
            }
            LanguageServerFeature::DocumentSymbols => matches!(
                capabilities.document_symbol_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::WorkspaceSymbols => matches!(
                capabilities.workspace_symbol_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::Diagnostics => true, // there's no extra server capability
            LanguageServerFeature::PullDiagnostics => capabilities.diagnostic_provider.is_some(),
            LanguageServerFeature::RenameSymbol => matches!(
                capabilities.rename_provider,
                Some(OneOf::Left(true)) | Some(OneOf::Right(_))
            ),
            LanguageServerFeature::InlayHints => matches!(
                capabilities.inlay_hint_provider,
                Some(OneOf::Left(true) | OneOf::Right(InlayHintServerCapabilities::Options(_)))
            ),
            LanguageServerFeature::DocumentColors => matches!(
                capabilities.color_provider,
                Some(
                    ColorProviderCapability::Simple(true)
                        | ColorProviderCapability::ColorProvider(_)
                        | ColorProviderCapability::Options(_)
                )
            ),
            LanguageServerFeature::CallHierarchy => matches!(
                capabilities.call_hierarchy_provider,
                Some(
                    CallHierarchyServerCapability::Simple(true)
                        | CallHierarchyServerCapability::Options(_)
                )
            ),
        }
    }

    pub fn offset_encoding(&self) -> OffsetEncoding {
        self.capabilities()
            .position_encoding
            .as_ref()
            .and_then(|encoding| match encoding.as_str() {
                "utf-8" => Some(OffsetEncoding::Utf8),
                "utf-16" => Some(OffsetEncoding::Utf16),
                "utf-32" => Some(OffsetEncoding::Utf32),
                encoding => {
                    log::error!("Server provided invalid position encoding {encoding}, defaulting to utf-16");
                    None
                },
            })
            .unwrap_or_default()
    }

    pub fn config(&self) -> Option<&Value> {
        self.config.as_ref()
    }

    pub async fn workspace_folders(
        &self,
    ) -> parking_lot::MutexGuard<'_, Vec<lsp::WorkspaceFolder>> {
        self.workspace_folders.lock()
    }

    /// Execute a RPC request on the language server.
    fn call<R: lsp::request::Request>(
        &self,
        params: R::Params,
    ) -> impl Future<Output = Result<R::Result>>
    where
        R::Params: serde::Serialize,
    {
        self.call_with_ref::<R>(&params)
    }

    fn call_with_ref<R: lsp::request::Request>(
        &self,
        params: &R::Params,
    ) -> impl Future<Output = Result<R::Result>>
    where
        R::Params: serde::Serialize,
    {
        self.call_with_timeout::<R>(params, self.req_timeout)
    }

    fn call_with_timeout<R: lsp::request::Request>(
        &self,
        params: &R::Params,
        timeout_secs: u64,
    ) -> impl Future<Output = Result<R::Result>>
    where
        R::Params: serde::Serialize,
    {
        let server_tx = self.server_tx.clone();
        let id = self.next_request_id();

        // It's important that this is not part of the future so that it gets executed right away
        // and the request order stays consistent.
        let rx = serde_json::to_value(params)
            .map_err(Error::from)
            .and_then(|params| {
                let request = jsonrpc::MethodCall {
                    jsonrpc: Some(jsonrpc::Version::V2),
                    id: id.clone(),
                    method: R::METHOD.to_string(),
                    params: Self::value_into_params(params),
                };
                let (tx, rx) = channel::<Result<Value>>(1);
                server_tx
                    .send(Payload::Request {
                        chan: tx,
                        value: request,
                    })
                    .map_err(|e| Error::Other(e.into()))?;
                Ok(rx)
            });

        async move {
            use std::time::Duration;
            use tokio::time::timeout;
            // TODO: delay other calls until initialize success
            timeout(Duration::from_secs(timeout_secs), rx?.recv())
                .await
                .map_err(|_| Error::Timeout(id))? // return Timeout
                .ok_or(Error::StreamClosed)?
                .and_then(|value| serde_json::from_value(value).map_err(Into::into))
        }
    }

    /// Send a RPC notification to the language server.
    pub fn notify<R: lsp::notification::Notification>(&self, params: R::Params)
    where
        R::Params: serde::Serialize,
    {
        let server_tx = self.server_tx.clone();

        let params = match serde_json::to_value(params) {
            Ok(params) => params,
            Err(err) => {
                log::error!(
                    "Failed to serialize params for notification '{}' for server '{}': {err}",
                    R::METHOD,
                    self.name,
                );
                return;
            }
        };

        let notification = jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: R::METHOD.to_string(),
            params: Self::value_into_params(params),
        };

        if let Err(err) = server_tx.send(Payload::Notification(notification)) {
            log::error!(
                "Failed to send notification '{}' to server '{}': {err}",
                R::METHOD,
                self.name
            );
        }
    }

    /// Reply to a language server RPC call.
    pub fn reply(
        &self,
        id: jsonrpc::Id,
        result: core::result::Result<Value, jsonrpc::Error>,
    ) -> Result<()> {
        use jsonrpc::{Failure, Output, Success, Version};

        let server_tx = self.server_tx.clone();

        let output = match result {
            Ok(result) => Output::Success(Success {
                jsonrpc: Some(Version::V2),
                id,
                result,
            }),
            Err(error) => Output::Failure(Failure {
                jsonrpc: Some(Version::V2),
                id,
                error,
            }),
        };

        server_tx
            .send(Payload::Response(output))
            .map_err(|e| Error::Other(e.into()))?;

        Ok(())
    }

    // -------------------------------------------------------------------------------------------
    // General messages
    // -------------------------------------------------------------------------------------------

    pub(crate) async fn initialize(&self, enable_snippets: bool) -> Result<lsp::InitializeResult> {
        if let Some(config) = &self.config {
            log::info!("Using custom LSP config: {}", config);
        }

        #[allow(deprecated)]
        let params = lsp::InitializeParams {
            process_id: Some(std::process::id()),
            workspace_folders: Some(self.workspace_folders.lock().clone()),
            // root_path is obsolete, but some clients like pyright still use it so we specify both.
            // clients will prefer _uri if possible
            root_path: self.root_path.to_str().map(|path| path.to_owned()),
            root_uri: self.root_uri.clone(),
            initialization_options: self.config.clone(),
            capabilities: lsp::ClientCapabilities {
                workspace: Some(lsp::WorkspaceClientCapabilities {
                    configuration: Some(true),
                    did_change_configuration: Some(lsp::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    workspace_folders: Some(true),
                    apply_edit: Some(true),
                    symbol: Some(lsp::WorkspaceSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        symbol_kind: Some(lsp::SymbolKindCapability {
                            value_set: Some(lsp::SymbolKind::all()),
                        }),
                        ..Default::default()
                    }),
                    execute_command: Some(lsp::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    inlay_hint: Some(lsp::InlayHintWorkspaceClientCapabilities {
                        refresh_support: Some(false),
                    }),
                    workspace_edit: Some(lsp::WorkspaceEditClientCapabilities {
                        document_changes: Some(true),
                        resource_operations: Some(vec![
                            lsp::ResourceOperationKind::Create,
                            lsp::ResourceOperationKind::Rename,
                            lsp::ResourceOperationKind::Delete,
                        ]),
                        failure_handling: Some(lsp::FailureHandlingKind::Abort),
                        normalizes_line_endings: Some(false),
                        change_annotation_support: None,
                    }),
                    did_change_watched_files: Some(lsp::DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration: Some(false),
                        relative_pattern_support: Some(false),
                    }),
                    ..Default::default()
                }),
                text_document: Some(lsp::TextDocumentClientCapabilities {
                    completion: Some(lsp::CompletionClientCapabilities {
                        completion_item: Some(lsp::CompletionItemCapability {
                            snippet_support: Some(enable_snippets),
                            resolve_support: Some(lsp::CompletionItemCapabilityResolveSupport {
                                properties: vec![
                                    String::from("documentation"),
                                    String::from("detail"),
                                    String::from("additionalTextEdits"),
                                ],
                            }),
                            insert_replace_support: Some(true),
                            deprecated_support: Some(true),
                            tag_support: Some(lsp::TagSupport {
                                value_set: vec![lsp::CompletionItemTag::DEPRECATED],
                            }),
                            ..Default::default()
                        }),
                        completion_item_kind: Some(lsp::CompletionItemKindCapability {
                            ..Default::default()
                        }),
                        context_support: None, // additional context information Some(true)
                        ..Default::default()
                    }),
                    hover: Some(lsp::HoverClientCapabilities {
                        // if not specified, rust-analyzer returns plaintext marked as markdown but
                        // badly formatted.
                        content_format: Some(vec![lsp::MarkupKind::Markdown]),
                        ..Default::default()
                    }),
                    signature_help: Some(lsp::SignatureHelpClientCapabilities {
                        signature_information: Some(lsp::SignatureInformationSettings {
                            documentation_format: Some(vec![lsp::MarkupKind::Markdown]),
                            parameter_information: Some(lsp::ParameterInformationSettings {
                                label_offset_support: Some(true),
                            }),
                            active_parameter_support: Some(true),
                        }),
                        ..Default::default()
                    }),
                    rename: Some(lsp::RenameClientCapabilities {
                        dynamic_registration: Some(false),
                        prepare_support: Some(true),
                        prepare_support_default_behavior: None,
                        honors_change_annotations: Some(false),
                    }),
                    formatting: Some(lsp::DocumentFormattingClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    code_action: Some(lsp::CodeActionClientCapabilities {
                        code_action_literal_support: Some(lsp::CodeActionLiteralSupport {
                            code_action_kind: lsp::CodeActionKindLiteralSupport {
                                value_set: [
                                    lsp::CodeActionKind::EMPTY,
                                    lsp::CodeActionKind::QUICKFIX,
                                    lsp::CodeActionKind::REFACTOR,
                                    lsp::CodeActionKind::REFACTOR_EXTRACT,
                                    lsp::CodeActionKind::REFACTOR_INLINE,
                                    lsp::CodeActionKind::REFACTOR_REWRITE,
                                    lsp::CodeActionKind::SOURCE,
                                    lsp::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                                    lsp::CodeActionKind::SOURCE_FIX_ALL,
                                ]
                                .iter()
                                .map(|kind| kind.as_str().to_string())
                                .collect(),
                            },
                        }),
                        is_preferred_support: Some(true),
                        disabled_support: Some(true),
                        data_support: Some(true),
                        resolve_support: Some(CodeActionCapabilityResolveSupport {
                            properties: vec!["edit".to_owned(), "command".to_owned()],
                        }),
                        ..Default::default()
                    }),
                    publish_diagnostics: Some(lsp::PublishDiagnosticsClientCapabilities {
                        version_support: Some(true),
                        tag_support: Some(lsp::TagSupport {
                            value_set: vec![
                                lsp::DiagnosticTag::UNNECESSARY,
                                lsp::DiagnosticTag::DEPRECATED,
                            ],
                        }),
                        ..Default::default()
                    }),
                    inlay_hint: Some(lsp::InlayHintClientCapabilities {
                        dynamic_registration: Some(false),
                        resolve_support: None,
                    }),
                    document_link: Some(lsp::DocumentLinkClientCapabilities {
                        dynamic_registration: Some(false),
                        tooltip_support: Some(false),
                    }),
                    call_hierarchy: Some(lsp::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    document_symbol: Some(lsp::DocumentSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        symbol_kind: Some(lsp::SymbolKindCapability {
                            value_set: Some(lsp::SymbolKind::all()),
                        }),
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                window: Some(lsp::WindowClientCapabilities {
                    show_message: Some(lsp::ShowMessageRequestClientCapabilities {
                        message_action_item: Some(lsp::MessageActionItemCapabilities {
                            additional_properties_support: Some(true),
                        }),
                    }),
                    work_done_progress: Some(true),
                    show_document: Some(lsp::ShowDocumentClientCapabilities { support: false }),
                }),
                general: Some(lsp::GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF32,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            trace: None,
            client_info: Some(lsp::ClientInfo {
                name: String::from("nori"),
                version: Some(String::from(env!("CARGO_PKG_VERSION"))),
            }),
            locale: None, // TODO
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
        };

        self.call::<lsp::request::Initialize>(params).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.call::<lsp::request::Shutdown>(()).await
    }

    pub fn exit(&self) {
        self.notify::<lsp::notification::Exit>(())
    }

    /// Tries to shut down the language server but returns
    /// early if server responds with an error.
    pub async fn shutdown_and_exit(&self) -> Result<()> {
        self.shutdown().await?;
        self.exit();
        Ok(())
    }

    /// Sends the LSP shutdown request followed immediately by the exit
    /// notification, without waiting for the shutdown response (fire-and-forget).
    /// The server receives both in order and exits on its own; any late shutdown
    /// response is discarded. Quitting therefore never blocks on a slow server
    /// (e.g. gopls flushing ~1k log messages before it would answer `shutdown`).
    ///
    /// The child process is not waited on here: it is killed by `kill_on_drop`
    /// when this `Client` is dropped. `close_language_servers` first gives a short
    /// grace window so the `exit` reaches stdin and well-behaved servers can begin
    /// exiting before the kill.
    pub fn force_shutdown(&self) {
        let request = jsonrpc::MethodCall {
            jsonrpc: Some(jsonrpc::Version::V2),
            id: self.next_request_id(),
            method: <lsp::request::Shutdown as lsp::request::Request>::METHOD.to_string(),
            params: jsonrpc::Params::None,
        };
        // The response receiver is dropped immediately; we do not wait for a reply.
        let (chan, _) = tokio::sync::mpsc::channel(1);
        let _ = self.server_tx.send(Payload::Request {
            chan,
            value: request,
        });
        self.exit();
    }

    /// Resolves once the `exit` notification has been written to the server's stdin
    /// (i.e. helix's *outbound* write completed — independent of how slow the server
    /// is, since it doesn't wait for any server response).
    pub async fn wait_shutdown_flushed(&self) {
        self.shutdown_flushed.notified().await;
    }

    // -------------------------------------------------------------------------------------------
    // Workspace
    // -------------------------------------------------------------------------------------------

    pub fn did_change_configuration(&self, settings: Value) {
        self.notify::<lsp::notification::DidChangeConfiguration>(
            lsp::DidChangeConfigurationParams { settings },
        )
    }

    pub fn did_change_workspace(&self, added: Vec<WorkspaceFolder>, removed: Vec<WorkspaceFolder>) {
        self.notify::<DidChangeWorkspaceFolders>(DidChangeWorkspaceFoldersParams {
            event: WorkspaceFoldersChangeEvent { added, removed },
        })
    }


    // -------------------------------------------------------------------------------------------
    // Text document
    // -------------------------------------------------------------------------------------------

    pub fn text_document_did_open(
        &self,
        uri: lsp::Url,
        version: i32,
        text: &str,
        language_id: String,
    ) {
        self.notify::<lsp::notification::DidOpenTextDocument>(lsp::DidOpenTextDocumentParams {
            text_document: lsp::TextDocumentItem {
                uri,
                language_id,
                version,
                text: text.to_owned(),
            },
        })
    }

    /// Sends the whole new text. A change event without a range replaces the document, which
    /// every server accepts whether it asked for full or incremental sync.
    pub fn text_document_did_change(
        &self,
        text_document: lsp::VersionedTextDocumentIdentifier,
        new_text: &str,
    ) -> Option<()> {
        let capabilities = self.capabilities.get()?;

        // Return early if the server does not support document sync.
        match capabilities.text_document_sync {
            Some(
                lsp::TextDocumentSyncCapability::Kind(kind)
                | lsp::TextDocumentSyncCapability::Options(lsp::TextDocumentSyncOptions {
                    change: Some(kind),
                    ..
                }),
            ) if kind != lsp::TextDocumentSyncKind::NONE => (),
            _ => return None,
        }

        self.notify::<lsp::notification::DidChangeTextDocument>(lsp::DidChangeTextDocumentParams {
            text_document,
            content_changes: vec![lsp::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: new_text.to_owned(),
            }],
        });
        Some(())
    }

    pub fn text_document_did_close(&self, text_document: lsp::TextDocumentIdentifier) {
        self.notify::<lsp::notification::DidCloseTextDocument>(lsp::DidCloseTextDocumentParams {
            text_document,
        })
    }

    // will_save / will_save_wait_until

    pub fn text_document_did_save(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        text: &str,
    ) -> Option<()> {
        let capabilities = self.capabilities.get().unwrap();

        let include_text = match &capabilities.text_document_sync.as_ref()? {
            lsp::TextDocumentSyncCapability::Options(lsp::TextDocumentSyncOptions {
                save: options,
                ..
            }) => match options.as_ref()? {
                lsp::TextDocumentSyncSaveOptions::Supported(true) => false,
                lsp::TextDocumentSyncSaveOptions::SaveOptions(lsp::SaveOptions {
                    include_text,
                }) => include_text.unwrap_or(false),
                lsp::TextDocumentSyncSaveOptions::Supported(false) => return None,
            },
            // see: https://github.com/microsoft/language-server-protocol/issues/288
            lsp::TextDocumentSyncCapability::Kind(..) => false,
        };

        self.notify::<lsp::notification::DidSaveTextDocument>(lsp::DidSaveTextDocumentParams {
            text_document,
            text: include_text.then(|| text.to_owned()),
        });
        Some(())
    }

    pub fn completion(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
        context: lsp::CompletionContext,
    ) -> Option<impl Future<Output = Result<Option<lsp::CompletionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support completion.
        capabilities.completion_provider.as_ref()?;

        let params = lsp::CompletionParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            context: Some(context),
            // TODO: support these tokens by async receiving and updating the choice list
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        Some(self.call::<lsp::request::Completion>(params))
    }

    pub fn resolve_completion_item(
        &self,
        completion_item: &lsp::CompletionItem,
    ) -> impl Future<Output = Result<lsp::CompletionItem>> {
        self.call_with_ref::<lsp::request::ResolveCompletionItem>(completion_item)
    }

    pub fn resolve_code_action(
        &self,
        code_action: &lsp::CodeAction,
    ) -> Option<impl Future<Output = Result<lsp::CodeAction>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support resolving code actions.
        match capabilities.code_action_provider {
            Some(lsp::CodeActionProviderCapability::Options(lsp::CodeActionOptions {
                resolve_provider: Some(true),
                ..
            })) => (),
            _ => return None,
        }

        Some(self.call_with_ref::<lsp::request::CodeActionResolveRequest>(code_action))
    }

    pub fn text_document_signature_help(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<SignatureHelp>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support signature help.
        capabilities.signature_help_provider.as_ref()?;

        let params = lsp::SignatureHelpParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            context: None,
            // lsp::SignatureHelpContext
        };

        Some(self.call::<lsp::request::SignatureHelpRequest>(params))
    }

    pub fn text_document_range_inlay_hints(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::InlayHint>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.inlay_hint_provider {
            Some(
                lsp::OneOf::Left(true)
                | lsp::OneOf::Right(lsp::InlayHintServerCapabilities::Options(_)),
            ) => (),
            _ => return None,
        }

        let params = lsp::InlayHintParams {
            text_document,
            range,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::InlayHintRequest>(params))
    }

    pub fn text_document_document_color(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Vec<lsp::ColorInformation>>>> {
        self.capabilities.get().unwrap().color_provider.as_ref()?;
        let params = lsp::DocumentColorParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: helix_lsp_types::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::DocumentColor>(params))
    }

    pub fn text_document_document_link(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::DocumentLink>>>>> {
        if !self.supports_feature(LanguageServerFeature::DocumentLinks) {
            return None;
        }

        let params = lsp::DocumentLinkParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::DocumentLinkRequest>(params))
    }

    pub fn resolve_document_link(
        &self,
        params: lsp::DocumentLink,
    ) -> Option<impl Future<Output = Result<lsp::DocumentLink>>> {
        if !self.supports_feature(LanguageServerFeature::DocumentLinks) {
            return None;
        }

        Some(self.call::<lsp::request::DocumentLinkResolve>(params))
    }

    pub fn text_document_hover(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::Hover>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support hover.
        match capabilities.hover_provider {
            Some(
                lsp::HoverProviderCapability::Simple(true)
                | lsp::HoverProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::HoverParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            // lsp::SignatureHelpContext
        };

        Some(self.call::<lsp::request::HoverRequest>(params))
    }

    // formatting

    pub fn text_document_formatting(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        options: lsp::FormattingOptions,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TextEdit>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support formatting.
        match capabilities.document_formatting_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        };

        let options = self.get_merged_formatting_options(options);

        let params = lsp::DocumentFormattingParams {
            text_document,
            options,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::Formatting>(params))
    }

    pub fn text_document_range_formatting(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        options: lsp::FormattingOptions,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TextEdit>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support range formatting.
        match capabilities.document_range_formatting_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        };

        let options = self.get_merged_formatting_options(options);

        let params = lsp::DocumentRangeFormattingParams {
            text_document,
            range,
            options,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::RangeFormatting>(params))
    }

    pub fn text_document_diagnostic(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        previous_result_id: Option<String>,
    ) -> Option<impl Future<Output = Result<lsp::DocumentDiagnosticReportResult>>> {
        let capabilities = self.capabilities();

        // Return early if the server does not support pull diagnostic.
        let identifier = match capabilities.diagnostic_provider.as_ref()? {
            lsp::DiagnosticServerCapabilities::Options(cap) => cap.identifier.clone(),
            lsp::DiagnosticServerCapabilities::RegistrationOptions(cap) => {
                cap.diagnostic_options.identifier.clone()
            }
        };

        let params = lsp::DocumentDiagnosticParams {
            text_document,
            identifier,
            previous_result_id,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::DocumentDiagnosticRequest>(params))
    }

    pub fn text_document_document_highlight(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::DocumentHighlight>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support document highlight.
        match capabilities.document_highlight_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::DocumentHighlightParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        Some(self.call::<lsp::request::DocumentHighlightRequest>(params))
    }

    fn goto_request<
        T: lsp::request::Request<
            Params = lsp::GotoDefinitionParams,
            Result = Option<lsp::GotoDefinitionResponse>,
        >,
    >(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<T::Result>> {
        let params = lsp::GotoDefinitionParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        self.call::<T>(params)
    }

    pub fn goto_definition(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-definition.
        match capabilities.definition_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoDefinition>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_declaration(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-declaration.
        match capabilities.declaration_provider {
            Some(
                lsp::DeclarationCapability::Simple(true)
                | lsp::DeclarationCapability::RegistrationOptions(_)
                | lsp::DeclarationCapability::Options(_),
            ) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoDeclaration>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_type_definition(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-type-definition.
        match capabilities.type_definition_provider {
            Some(
                lsp::TypeDefinitionProviderCapability::Simple(true)
                | lsp::TypeDefinitionProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoTypeDefinition>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_implementation(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-definition.
        match capabilities.implementation_provider {
            Some(
                lsp::ImplementationProviderCapability::Simple(true)
                | lsp::ImplementationProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoImplementation>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_reference(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        include_declaration: bool,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::Location>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-reference.
        match capabilities.references_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::ReferenceParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            context: lsp::ReferenceContext {
                include_declaration,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        Some(self.call::<lsp::request::References>(params))
    }

    pub fn document_symbols(
        &self,
        text_document: lsp::TextDocumentIdentifier,
    ) -> Option<impl Future<Output = Result<Option<lsp::DocumentSymbolResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support document symbols.
        match capabilities.document_symbol_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::DocumentSymbolParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::DocumentSymbolRequest>(params))
    }

    pub fn prepare_call_hierarchy(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CallHierarchyItem>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.call_hierarchy_provider {
            Some(
                lsp::CallHierarchyServerCapability::Simple(true)
                | lsp::CallHierarchyServerCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CallHierarchyPrepareParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
        };

        Some(self.call::<lsp::request::CallHierarchyPrepare>(params))
    }

    pub fn call_hierarchy_incoming(
        &self,
        item: lsp::CallHierarchyItem,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CallHierarchyIncomingCall>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.call_hierarchy_provider {
            Some(
                lsp::CallHierarchyServerCapability::Simple(true)
                | lsp::CallHierarchyServerCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CallHierarchyIncomingCallsParams {
            item,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::CallHierarchyIncomingCalls>(params))
    }

    pub fn call_hierarchy_outgoing(
        &self,
        item: lsp::CallHierarchyItem,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CallHierarchyOutgoingCall>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.call_hierarchy_provider {
            Some(
                lsp::CallHierarchyServerCapability::Simple(true)
                | lsp::CallHierarchyServerCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CallHierarchyOutgoingCallsParams {
            item,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::CallHierarchyOutgoingCalls>(params))
    }

    pub fn prepare_rename(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Option<impl Future<Output = Result<Option<lsp::PrepareRenameResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.rename_provider {
            Some(lsp::OneOf::Right(lsp::RenameOptions {
                prepare_provider: Some(true),
                ..
            })) => (),
            _ => return None,
        }

        let params = lsp::TextDocumentPositionParams {
            text_document,
            position,
        };

        Some(self.call::<lsp::request::PrepareRenameRequest>(params))
    }

    // empty string to get all symbols
    pub fn workspace_symbols(
        &self,
        query: String,
    ) -> Option<impl Future<Output = Result<Option<lsp::WorkspaceSymbolResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support workspace symbols.
        match capabilities.workspace_symbol_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::WorkspaceSymbolParams {
            query,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::WorkspaceSymbolRequest>(params))
    }

    pub fn code_actions(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        context: lsp::CodeActionContext,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CodeActionOrCommand>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support code actions.
        match capabilities.code_action_provider {
            Some(
                lsp::CodeActionProviderCapability::Simple(true)
                | lsp::CodeActionProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CodeActionParams {
            text_document,
            range,
            context,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::CodeActionRequest>(params))
    }

    pub fn rename_symbol(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        new_name: String,
    ) -> Option<impl Future<Output = Result<Option<lsp::WorkspaceEdit>>>> {
        if !self.supports_feature(LanguageServerFeature::RenameSymbol) {
            return None;
        }

        let params = lsp::RenameParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            new_name,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: None,
            },
        };

        Some(self.call::<lsp::request::Rename>(params))
    }

    pub fn command(
        &self,
        command: lsp::Command,
    ) -> Option<impl Future<Output = Result<Option<Value>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the language server does not support executing commands.
        capabilities.execute_command_provider.as_ref()?;

        let params = lsp::ExecuteCommandParams {
            command: command.command,
            arguments: command.arguments.unwrap_or_default(),
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: None,
            },
        };

        Some(self.call::<lsp::request::ExecuteCommand>(params))
    }

    pub fn did_change_watched_files(&self, changes: Vec<lsp::FileEvent>) {
        self.notify::<lsp::notification::DidChangeWatchedFiles>(lsp::DidChangeWatchedFilesParams {
            changes,
        })
    }
}
