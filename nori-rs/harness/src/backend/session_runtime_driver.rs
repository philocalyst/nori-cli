//! The driver that owns a session's [`SessionRuntime`] and the actor that serializes everything
//! that touches it.

use super::*;

use crate::normalized::ClientEvent;
use crate::normalized::ClientEventNormalizer;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::SessionPhaseView;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::Turn;
use crate::normalized::turn::TurnOwner;

use super::session_reducer::AgentTurnEnded;
use super::session_reducer::InboundEvent;
use super::session_reducer::SideEffect;
use super::session_reducer::Wake;
use super::session_reducer::reduce;

pub(crate) use super::session_reducer::CompletedTurn;

tokio::task_local! {
    /// Set inside the runtime actor: the ledger's only writer must not wait on itself.
    pub(super) static IN_RUNTIME_ACTOR: ();
}

pub(crate) struct SessionDriver {
    runtime: SessionRuntime,
    normalizer: ClientEventNormalizer,
}

pub(crate) enum SessionRuntimeInput {
    Reducer(InboundEvent),
    /// A prompt nobody typed asks to start; see [`AcpBackend::offer_prompt`].
    Offer {
        generation: Option<i64>,
        prompt: Box<QueuedPrompt>,
        admitted: tokio::sync::oneshot::Sender<bool>,
    },
    Barrier {
        response_tx: tokio::sync::oneshot::Sender<()>,
    },
    QuestionCancelled(Box<nori_protocol::AcpEvent>),
    PermissionRequest {
        pending_request: Box<PendingApprovalRequest>,
        current_policy: AskForApproval,
    },
    /// A wake-up the reducer scheduled has come due.
    Wake(Wake),
}

pub(crate) struct ReducerActions {
    pub events: Vec<ClientEvent>,
    pub side_effects: Vec<SideEffect>,
    pub completed_turn: Option<CompletedTurn>,
    pub agent_ended: Option<AgentTurnEnded>,
    pub queue_ops: Vec<super::session_reducer::QueueOp>,
}

impl SessionDriver {
    pub(crate) fn new() -> Self {
        Self {
            runtime: SessionRuntime::new(),
            normalizer: ClientEventNormalizer::default(),
        }
    }

