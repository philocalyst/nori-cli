//! Serialize prompts and controls for provider-owned child sessions.
use super::native_child_history::NativeHistory;
use crate::connection::acp_connection::AcpConnection;
use anyhow::Result;
use anyhow::anyhow;
use nori_protocol::Patch;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

type Content = Vec<acp::ContentBlock>;
type Reply = oneshot::Sender<Result<()>>;
enum Command {
    State(bool),
    ConfigSnapshot(Vec<acp::SessionConfigOption>),
    GetConfig(oneshot::Sender<Vec<acp::SessionConfigOption>>),
    SetConfig(
        acp::SessionConfigId,
        acp::SessionConfigValueId,
        oneshot::Sender<Result<Vec<acp::SessionConfigOption>>>,
    ),
    Enqueue(Content, Reply),
    Steer(Content, Reply),
    Promote(Reply),
    Cancel(Reply),
    Finished,
    Busy(oneshot::Sender<bool>),
    Reserve(Reply),
    CheckReservation(Reply),
    Release,
    Closed,
    Shutdown(oneshot::Sender<()>),
}

#[derive(Default)]
pub(super) struct NativeChildren {
    actors: Mutex<HashMap<acp::SessionId, mpsc::UnboundedSender<Command>>>,
    suppressed: Mutex<HashSet<acp::SessionId>>,
    parents: Mutex<HashMap<acp::SessionId, acp::SessionId>>,
    snapshots: Mutex<HashMap<acp::SessionId, Vec<acp::SessionConfigOption>>>,
    closed: Mutex<HashSet<acp::SessionId>>,
    archived: Mutex<HashMap<acp::SessionId, String>>,
    event_gate: Mutex<()>,
    control_gates: Mutex<HashMap<acp::SessionId, Arc<Mutex<()>>>>,
    history: Arc<RwLock<Option<Arc<NativeHistory>>>>,
    own_prompts: Arc<Mutex<HashSet<acp::SessionId>>>,
    /// The tree's directory, where every provider child is registered as an actor.
    directory: std::sync::OnceLock<(
        Arc<super::orchestrator::Tree>,
        std::sync::Weak<super::AcpBackend>,
    )>,
}

impl NativeChildren {
    /// Register provider children in `tree`'s directory as they are announced; `backend` is the
    /// connection that owns them.
    pub(super) fn attach_directory(
        &self,
        tree: Arc<super::orchestrator::Tree>,
        backend: std::sync::Weak<super::AcpBackend>,
    ) {
        let _ = self.directory.set((tree, backend));
    }

    /// Mirror an announcement into the directory: the child is an addressable actor, and its
    /// lifecycle follows the provider's reports.
    fn register_actor(&self, update: &nori_protocol::SubagentUpdate) {
        let Some((tree, backend)) = self.directory.get() else {
            return;
        };
        fn text(patch: &Patch<String>) -> Option<&str> {
            match patch {
                Patch::Set(text) => Some(text.as_str()),
                Patch::Keep | Patch::Clear => None,
            }
        }
        let (running, ended) = match &update.state {
            Patch::Set(SubagentState::Running) => (Some(true), None),
            Patch::Set(SubagentState::Idle) => (Some(false), None),
            Patch::Set(SubagentState::Ended(outcome)) => (None, Some(outcome.clone())),
            Patch::Keep | Patch::Clear => (None, None),
        };
        tree.directory.observe_native(
            &update.session_id,
            &update.parent_session_id,
            text(&update.agent),
            text(&update.title),
            text(&update.model),
            running,
            ended,
            backend,
        );
    }

