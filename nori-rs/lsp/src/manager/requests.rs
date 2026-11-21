//! The request APIs: definition, references, hover, symbols, rename preview, call hierarchy and
//! code actions. Each takes plain Nori types, asks the file's server and answers in plain Nori
//! types.

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use helix_lsp::OffsetEncoding;
use helix_lsp::lsp;
use tokio::sync::oneshot;

use super::LspManager;
use super::actor::Command;
use crate::convert::Sources;
use crate::convert::{self};
use crate::diagnostics::Severity;
use crate::error::LspError;
use crate::error::LspResult;
use crate::position::resolve_target;
use crate::position::split_lines;
use crate::position::to_lsp_position;
use crate::server::ServerHandle;
use crate::types::CallDirection;
use crate::types::CallItem;
use crate::types::CodeAction;
use crate::types::EditPreview;
use crate::types::GotoKind;
use crate::types::Hover;
use crate::types::Location;
use crate::types::ResolvedTarget;
use crate::types::Symbol;
use crate::types::Target;

type GotoFuture = std::pin::Pin<
    Box<dyn Future<Output = helix_lsp::Result<Option<lsp::GotoDefinitionResponse>>> + Send>,
>;

/// A file made ready for requests.
pub(crate) struct Prepared {
    pub path: PathBuf,
    pub text: Arc<str>,
    pub handles: Vec<Arc<ServerHandle>>,
}

/// An answer together with the position it is about, so callers can say where a `symbol` landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer<T> {
    pub target: ResolvedTarget,
    pub value: T,
}

impl Prepared {
    fn resolve(&self, target: &Target) -> LspResult<ResolvedTarget> {
        let mut target = target.clone();
        target.path = self.path.clone();
        resolve_target(&self.text, &target)
    }

    fn position(&self, resolved: &ResolvedTarget, encoding: OffsetEncoding) -> lsp::Position {
        to_lsp_position(
            &split_lines(&self.text),
            resolved.line,
            resolved.column,
            encoding,
        )
    }

    fn sources(&self) -> Sources {
        Sources::default().with(&self.path, &self.text)
    }
}

impl LspManager {
    /// Runs one request with the request timeout and maps failures to [`LspError`].
    async fn timed<T>(
        &self,
        handle: &ServerHandle,
        request: &str,
        future: impl Future<Output = helix_lsp::Result<T>>,
    ) -> LspResult<T> {
        let limit = self.inner.timing.request;
        match tokio::time::timeout(limit, future).await {
            Err(_) => Err(LspError::Timeout {
                server: handle.name().to_owned(),
                request: request.to_owned(),
                seconds: i64::try_from(limit.as_secs()).unwrap_or(i64::MAX),
            }),
            Ok(Err(error)) => Err(LspError::Request {
                server: handle.name().to_owned(),
                request: request.to_owned(),
                message: error.to_string(),
            }),
            Ok(Ok(value)) => Ok(value),
        }
    }

    /// Asks each server of the file in turn until one answers. A server that lacks the feature
    /// (`ask` returns `None`) is skipped.
    async fn first_server<T, Fut>(
        &self,
        handles: &[Arc<ServerHandle>],
        request: &str,
        ask: impl Fn(&ServerHandle) -> Option<Fut>,
    ) -> LspResult<(OffsetEncoding, T)>
    where
        Fut: Future<Output = helix_lsp::Result<T>>,
    {
        let mut last_error = None;
        for handle in handles {
            match ask(handle) {
                None => {}
                Some(future) => match self.timed(handle, request, future).await {
                    Ok(value) => return Ok((handle.encoding(), value)),
                    Err(error) => last_error = Some(error),
                },
            }
        }
        Err(last_error.unwrap_or_else(|| LspError::Unsupported {
            server: handles
                .iter()
                .map(|handle| handle.name())
                .collect::<Vec<_>>()
                .join(", "),
            feature: request.to_owned(),
        }))
    }

    /// Go to the definition, type definition, implementation or declaration of the symbol at
    /// `target`.
    pub async fn goto(&self, kind: GotoKind, target: &Target) -> LspResult<Answer<Vec<Location>>> {
        let prepared = self.prepare(&target.path).await?;
        let resolved = prepared.resolve(target)?;
        let (encoding, response) = self
            .first_server(&prepared.handles, kind.label(), |handle| {
                let id = ServerHandle::identifier(&prepared.path)?;
                let position = prepared.position(&resolved, handle.encoding());
                let client = handle.client();
                let future: Option<GotoFuture> = match kind {
                    GotoKind::Definition => client
                        .goto_definition(id, position, None)
                        .map(|future| Box::pin(future) as GotoFuture),
                    GotoKind::TypeDefinition => client
                        .goto_type_definition(id, position, None)
                        .map(|future| Box::pin(future) as GotoFuture),
                    GotoKind::Implementation => client
                        .goto_implementation(id, position, None)
                        .map(|future| Box::pin(future) as GotoFuture),
                    GotoKind::Declaration => client
                        .goto_declaration(id, position, None)
                        .map(|future| Box::pin(future) as GotoFuture),
                };
                future
            })
            .await?;
        let mut sources = prepared.sources();
        let value = match response {
            Some(response) => convert::goto_locations(&mut sources, encoding, response).await,
            None => Vec::new(),
        };
        Ok(Answer {
            target: resolved,
            value,
        })
    }

