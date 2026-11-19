//! Validated provider-owned callers of the root session's Nori MCP endpoint.
//!
//! A provider's own child (a native actor) reaches the same tools through the root's endpoint. It
//! shares the root's user control plane, but keeps its own identity in the directory: it owns the
//! tasks it started, its own delegation depth and its own completion destination, and it can
//! only name tasks of its own.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nori_protocol::acp::v1 as acp;
use tokio::sync::Mutex;

use super::Binding;
use super::Inner;
use super::Orchestrator;
use super::polling::PollTracker;

impl Orchestrator {
    pub(crate) fn is_native_caller(&self) -> bool {
        self.inner.caller_view.is_some()
    }

    /// Metadata is supplied by the provider transport, never by tool arguments. A caller
    /// still has to belong to this backend's observed tree; IDs alone confer no authority.
    pub(crate) async fn for_caller(&self, caller: &str) -> Result<Self, String> {
        let root = self
            .inner
            .parent_session()
            .ok_or("The session is not bound")?;
        let caller = acp::SessionId::new(caller);
        if caller == root && self.inner.caller_view.is_none() {
            return Ok(self.clone());
        }
        let backend = self
            .inner
            .backend()
            .ok_or("The caller's backend is unavailable")?;
        // Rebinding invalidates old root metadata even if an old endpoint is still closing.
        if backend.session_id().await != root {
            return Err("The caller's session binding has changed".into());
        }
        // The provider's SSE child announcement and MCP call use independent transports.
        // Allow registration to catch up, but never interpret an unresolved caller as root.
        let registration_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !backend.native_children.owns(&caller).await
            || !backend.native_children.known_ids().await.contains(&caller)
        {
            if let Some(reason) = backend.native_children.unavailable_reason(&caller).await {
                return Err(reason);
            }
            if tokio::time::Instant::now() >= registration_deadline {
                return Err("Unknown or foreign caller session; retry after the provider announces the child".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut cursor = caller.clone();
        let mut seen = HashSet::new();
        let mut depth = self.inner.seed.depth;
        while cursor != root {
            if !seen.insert(cursor.clone()) || !backend.native_children.owns(&cursor).await {
                return Err("Unknown or foreign caller session".into());
            }
            depth = depth.checked_add(1).ok_or("Caller depth overflow")?;
            cursor = backend
                .native_children
                .parent_id(&cursor)
                .await
                .map_err(|error| error.to_string())?;
        }
        self.native_scope(caller, depth).await
    }

    pub(super) async fn native_scope(
        &self,
        caller: acp::SessionId,
        depth: i64,
    ) -> Result<Self, String> {
        let mut scopes = self.inner.native_scopes.lock().await;
        if let Some(scope) = scopes.get(&caller) {
            return Ok(scope.clone());
        }
        let approval_policy = self
            .inner
            .binding
            .lock()
            .map_err(|_| "Session binding is unavailable")?
            .as_ref()
            .ok_or("The session is not bound")?
            .approval_policy
            .clone();
        let mut seed = self.inner.seed.clone();
        seed.depth = depth;
        let program_backend = std::sync::OnceLock::new();
        if let Some(backend) = self.inner.program_backend.get() {
            let _ = program_backend.set(backend.clone());
        }
        let signals = self
            .inner
            .signals
            .lock()
            .map_err(|_| "Session signals are unavailable")?
            .clone();
        let scope = Self {
            inner: Arc::new(Inner {
                seed,
                program_backend,
                events: self.inner.events.clone(),
                binding: std::sync::Mutex::new(Some(Binding {
                    session_id: caller.clone(),
                    approval_policy,
                })),
                signals: std::sync::Mutex::new(signals),
                admission: self.inner.admission.clone(),
                generation: self.inner.generation.clone(),
                shut_down: self.inner.shut_down.clone(),
                caller_view: Some(caller.clone()),
                native_scopes: Mutex::new(std::collections::HashMap::new()),
                polls: std::sync::Mutex::new(PollTracker::default()),
                // Never armed through a provider-owned caller: its monitor tools are refused.
                monitors: self.inner.monitors.clone(),
            }),
        };
        let backend = self
            .inner
            .program_backend
            .get()
            .cloned()
            .unwrap_or_default();
        let root = self.inner.parent_session();
        let provider = self
            .inner
            .backend()
            .map(|backend| backend.agent_name().to_owned());
        let parent_session = match self.inner.backend() {
            Some(backend) => backend.native_children.parent_id(&caller).await.ok(),
            None => None,
        }
        .or(root);
        if let Some(parent_session) = parent_session {
            self.inner.seed.tree.directory.with(|registry| {
                registry.observe_native(
                    &caller,
                    &parent_session,
                    provider.as_deref(),
                    None,
                    None,
                    None,
                    None,
                    &backend,
                );
            });
        }
        scopes.insert(caller, scope.clone());
        Ok(scope)
    }

    /// Hand each provider-owned caller the results of the tasks it delegated, even while the root
    /// session is busy: a native child may be waiting on them inside a provider Task call.
    pub(crate) async fn deliver_native_caller_results(&self) {
        let Some(backend) = self.inner.backend() else {
            return;
        };
        let scopes: Vec<_> = self
            .inner
            .native_scopes
            .lock()
            .await
            .values()
            .cloned()
            .collect();
        for scope in scopes {
            let Some(view) = &scope.inner.caller_view else {
                continue;
            };
            let Some(wake) = scope.take_wake().await else {
                continue;
            };
            if let Err(error) = backend
                .native_children
                .enqueue(
                    view,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(wake.text))],
                )
                .await
            {
                tracing::warn!(%error, %view, "failed to deliver results to native caller");
                // Keep the receipt in the caller's journal instead of waking the root actor.
                if let Ok(recorder) = backend.native_children.history(view).await {
                    let _ = recorder
                        .record_command("native-delegation-delivery-failed", error.to_string())
                        .await;
                }
            }
        }
    }
}