    pub(super) async fn observe(
        &self,
        connection: &Arc<AcpConnection>,
        events: &mpsc::Sender<super::BackendEvent>,
        root: &acp::SessionId,
        wire: &SubagentWire,
    ) -> bool {
        let _gate = self.event_gate.lock().await;
        if self.is_wire_suppressed(wire).await {
            return false;
        }
        let SubagentWire::Update(update) = wire else {
            if let SubagentWire::Message(message) = wire
                && let Some(id) = &message.recipient_session_id
                && self.actors.lock().await.contains_key(id)
                && let Some(history) = self.history.read().await.clone()
                && let Err(error) = history.record_wire(wire).await
            {
                tracing::warn!(%error, "failed to record native child message");
            }
            return true;
        };
        self.register_actor(update);
        let mut actors = self.actors.lock().await;
        if matches!(
            &update.state,
            Patch::Set(SubagentState::Ended(nori_protocol::SubagentOutcome::Closed))
        ) {
            if let Some(history) = self.history.read().await.clone()
                && let Err(error) = history.record_wire(wire).await
            {
                tracing::warn!(%error, "failed to record native child close");
            }
            self.closed.lock().await.insert(update.session_id.clone());
            if let Some(actor) = actors.remove(&update.session_id) {
                let _ = actor.send(Command::Closed);
            }
            return true;
        }
        if &update.parent_session_id != root
            && !actors.contains_key(&update.parent_session_id)
            && !self
                .archived
                .lock()
                .await
                .contains_key(&update.parent_session_id)
        {
            return true;
        }
        self.closed.lock().await.remove(&update.session_id);
        self.archived.lock().await.remove(&update.session_id);
        self.parents
            .lock()
            .await
            .entry(update.session_id.clone())
            .or_insert_with(|| update.parent_session_id.clone());
        if update.session_id != update.parent_session_id
            && let Some(history) = self.history.read().await.clone()
        {
            let reference = if &update.parent_session_id == root {
                history.recorder(&update.session_id).await.map(drop)
            } else {
                history
                    .record_descendant_reference(&update.parent_session_id, &update.session_id)
                    .await
            };
            if let Err(error) = reference {
                tracing::warn!(%error, "failed to record initial native child reference");
            }
        }
        if &update.parent_session_id != root
            && let Some(history) = self.history.read().await.clone()
            && let Err(error) = history
                .record_descendant_event(
                    &update.parent_session_id,
                    nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                        wire.clone(),
                    )),
                )
                .await
        {
            tracing::warn!(%error, "failed to record nested child lifecycle");
        }
        if !self.own_prompts.lock().await.contains(&update.session_id)
            && let Some(history) = self.history.read().await.clone()
            && let Err(error) = history.record_wire(wire).await
        {
            tracing::warn!(%error, "failed to record native child event");
        }
        let tx = actors.entry(update.session_id.clone()).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(run(
                NativeActor {
                    connection: connection.clone(),
                    events: events.clone(),
                    parent: update.parent_session_id.clone(),
                    id: update.session_id.clone(),
                    tx: tx.downgrade(),
                    history: self.history.clone(),
                    own_prompts: self.own_prompts.clone(),
                },
                rx,
            ));
            tx
        });
        match &update.state {
            Patch::Set(SubagentState::Running) => {
                let _ = tx.send(Command::State(true));
            }
            Patch::Set(SubagentState::Idle | SubagentState::Ended(_)) => {
                let _ = tx.send(Command::State(false));
            }
            Patch::Clear | Patch::Keep => {}
        }
        true
    }

    /// Historical announcements identify children but do not prove that old work is still active.
    pub(super) async fn observe_replayed(
        &self,
        connection: &Arc<AcpConnection>,
        events: &mpsc::Sender<super::BackendEvent>,
        root: &acp::SessionId,
        wire: &SubagentWire,
    ) {
        let SubagentWire::Update(update) = wire else {
            return;
        };
        if update.session_id.to_string().starts_with("nori-task-") {
            return;
        }
        let mut update = update.clone();
        if matches!(&update.state, Patch::Set(SubagentState::Running)) {
            update.state = Patch::Set(SubagentState::Idle);
        }
        let _ = self
            .observe(connection, events, root, &SubagentWire::Update(update))
            .await;
    }

    pub(super) async fn observe_notification(
        &self,
        notification: &acp::SessionNotification,
    ) -> bool {
        let _gate = self.event_gate.lock().await;
        if self.is_suppressed(&notification.session_id).await {
            return false;
        }
        let own_echo = matches!(
            &notification.update,
            acp::SessionUpdate::UserMessageChunk(_)
        ) && self
            .own_prompts
            .lock()
            .await
            .contains(&notification.session_id);
        if !own_echo
            && self
                .actors
                .lock()
                .await
                .contains_key(&notification.session_id)
            && let Some(history) = self.history.read().await.clone()
            && let Err(error) = history.record_notification(notification).await
        {
            tracing::warn!(%error, "failed to record native child update");
        }
        if let acp::SessionUpdate::ConfigOptionUpdate(update) = &notification.update {
            self.snapshots.lock().await.insert(
                notification.session_id.clone(),
                update.config_options.clone(),
            );
            let parents = self.parents.lock().await.clone();
            if let Some(parent) = parents.get(&notification.session_id)
                && let Ok((model, options)) =
                    super::child_control::child_model_and_options(update.config_options.clone())
                && let Some(history) = self.history.read().await.clone()
                && let Err(error) = if parents.contains_key(parent) {
                    history
                        .record_descendant_binding(parent, &notification.session_id, model, options)
                        .await
                } else {
                    history
                        .record_direct_binding(parent, &notification.session_id, model, options)
                        .await
                }
            {
                tracing::warn!(%error, "failed to record nested child binding");
            }
            if let Some(actor) = self.actors.lock().await.get(&notification.session_id) {
                let _ = actor.send(Command::ConfigSnapshot(update.config_options.clone()));
            }
        }
        true
    }

    pub(super) async fn config_options(
        &self,
        id: &acp::SessionId,
    ) -> Result<Vec<acp::SessionConfigOption>> {
        if let Some(reason) = self.unavailable_reason(id).await {
            return Err(anyhow!(reason));
        }
        if let Ok(actor) = self.actor(id).await {
            let (tx, rx) = oneshot::channel();
            actor
                .send(Command::GetConfig(tx))
                .map_err(|_| anyhow!("Child session closed"))?;
            rx.await.map_err(|_| anyhow!("Child session closed"))
        } else {
            self.snapshots
                .lock()
                .await
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow!("Child session configuration is unavailable"))
        }
    }

    pub(super) async fn set_config_option(
        &self,
        id: &acp::SessionId,
        config_id: acp::SessionConfigId,
        value: acp::SessionConfigValueId,
    ) -> Result<Vec<acp::SessionConfigOption>> {
        let (tx, rx) = oneshot::channel();
        self.actor(id)
            .await?
            .send(Command::SetConfig(config_id, value, tx))
            .map_err(|_| anyhow!("Child session closed"))?;
        let options = rx.await.map_err(|_| anyhow!("Child session closed"))??;
        self.snapshots
            .lock()
            .await
            .insert(id.clone(), options.clone());
        Ok(options)
    }

    pub(super) async fn configure_history(&self, history: Arc<NativeHistory>) {
        *self.history.write().await = Some(history);
    }

    pub(super) async fn rebound_binding(
        &self,
        id: &acp::SessionId,
    ) -> Result<Option<super::native_child_history::ReboundBinding>> {
        match self.history.read().await.clone() {
            Some(history) => history.rebound_binding(id).await,
            None => Ok(None),
        }
    }
    pub(super) async fn bind_rebound(
        &self,
        id: &acp::SessionId,
        recorder: Arc<super::TranscriptRecorder>,
        binding: super::native_child_history::ReboundBinding,
    ) -> Result<()> {
        self.history
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("This child has no conversation journal"))?
            .bind_rebound(id, recorder, binding)
            .await
    }
    pub(super) async fn bind_native(
        &self,
        id: &acp::SessionId,
        recorder: Arc<super::TranscriptRecorder>,
    ) -> Result<()> {
        self.history
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("This child has no conversation journal"))?
            .bind_native(id, recorder)
            .await
    }
    pub(super) async fn unavailable_reason(&self, id: &acp::SessionId) -> Option<String> {
        self.archived.lock().await.get(id).cloned()
    }

    pub(super) async fn register_archived(
        &self,
        id: acp::SessionId,
        parent: acp::SessionId,
        reason: String,
    ) {
        let _gate = self.event_gate.lock().await;
        self.parents.lock().await.insert(id.clone(), parent);
        self.closed.lock().await.insert(id.clone());
        self.archived.lock().await.insert(id.clone(), reason);
        if let Some(actor) = self.actors.lock().await.remove(&id) {
            let _ = actor.send(Command::Closed);
        }
    }

    pub(super) async fn known_ids(&self) -> Vec<acp::SessionId> {
        self.parents.lock().await.keys().cloned().collect()
    }

    pub(super) async fn bootstrap_history(
        &self,
        transcript: &crate::transcript::Transcript,
    ) -> Result<()> {
        let ids = self.actors.lock().await.keys().cloned().collect::<Vec<_>>();
        if let Some(history) = self.history.read().await.clone() {
            history.bootstrap(transcript, &ids).await?;
        }
        Ok(())
    }

    pub(super) async fn history(
        &self,
        id: &acp::SessionId,
    ) -> Result<Arc<super::TranscriptRecorder>> {
        if !self.parents.lock().await.contains_key(id) {
            return Err(anyhow!("Unknown native child session"));
        }
        self.history
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("This child has no conversation journal"))?
            .recorder(id)
            .await
    }

    async fn actor(&self, id: &acp::SessionId) -> Result<mpsc::UnboundedSender<Command>> {
        if let Some(reason) = self.unavailable_reason(id).await {
            return Err(anyhow!(reason));
        }
        self.actors
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown native child session"))
    }
    pub(super) async fn enqueue(&self, id: &acp::SessionId, content: Content) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.actor(id)
            .await?
            .send(Command::Enqueue(content, tx))
            .map_err(|_| anyhow!("Child session closed"))?;
        rx.await.map_err(|_| anyhow!("Child session closed"))?
    }
    pub(super) async fn steer(&self, id: &acp::SessionId, content: Content) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.actor(id)
            .await?
            .send(Command::Steer(content, tx))
            .map_err(|_| anyhow!("Child session closed"))?;
        rx.await.map_err(|_| anyhow!("Child session closed"))?
    }
    pub(super) async fn promote(&self, id: &acp::SessionId) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.actor(id)
            .await?
            .send(Command::Promote(tx))
            .map_err(|_| anyhow!("Child session closed"))?;
        rx.await.map_err(|_| anyhow!("Child session closed"))?
    }
    pub(super) async fn cancel(&self, id: &acp::SessionId) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.actor(id)
            .await?
            .send(Command::Cancel(tx))
            .map_err(|_| anyhow!("Child session closed"))?;
        rx.await.map_err(|_| anyhow!("Child session closed"))?
    }
    pub(super) async fn owns(&self, id: &acp::SessionId) -> bool {
        self.parents.lock().await.contains_key(id) && !self.is_suppressed(id).await
    }
    pub(super) async fn parent_id(&self, id: &acp::SessionId) -> Result<acp::SessionId> {
        self.parents
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown native child parent"))
    }
    pub(super) async fn control_gate(
        &self,
        id: &acp::SessionId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = self
            .control_gates
            .lock()
            .await
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        gate.lock_owned().await
    }
    pub(super) async fn adoption_gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.event_gate.lock().await
    }
    pub(super) async fn reserve(&self, id: &acp::SessionId) -> Result<()> {
        if self.closed.lock().await.contains(id) {
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.actor(id)
            .await?
            .send(Command::Reserve(tx))
            .map_err(|_| anyhow!("Child session closed"))?;
        rx.await.map_err(|_| anyhow!("Child session closed"))?
    }
    pub(super) async fn check_reservation(&self, id: &acp::SessionId) -> Result<()> {
        if self.closed.lock().await.contains(id) {
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.actor(id)
            .await?
            .send(Command::CheckReservation(tx))
            .map_err(|_| anyhow!("Child session closed"))?;
        rx.await.map_err(|_| anyhow!("Child session closed"))?
    }
    pub(super) async fn release(&self, id: &acp::SessionId) {
        if let Ok(actor) = self.actor(id).await {
            let _ = actor.send(Command::Release);
        }
    }
    pub(super) async fn is_wire_suppressed(&self, wire: &SubagentWire) -> bool {
        match wire {
            SubagentWire::Update(update) => {
                self.is_suppressed(&update.session_id).await
                    || self.is_suppressed(&update.parent_session_id).await
            }
            SubagentWire::Message(message) => {
                if let Some(id) = &message.recipient_session_id
                    && self.is_suppressed(id).await
                {
                    return true;
                }
                if let Some(id) = &message.sender_session_id
                    && self.is_suppressed(id).await
                {
                    return true;
                }
                false
            }
        }
    }
    pub(super) async fn is_suppressed(&self, id: &acp::SessionId) -> bool {
        self.suppressed.lock().await.contains(id)
    }
    pub(super) async fn suppress(&self, id: &acp::SessionId, value: bool) {
        let mut suppressed = self.suppressed.lock().await;
        if value {
            suppressed.insert(id.clone());
        } else {
            suppressed.remove(id);
        }
    }
    pub(super) async fn subtree(&self, id: &acp::SessionId) -> Vec<acp::SessionId> {
        let parents = self.parents.lock().await;
        let mut ids = vec![id.clone()];
        let mut index = 0;
        while index < ids.len() {
            for (child, parent) in parents.iter() {
                if parent == &ids[index] && !ids.contains(child) {
                    ids.push(child.clone());
                }
            }
            index += 1;
        }
        ids
    }
    pub(super) async fn retire(&self, id: &acp::SessionId) {
        if let Some(actor) = self.actors.lock().await.remove(id) {
            let _ = actor.send(Command::Closed);
        }
    }

    pub(crate) async fn has_pending_work(&self) -> bool {
        let actors = self
            .actors
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for actor in actors {
            let (tx, rx) = oneshot::channel();
            if actor.send(Command::Busy(tx)).is_err() || rx.await.unwrap_or(true) {
                return true;
            }
        }
        false
    }
    pub(super) async fn shutdown(&self) {
        self.suppressed.lock().await.clear();
        self.parents.lock().await.clear();
        self.snapshots.lock().await.clear();
        self.closed.lock().await.clear();
        self.archived.lock().await.clear();
        let mut acknowledgements = Vec::new();
        for actor in std::mem::take(&mut *self.actors.lock().await).into_values() {
            let (tx, rx) = oneshot::channel();
            if actor.send(Command::Shutdown(tx)).is_ok() {
                acknowledgements.push(rx);
            }
        }
        for reply in acknowledgements {
            let _ = reply.await;
        }
        if let Some(history) = self.history.read().await.clone() {
            history.shutdown().await;
        }
    }
}

/// What a provider child's actor works with, besides the commands it is sent.
struct NativeActor {
    connection: Arc<AcpConnection>,
    events: mpsc::Sender<super::BackendEvent>,
    parent: acp::SessionId,
    id: acp::SessionId,
    /// The actor's own command channel, to tell itself a prompt finished.
    tx: mpsc::WeakUnboundedSender<Command>,
    history: Arc<RwLock<Option<Arc<NativeHistory>>>>,
    own_prompts: Arc<Mutex<HashSet<acp::SessionId>>>,
}

async fn run(actor: NativeActor, mut rx: mpsc::UnboundedReceiver<Command>) {
    let NativeActor {
        connection,
        events,
        parent,
        id,
        tx,
        history,
        own_prompts,
    } = actor;

    let mut pending = VecDeque::new();
    let mut provider_running = true;
    let mut reserved = false;
    let mut config_options = Vec::new();
    let mut own_running = false;
    let mut last_queue: Vec<String> = Vec::new();
    let mut prompt_task: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(command) = rx.recv().await {
        match command {
            Command::State(running) => provider_running = running,
            Command::ConfigSnapshot(options) => config_options = options,
            Command::GetConfig(reply) => {
                let _ = reply.send(config_options.clone());
            }
            Command::SetConfig(config_id, value, reply) => {
                let result = if reserved {
                    Err(anyhow!("Child history is being rewound"))
                } else {
                    connection
                        .set_child_config_option(&id, config_id, value)
                        .await
                };
                if let Ok(options) = &result {
                    config_options = options.clone();
                }
                let _ = reply.send(result);
            }
            Command::Enqueue(content, reply) => {
                if reserved {
                    let _ = reply.send(Err(anyhow!("Child history is being rewound")));
                } else {
                    pending.push_back(content);
                    let _ = reply.send(Ok(()));
                }
            }
            Command::Steer(content, reply) => {
                if reserved {
                    let _ = reply.send(Err(anyhow!("Child history is being rewound")));
                } else {
                    pending.push_front(content);
                    if provider_running || own_running {
                        let _ = connection.cancel(&id).await;
                    }
                    let _ = reply.send(Ok(()));
                }
            }
            Command::Promote(reply) => {
                let result = if reserved {
                    Err(anyhow!("Child history is being rewound"))
                } else if pending.is_empty() {
                    Err(anyhow!("There is no queued child message"))
                } else if provider_running || own_running {
                    connection.cancel(&id).await
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Command::Cancel(reply) => {
                let result = if reserved {
                    Err(anyhow!("Child history is being rewound"))
                } else if provider_running || own_running {
                    connection.cancel(&id).await
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Command::Finished => {
                own_running = false;
                provider_running = false;
                prompt_task = None;
            }
            Command::Busy(reply) => {
                let _ =
                    reply.send(reserved || provider_running || own_running || !pending.is_empty());
            }
            Command::Reserve(reply) => {
                let result = if reserved || provider_running || own_running || !pending.is_empty() {
                    Err(anyhow!(
                        "Cannot rewind while child work is active or queued"
                    ))
                } else {
                    reserved = true;
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Command::CheckReservation(reply) => {
                let _ = reply.send(
                    if reserved && !provider_running && !own_running && pending.is_empty() {
                        Ok(())
                    } else {
                        Err(anyhow!("Child work changed during rewind"))
                    },
                );
            }
            Command::Release => reserved = false,
            Command::Closed => {
                if let Some(task) = prompt_task.take() {
                    task.abort();
                }
                own_prompts.lock().await.remove(&id);
                break;
            }
            Command::Shutdown(reply) => {
                if provider_running || own_running {
                    let _ = connection.cancel(&id).await;
                }
                if let Some(task) = prompt_task.take() {
                    task.abort();
                }
                own_prompts.lock().await.remove(&id);
                let _ = reply.send(());
                break;
            }
        }
        if !reserved
            && !provider_running
            && !own_running
            && let Some(content) = pending.pop_front()
        {
            own_running = true;
            own_prompts.lock().await.insert(id.clone());
            let connection = connection.clone();
            let id = id.clone();
            let tx = tx.clone();
            let events = events.clone();
            let parent = parent.clone();
            let history = history.clone();
            let own_prompts = own_prompts.clone();
            prompt_task = Some(tokio::spawn(async move {
                let journal = history.read().await.clone();
                let result = async {
                    if let Some(journal) = &journal {
                        journal.record_user(&id, &content).await?;
                    }
                    let result = connection.prompt(id.clone(), content).await;
                    connection.event_barrier().await?;
                    if let Some(journal) = &journal {
                        let outcome = match &result {
                            Ok(reason) => Ok(*reason),
                            Err(error) => Err(format!("{error:#}")),
                        };
                        journal.finish(&id, outcome).await?;
                    }
                    result.map(drop)
                }
                .await;
                if let Err(error) = result {
                    let update = nori_protocol::SubagentUpdate {
                        parent_session_id: parent,
                        session_id: id.clone(),
                        agent: Patch::Keep,
                        model: Patch::Keep,
                        title: Patch::Keep,
                        description: Patch::Keep,
                        state: Patch::Set(SubagentState::Ended(
                            nori_protocol::SubagentOutcome::Failed {
                                reason: Some(error.to_string()),
                            },
                        )),
                    };
                    let _ = events
                        .send(super::BackendEvent::Public(
                            nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                                SubagentWire::Update(update),
                            )),
                        ))
                        .await;
                }
                own_prompts.lock().await.remove(&id);
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(Command::Finished);
                }
            }));
        }
        let prompts = pending
            .iter()
            .map(|content: &Content| {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    "[attachment]".to_owned()
                } else {
                    text
                }
            })
            .collect::<Vec<_>>();
        if prompts != last_queue {
            last_queue = prompts.clone();
            let _ = events
                .send(super::BackendEvent::Public(
                    nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SubagentQueueChanged {
                            session_id: id.clone(),
                            prompts,
                        },
                    ),
                ))
                .await;
        }
    }
}