    pub async fn definition(&self, target: &Target) -> LspResult<Answer<Vec<Location>>> {
        self.goto(GotoKind::Definition, target).await
    }

    pub async fn type_definition(&self, target: &Target) -> LspResult<Answer<Vec<Location>>> {
        self.goto(GotoKind::TypeDefinition, target).await
    }

    pub async fn implementation(&self, target: &Target) -> LspResult<Answer<Vec<Location>>> {
        self.goto(GotoKind::Implementation, target).await
    }

    /// Every reference to the symbol at `target`.
    pub async fn references(
        &self,
        target: &Target,
        include_declaration: bool,
    ) -> LspResult<Answer<Vec<Location>>> {
        let prepared = self.prepare(&target.path).await?;
        let resolved = prepared.resolve(target)?;
        let (encoding, found) = self
            .first_server(&prepared.handles, "references", |handle| {
                let id = ServerHandle::identifier(&prepared.path)?;
                let position = prepared.position(&resolved, handle.encoding());
                handle
                    .client()
                    .goto_reference(id, position, include_declaration, None)
            })
            .await?;
        let mut sources = prepared.sources();
        let value = convert::locations(&mut sources, encoding, found.unwrap_or_default()).await;
        Ok(Answer {
            target: resolved,
            value,
        })
    }

    /// The documentation and type of the symbol at `target`, as markdown.
    pub async fn hover(&self, target: &Target) -> LspResult<Answer<Option<Hover>>> {
        let prepared = self.prepare(&target.path).await?;
        let resolved = prepared.resolve(target)?;
        let (encoding, hover) = self
            .first_server(&prepared.handles, "hover", |handle| {
                let id = ServerHandle::identifier(&prepared.path)?;
                let position = prepared.position(&resolved, handle.encoding());
                handle.client().text_document_hover(id, position, None)
            })
            .await?;
        let value =
            hover.and_then(|hover| convert::hover(&split_lines(&prepared.text), encoding, hover));
        Ok(Answer {
            target: resolved,
            value,
        })
    }

    /// The symbols declared in one file, nested when the server nests them.
    pub async fn document_symbols(&self, path: &Path) -> LspResult<Vec<Symbol>> {
        let prepared = self.prepare(path).await?;
        let (encoding, response) = self
            .first_server(&prepared.handles, "document symbols", |handle| {
                let id = ServerHandle::identifier(&prepared.path)?;
                handle.client().document_symbols(id)
            })
            .await?;
        let mut sources = prepared.sources();
        Ok(match response {
            Some(response) => {
                convert::document_symbols(&mut sources, encoding, &prepared.path, response).await
            }
            None => Vec::new(),
        })
    }

    /// Symbols across the project whose name matches `query`. `near` names a file of the
    /// language to search (it starts that language's server); without it, servers that are
    /// already running answer.
    pub async fn workspace_symbols(
        &self,
        query: &str,
        near: Option<&Path>,
    ) -> LspResult<Vec<Symbol>> {
        let handles = match near {
            Some(path) => self.prepare(path).await?.handles,
            None => {
                let (reply, answer) = oneshot::channel();
                self.inner
                    .commands
                    .send(Command::Ready { reply })
                    .map_err(|_| LspError::ShutDown)?;
                let running = answer.await.map_err(|_| LspError::ShutDown)?;
                if running.is_empty() {
                    return Err(LspError::NoServer {
                        path: self.inner.workspace_root.clone(),
                        hint: "; no language server is running yet. Pass the path of a source \
                               file to start the server for its language"
                            .to_owned(),
                    });
                }
                running
            }
        };
        let (encoding, response) = self
            .first_server(&handles, "workspace symbols", |handle| {
                handle.client().workspace_symbols(query.to_owned())
            })
            .await?;
        Ok(match response {
            Some(response) => {
                convert::workspace_symbols(&mut Sources::default(), encoding, response).await
            }
            None => Vec::new(),
        })
    }