    /// Take in what the agent announced about the session while it was being set up (its
    /// commands, mode, settings, context usage), before the ledger ran: the ledger starts knowing
    /// it. Turn activity of setup is not the ledger's, and what the reduction says was already
    /// published as the setup's own events.
    pub(crate) fn absorb_setup<'a>(
        &mut self,
        updates: impl IntoIterator<Item = &'a acp::SessionUpdate>,
    ) {
        for update in updates {
            if super::turn::metadata::is_session_metadata(update) {
                self.apply(InboundEvent::Notification(Box::new(update.clone())));
            }
        }
    }

    pub(crate) fn apply(&mut self, event: InboundEvent) -> ReducerActions {
        let out = reduce(&mut self.runtime, event, &mut self.normalizer);
        ReducerActions {
            events: out.events,
            side_effects: out.side_effects,
            completed_turn: out.completed,
            agent_ended: out.agent_ended,
            queue_ops: out.queue_ops,
        }
    }

    pub(crate) fn active_request_id(&self) -> Option<String> {
        self.active_wire_request_id()
            .map(|request_id| request_id.to_string())
    }

    /// The request that owns the open turn, if a request does.
    pub(crate) fn active_wire_request_id(&self) -> Option<nori_protocol::acp::v1::RequestId> {
        self.runtime
            .turn
            .as_ref()
            .and_then(|turn| turn.request_id().cloned())
    }

    pub(crate) fn phase_label(&self) -> &'static str {
        self.runtime.phase_view().label()
    }

    pub(crate) fn queue_len(&self) -> usize {
        self.runtime.queue.len()
    }

    /// A turn is open, whoever owns it, prompts are waiting to start, or a session swap is in
    /// flight.
    pub(crate) fn prompt_is_busy(&self) -> bool {
        self.runtime.phase_view() != SessionPhaseView::Idle
            || !self.runtime.queue.is_empty()
            || self.runtime.fenced
    }

    /// The ledger sequence number of the local turn, if the open turn is one.
    pub(crate) fn local_turn_seq(&self) -> Option<i64> {
        self.runtime
            .turn
            .as_ref()
            .filter(|turn| matches!(turn.owner, TurnOwner::Local { .. }))
            .map(|turn| turn.seq)
    }

    /// The loop and compaction policies in force, from the next tool call and turn end on.
    pub(crate) fn set_policy(
        &mut self,
        loops: nori_workflow::LoopPolicy,
        compaction: nori_workflow::CompactionPolicy,
    ) {
        self.runtime.loops.policy = loops;
        self.runtime.compaction.policy = compaction;
    }

    /// A compaction ended. A compacted session is measured again from the agent's next report.
    /// Returns whether automatic compaction stopped for this session because of it.
    pub(crate) fn compaction_ended(
        &mut self,
        end: crate::backend::turn::pressure::CompactionEnd,
    ) -> bool {
        let used_tokens = self
            .runtime
            .context_usage
            .map_or(0, |usage| usage.used_tokens);
        if end == crate::backend::turn::pressure::CompactionEnd::Compacted {
            self.runtime.context_usage = None;
        }
        self.runtime.compaction.ended(end, used_tokens)
    }

    /// A turn of any owner is open.
    pub(crate) fn has_open_turn(&self) -> bool {
        self.runtime.turn.is_some()
    }

    /// A session swap holds the fence.
    pub(crate) fn is_fenced(&self) -> bool {
        self.runtime.fenced
    }

    /// Stop prompts from starting while the session is swapped, if no turn is open. What is
    /// queued stays queued and starts when the fence is released. The check and the fence are
    /// one step under the driver's lock, which every change to the ledger also takes. Release
    /// with [`InboundEvent::ReleaseFence`].
    pub(crate) fn try_fence(&mut self) -> bool {
        let free = self.runtime.turn.is_none() && !self.runtime.fenced;
        self.runtime.fenced |= free;
        free
    }

    /// The prompt of the local turn, if the open turn is one.
    pub(crate) fn local_prompt(&self) -> Option<&QueuedPrompt> {
        match self.runtime.turn.as_ref().map(|turn| &turn.owner) {
            Some(TurnOwner::Local { prompt, .. }) => Some(prompt),
            Some(TurnOwner::Replay { .. } | TurnOwner::Agent { .. }) | None => None,
        }
    }

    pub(crate) fn owns_active_prompt_echo(&self, chunk: &acp::ContentChunk) -> bool {
        let Some(owner_event_id) = chunk
            .meta
            .as_ref()
            .and_then(|meta| meta.get(nori_protocol::PROMPT_ECHO_ID_META_KEY))
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };

        self.local_prompt().is_some_and(|prompt| {
            prompt
                .prompt_meta
                .as_ref()
                .and_then(|meta| meta.get(nori_protocol::PROMPT_ECHO_ID_META_KEY))
                .and_then(serde_json::Value::as_str)
                == Some(owner_event_id)
                && (prompt.content.contains(&chunk.content)
                    // The prompt waits here as the person wrote it; what Nori puts around it
                    // is added as it is sent, and an agent echoes that too.
                    || super::prompt_context::is_injected(&chunk.content))
        })
    }

    /// Whether the connected agent currently advertises a slash command with
    /// the given name.
    pub(crate) fn advertises_command(&self, name: &str) -> bool {
        self.runtime
            .available_commands
            .iter()
            .any(|command| command.name == name)
    }

    /// The phase published to clients.
    pub(crate) fn public_phase(&self) -> nori_protocol::SessionPhase {
        self.runtime
            .turn
            .as_ref()
            .map_or(nori_protocol::SessionPhase::Idle, Turn::phase)
    }
}

impl Default for SessionDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpBackend {
    /// The agent is gone: end whatever turn is open and wait until the ledger has done so, so
    /// nothing is torn down under it.
    pub(super) async fn connection_lost(&self) {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        for input in [
            SessionRuntimeInput::Reducer(InboundEvent::ConnectionLost),
            SessionRuntimeInput::Barrier { response_tx },
        ] {
            if self.session_event_tx.send(input).await.is_err() {
                return;
            }
        }
        let _ = response_rx.await;
    }

    /// Spawn the actor that applies every input to the session runtime one at a time. Nothing
    /// else mutates the runtime, so the ledger is the single arbiter of what owns the session.
    pub(super) fn spawn_runtime_actor(
        &self,
        mut inputs: mpsc::Receiver<SessionRuntimeInput>,
    ) -> tokio::task::AbortHandle {
        let backend = self.clone();
        tokio::spawn(IN_RUNTIME_ACTOR.scope((), async move {
            while let Some(input) = inputs.recv().await {
                match input {
                    SessionRuntimeInput::Reducer(event) => {
                        backend.apply_session_event(event).await;
                    }
                    SessionRuntimeInput::Offer {
                        generation,
                        prompt,
                        admitted,
                    } => {
                        let started = backend.admit_offer(generation, *prompt).await;
                        let _ = admitted.send(started);
                    }
                    SessionRuntimeInput::Barrier { response_tx } => {
                        let _ = response_tx.send(());
                    }
                    SessionRuntimeInput::QuestionCancelled(event) => {
                        backend.withdraw_cancelled_question(&event).await;
                    }
                    SessionRuntimeInput::PermissionRequest {
                        pending_request,
                        current_policy,
                    } => {
                        backend
                            .handle_permission_request(pending_request, current_policy)
                            .await;
                    }
                    SessionRuntimeInput::Wake(wake) => {
                        let live_requests = backend.live_request_ids().await;
                        backend
                            .apply_session_event(InboundEvent::Woke {
                                wake,
                                live_requests,
                            })
                            .await;
                    }
                }
            }
        }))
        .abort_handle()
    }
}