    /// Computes the edits for renaming the symbol at `target`. Nothing is applied.
    pub async fn rename(&self, target: &Target, new_name: &str) -> LspResult<Answer<EditPreview>> {
        let prepared = self.prepare(&target.path).await?;
        let resolved = prepared.resolve(target)?;
        let (encoding, edit) = self
            .first_server(&prepared.handles, "rename", |handle| {
                let id = ServerHandle::identifier(&prepared.path)?;
                let position = prepared.position(&resolved, handle.encoding());
                handle
                    .client()
                    .rename_symbol(id, position, new_name.to_owned())
            })
            .await?;
        let value = match edit {
            Some(edit) => convert::edit_preview(&mut prepared.sources(), encoding, edit).await,
            None => EditPreview::default(),
        };
        Ok(Answer {
            target: resolved,
            value,
        })
    }

    /// Who calls the function at `target` (incoming) or what it calls (outgoing).
    pub async fn call_hierarchy(
        &self,
        target: &Target,
        direction: CallDirection,
    ) -> LspResult<Answer<Vec<CallItem>>> {
        let prepared = self.prepare(&target.path).await?;
        let resolved = prepared.resolve(target)?;
        let (encoding, roots) = self
            .first_server(&prepared.handles, "call hierarchy", |handle| {
                let id = ServerHandle::identifier(&prepared.path)?;
                let position = prepared.position(&resolved, handle.encoding());
                handle.client().prepare_call_hierarchy(id, position)
            })
            .await?;
        let mut sources = prepared.sources();
        let mut value = Vec::new();
        for root in roots.unwrap_or_default() {
            let root_path = root.uri.to_file_path().ok();
            match direction {
                CallDirection::Incoming => {
                    let (_, calls) = self
                        .first_server(&prepared.handles, "incoming calls", |handle| {
                            handle.client().call_hierarchy_incoming(root.clone())
                        })
                        .await?;
                    for call in calls.unwrap_or_default() {
                        value.extend(
                            convert::call_item(
                                &mut sources,
                                encoding,
                                call.from,
                                call.from_ranges,
                                None,
                            )
                            .await,
                        );
                    }
                }
                CallDirection::Outgoing => {
                    let (_, calls) = self
                        .first_server(&prepared.handles, "outgoing calls", |handle| {
                            handle.client().call_hierarchy_outgoing(root.clone())
                        })
                        .await?;
                    for call in calls.unwrap_or_default() {
                        value.extend(
                            convert::call_item(
                                &mut sources,
                                encoding,
                                call.to,
                                call.from_ranges,
                                root_path.as_deref(),
                            )
                            .await,
                        );
                    }
                }
            }
        }
        Ok(Answer {
            target: resolved,
            value,
        })
    }

    /// The code actions (quick fixes, refactorings) the server offers at `target`. Errors and
    /// warnings on that line are passed along so servers offer their fixes.
    pub async fn code_actions(&self, target: &Target) -> LspResult<Answer<Vec<CodeAction>>> {
        let prepared = self.prepare(&target.path).await?;
        let resolved = prepared.resolve(target)?;
        let lines = split_lines(&prepared.text);
        let on_line: Vec<_> = self
            .diagnostics(&prepared.path)
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.range.line <= resolved.line && resolved.line <= diagnostic.range.end_line
            })
            .collect();
        let (encoding, actions) = self
            .first_server(&prepared.handles, "code actions", |handle| {
                let id = ServerHandle::identifier(&prepared.path)?;
                let encoding = handle.encoding();
                let at = prepared.position(&resolved, encoding);
                let context = lsp::CodeActionContext {
                    diagnostics: on_line
                        .iter()
                        .map(|diagnostic| lsp::Diagnostic {
                            range: lsp::Range::new(
                                to_lsp_position(
                                    &lines,
                                    diagnostic.range.line,
                                    diagnostic.range.column,
                                    encoding,
                                ),
                                to_lsp_position(
                                    &lines,
                                    diagnostic.range.end_line,
                                    diagnostic.range.end_column,
                                    encoding,
                                ),
                            ),
                            severity: Some(match diagnostic.severity {
                                Severity::Error => lsp::DiagnosticSeverity::ERROR,
                                Severity::Warning => lsp::DiagnosticSeverity::WARNING,
                                Severity::Information => lsp::DiagnosticSeverity::INFORMATION,
                                Severity::Hint => lsp::DiagnosticSeverity::HINT,
                            }),
                            source: diagnostic.source.clone(),
                            message: diagnostic.message.clone(),
                            ..Default::default()
                        })
                        .collect(),
                    only: None,
                    trigger_kind: Some(lsp::CodeActionTriggerKind::INVOKED),
                };
                handle
                    .client()
                    .code_actions(id, lsp::Range::new(at, at), context)
            })
            .await?;
        let value = convert::code_actions(
            &mut prepared.sources(),
            encoding,
            actions.unwrap_or_default(),
        )
        .await;
        Ok(Answer {
            target: resolved,
            value,
        })
    }
}
