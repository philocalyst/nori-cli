//! The subagents of one conversation: who is running, under whom, and who finished.
//!
//! Every operation costs O(1) map work plus the size of the update it applies, except
//! [`SubagentTree::on_root_turn_end`], which visits each running child once. Finished children
//! leave the tree as [`FinishedSubagent`]s; their ids stay mapped to [`Link::Ended`] so late
//! updates for them are absorbed instead of resurrecting them.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::Patch;
use nori_protocol::SubagentMessage;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::acp::v1 as acp;
use nori_protocol::acp::v1::SessionId;

use super::node::CellContext;
use super::node::Identity;
use super::node::Node;
use super::node::NodeId;
use crate::history_cell::HistoryCell;
use crate::presentation::Artifact;
use crate::presentation::ClientEvent;
use crate::presentation::ClientEventNormalizer;
use crate::presentation::ToolPhase;
use crate::presentation::ToolSnapshot;

mod reactivation;

/// A child that finished, ready to become a scrollback cell.
#[derive(Debug, Clone)]
pub(crate) struct FinishedSubagent {
    pub(crate) node_id: NodeId,
    pub(crate) label: String,
    pub(crate) kind: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) outcome: SubagentOutcome,
    pub(crate) tool_count: usize,
    pub(crate) elapsed: Duration,
    /// What the child said after its last tool call.
    pub(crate) message: String,
    pub(crate) detail: Vec<Arc<dyn HistoryCell>>,
    /// Compact summaries of descendants that finished under this child. These
    /// stay one line in the parent summary while their full cells remain in
    /// `detail` for the transcript view.
    pub(crate) nested: Vec<Arc<super::SubagentSummaryCell>>,
    /// How deep in the tree the child was, for indenting its summary.
    pub(crate) depth: usize,
}

/// A message from one actor to another, ready to be a card.
#[derive(Debug, Clone)]
pub(crate) struct PeerMessage {
    pub(crate) from: crate::history_cell::PeerParty,
    pub(crate) to: crate::history_cell::PeerParty,
    pub(crate) delivery: crate::history_cell::PeerHandle,
    pub(crate) text: String,
}

/// Bring a card's delivery state up to what `message` says. A provider's own session message
/// carries none and was, by being on the wire, delivered.
fn apply_delivery(handle: &crate::history_cell::PeerHandle, message: &SubagentMessage) {
    use crate::history_cell::PeerDelivery;
    match message.delivery {
        Some(nori_protocol::MessageDelivery::Queued) => handle.advance(PeerDelivery::Queued),
        Some(nori_protocol::MessageDelivery::Acknowledged) => {
            handle.advance(PeerDelivery::Acknowledged);
        }
        Some(nori_protocol::MessageDelivery::Delivered) | None => {
            handle.advance(PeerDelivery::Delivered);
        }
        Some(nori_protocol::MessageDelivery::Undeliverable) => handle.fail(message.failure.clone()),
    }
}

/// A child that finished, kept so the subagent switcher can still show what it did.
#[derive(Debug)]
pub(super) struct EndedRecord {
    pub(super) session_id: Option<SessionId>,
    pub(super) agent: Option<String>,
    pub(super) work_at: Instant,
    pub(super) history_parent: Option<NodeId>,
    pub(super) revision: i64,
    pub(super) finished: FinishedSubagent,
    /// What it was called, kept so a late announcement can rename the card.
    pub(super) identity: Identity,
    /// The label of the running subagent that started it, when it had one.
    pub(super) parent: Option<String>,
    pub(super) parent_id: Option<NodeId>,
    pub(super) ended_at: Instant,
}

/// Finished children kept for the switcher; the oldest are dropped past this.
const MAX_ENDED_KEPT: usize = 256;

/// What a session id or launcher call id refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Live(NodeId),
    /// The child completed but may be reused for a later turn.
    Ended(NodeId),
}

/// Which node an update for a child belongs to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ChildTarget<'a> {
    /// Delivered on the child's own session.
    Session(&'a SessionId),
    /// A root-session update tagged with the id of the tool call that launched the child.
    Launcher(&'a str),
}

/// All subagents of the session.
#[derive(Debug)]
pub(crate) struct SubagentTree {
    /// Running subagents by id, so iteration is in the order they appeared.
    pub(super) live: BTreeMap<NodeId, Node>,
    /// When the current burst of work began: what finished since is what the lanes' tally
    /// counts. A burst begins when a subagent starts or resumes while none is running.
    pub(super) burst_since: Instant,
    by_session: HashMap<SessionId, Link>,
    by_launcher: HashMap<String, Link>,
    root_session_id: Option<SessionId>,
    pub(super) context: CellContext,
    /// Merges launcher tool-call updates, which carry only the changed fields.
    launcher_normalizer: ClientEventNormalizer,
    finished: Vec<FinishedSubagent>,
    /// The revival flag of each committed root summary card, by child.
    summary_revivals: HashMap<NodeId, Arc<std::sync::atomic::AtomicBool>>,
    /// Counts every change to what the lanes show; what is cached for a frame is valid for one
    /// value of it.
    pub(super) revision: u64,
    pub(super) created_at: Instant,
    pub(super) below_memo: std::cell::RefCell<super::lanes::BelowMemo>,
    pub(super) lane_cache: std::cell::RefCell<Option<super::lanes::LaneCache>>,
    /// Peer messages that are whole, waiting to become cards.
    peers: Vec<(Instant, PeerMessage)>,
    /// The delivery state of each peer message already drawn, by message id, oldest first.
    peer_handles: Vec<(String, crate::history_cell::PeerHandle)>,
    /// Streamed peer messages still receiving chunks, by message id.
    peer_open: Vec<(String, PeerMessage)>,
    /// Every finished child, by node id, for the switcher.
    pub(super) ended: BTreeMap<NodeId, EndedRecord>,
    /// Finished nodes whose root summary waits for live descendants.
    pub(super) pending: BTreeMap<NodeId, FinishedSubagent>,
    pending_children: HashMap<NodeId, BTreeSet<NodeId>>,
    pub(super) pending_nested: HashMap<NodeId, BTreeMap<NodeId, Arc<super::SubagentSummaryCell>>>,
    /// Nodes that a later node took the place of: `(old, new)`.
    pub(super) replaced: Vec<(NodeId, NodeId)>,
    next_id: i64,
}

impl SubagentTree {
    pub(crate) fn new(cwd: std::path::PathBuf, animations: bool) -> Self {
        Self {
            live: BTreeMap::new(),
            burst_since: Instant::now(),
            by_session: HashMap::new(),
            by_launcher: HashMap::new(),
            root_session_id: None,
            context: CellContext { cwd, animations },
            launcher_normalizer: ClientEventNormalizer::default(),
            finished: Vec::new(),
            summary_revivals: HashMap::new(),
            revision: 0,
            created_at: Instant::now(),
            below_memo: std::cell::RefCell::default(),
            lane_cache: std::cell::RefCell::new(None),
            peers: Vec::new(),
            peer_handles: Vec::new(),
            peer_open: Vec::new(),
            ended: BTreeMap::new(),
            pending: BTreeMap::new(),
            pending_children: HashMap::new(),
            pending_nested: HashMap::new(),
            replaced: Vec::new(),
            next_id: 0,
        }
    }

    pub(crate) fn set_root(&mut self, session_id: SessionId) {
        self.revision += 1;
        self.root_session_id = Some(session_id);
    }

    pub(crate) fn is_root(&self, session_id: &SessionId) -> bool {
        self.root_session_id.as_ref() == Some(session_id)
    }

    pub(crate) fn animations(&self) -> bool {
        self.context.animations
    }

    pub(crate) fn running_count(&self) -> usize {
        self.live.values().filter(|node| !node.idle).count()
    }

    pub(crate) fn has_running(&self) -> bool {
        self.running_count() > 0
    }

    /// Children that finished since the last call, in completion order.
    pub(crate) fn take_finished(&mut self) -> Vec<FinishedSubagent> {
        std::mem::take(&mut self.finished)
    }

    /// A committed summary card's revival flag, so that the child coming back flips the card.
    pub(crate) fn register_summary(
        &mut self,
        node_id: NodeId,
        revival: Arc<std::sync::atomic::AtomicBool>,
    ) {
        if self.summary_revivals.len() >= 512 {
            self.summary_revivals.clear();
        }
        self.summary_revivals.insert(node_id, revival);
    }

    /// The label to show for a request that came from `session_id`, if it is a running child.
    pub(crate) fn label_for_session(&self, session_id: &SessionId) -> Option<String> {
        let Some(Link::Live(id)) = self.by_session.get(session_id) else {
            return None;
        };
        Some(self.live.get(id)?.label())
    }

    /// Whether `call_id` is a root tool call that stands for a subagent.
    pub(crate) fn is_launcher_call(&self, call_id: &str) -> bool {
        self.by_launcher.contains_key(call_id)
    }

    fn create(&mut self, now: Instant) -> NodeId {
        self.next_id += 1;
        let id = NodeId(self.next_id);
        if self.running_count() == 0 {
            self.burst_since = now;
        }
        self.live.insert(id, Node::new(now));
        id
    }

    /// The running node for a child session, created on first sight; `None` once it finished.
    fn node_for_session(&mut self, session_id: &SessionId, now: Instant) -> Option<NodeId> {
        match self.by_session.get(session_id) {
            Some(Link::Live(id)) => return Some(*id),
            Some(Link::Ended(_)) => return None,
            None => {}
        }
        let id = self.create(now);
        if let Some(node) = self.live.get_mut(&id) {
            node.session_id = Some(session_id.clone());
        }
        self.by_session.insert(session_id.clone(), Link::Live(id));
        Some(id)
    }

    fn reactivate_session(&mut self, session_id: &SessionId, now: Instant) -> Option<NodeId> {
        let Some(Link::Ended(id)) = self.by_session.get(session_id).copied() else {
            return None;
        };
        let record = self.ended.get(&id)?;
        // A child that failed on one route can go on, on another: it runs again.
        if !matches!(
            record.finished.outcome,
            SubagentOutcome::Completed { .. } | SubagentOutcome::Failed { .. }
        ) {
            return None;
        }
        if self.running_count() == 0 {
            self.burst_since = now;
        }
        // Its failure, if it is still held back, never happened as far as the transcript goes.
        self.finished.retain(|finished| finished.node_id != id);
        let mut node = Node::new(now);
        node.session_id = Some(session_id.clone());
        node.agent = record.agent.clone();
        node.model = record.finished.model.clone();
        node.name = record.identity.name.clone();
        node.title = record.identity.title.clone();
        node.description = record.identity.description.clone();
        node.history_parent = record.history_parent;
        node.archived = record.finished.detail.clone();
        node.nested_by_id = record
            .finished
            .nested
            .iter()
            .map(|summary| (summary.node_id(), Arc::clone(summary)))
            .collect();
        node.parent = record.parent_id;
        if let Some(children) = self.pending_children.remove(&id) {
            node.children.extend(children);
        }
        if let Some(nested) = self.pending_nested.remove(&id) {
            node.nested_by_id.extend(nested);
        }
        node.nested = node.nested_by_id.values().cloned().collect();
        // Current child summaries replace their archived versions. Their own
        // transcript retains previous turns, so showing both repeats content.
        node.archived.retain(|cell| {
            !cell
                .as_any()
                .downcast_ref::<super::SubagentSummaryCell>()
                .is_some_and(|summary| node.nested_by_id.contains_key(&summary.node_id()))
        });
        self.pending.remove(&id);
        let descendants: Vec<NodeId> = self
            .live
            .iter()
            .filter_map(|(child, node)| (node.parent == Some(id)).then_some(*child))
            .collect();
        node.children.extend(descendants);
        self.restore_ancestor_chain(id, node.parent);
        self.live.insert(id, node);
        self.by_session.insert(session_id.clone(), Link::Live(id));
        Some(id)
    }

    /// The running node for a launcher tool call, created on first sight; `None` once it finished.
    fn node_for_launcher(&mut self, call_id: &str, now: Instant) -> Option<NodeId> {
        match self.by_launcher.get(call_id) {
            Some(Link::Live(id)) => return Some(*id),
            Some(Link::Ended(_)) => return None,
            None => {}
        }
        let id = self.create(now);
        if let Some(node) = self.live.get_mut(&id) {
            node.launcher_call_id = Some(call_id.to_owned());
        }
        self.by_launcher.insert(call_id.to_owned(), Link::Live(id));
        Some(id)
    }

    /// Make `child` a child of `parent` unless it already has a parent or `parent` descends from
    /// it. An agent that contradicts itself cannot make a cycle, which would hide both nodes.
    fn adopt(&mut self, parent: NodeId, child: NodeId) {
        let mut ancestor = Some(parent);
        let mut seen = BTreeSet::new();
        while let Some(id) = ancestor {
            if id == child || !seen.insert(id) {
                return;
            }
            ancestor = self
                .live
                .get(&id)
                .and_then(|node| node.parent)
                .or_else(|| self.ended.get(&id).and_then(|record| record.parent_id));
        }
        let Some(node) = self.live.get_mut(&child) else {
            return;
        };
        if node.parent.is_some() {
            return;
        }
        node.parent = Some(parent);
        node.history_parent = Some(parent);
        if let Some(parent) = self.live.get_mut(&parent) {
            parent.children.insert(child);
        }
    }

    /// An announcement, or a change to a child already announced.
    pub(crate) fn on_update(&mut self, update: SubagentUpdate, now: Instant) {
        self.revision += 1;
        let was_running = match self.by_session.get(&update.session_id) {
            Some(Link::Live(id)) => self.live.get(id).is_some_and(|node| !node.idle),
            Some(Link::Ended(_)) | None => false,
        };
        let id = if matches!(update.state, Patch::Set(SubagentState::Running)) {
            self.reactivate_session(&update.session_id, now)
                .or_else(|| self.node_for_session(&update.session_id, now))
        } else {
            self.node_for_session(&update.session_id, now)
        };
        let Some(id) = id else {
            if let Some(record) = self
                .ended
                .values_mut()
                .find(|record| record.session_id.as_ref() == Some(&update.session_id))
            {
                update.agent.clone().apply_to(&mut record.agent);
                update.model.clone().apply_to(&mut record.finished.model);
                record.identity.apply(&update);
                record.finished.kind = record.identity.kind();
                record.finished.label = record.identity.label();
                record.revision = record.revision.saturating_add(1);
            }
            return;
        };
        if let Some(Link::Live(parent)) = self.by_session.get(&update.parent_session_id).copied() {
            self.adopt(parent, id);
        } else if let Some(parent) = self.ended.iter().find_map(|(parent, record)| {
            (record.session_id.as_ref() == Some(&update.parent_session_id)).then_some(*parent)
        }) && let Some(node) = self.live.get_mut(&id)
        {
            node.history_parent = Some(parent);
        }
        if let Some(node) = self.live.get_mut(&id) {
            update.name.apply_to(&mut node.name);
            update.title.apply_to(&mut node.title);
            update.description.apply_to(&mut node.description);
            update.agent.apply_to(&mut node.agent);
            update.model.apply_to(&mut node.model);
            node.touch();
        }
        match update.state {
            nori_protocol::Patch::Set(SubagentState::Ended(outcome)) => {
                self.finish(id, outcome, None, now);
            }
            nori_protocol::Patch::Set(SubagentState::Idle { interrupted: false })
                if was_running =>
            {
                // The turn ended by itself: the child finished its work, as one that reports
                // `completed` does. It stays reachable; its next turn reactivates it.
                let outcome = SubagentOutcome::Completed {
                    stop_reason: Some("end_turn".to_owned()),
                };
                self.finish(id, outcome, None, now);
            }
            nori_protocol::Patch::Set(
                state @ (SubagentState::Running | SubagentState::Idle { .. }),
            ) => {
                if let Some(node) = self.live.get_mut(&id) {
                    let idle = matches!(state, SubagentState::Idle { .. });
                    if idle != node.idle {
                        node.idle = idle;
                        node.touch();
                    }
                }
            }
            nori_protocol::Patch::Keep | nori_protocol::Patch::Clear => {}
        }
    }

    /// A message between sessions. The prompt a parent sent to a child names the parent's launcher
    /// tool call, which links the call to the child: the child's node takes over the launcher's.
    /// Any other message is one actor talking to another, and becomes a card.
    pub(crate) fn on_message(&mut self, message: SubagentMessage, now: Instant) {
        self.revision += 1;
        if self.link_launcher(&message, now) {
            return;
        }
        self.record_peer_message(message);
    }

    /// Link the child a launcher's prompt went to. Whether `message` was such a prompt.
    fn link_launcher(&mut self, message: &SubagentMessage, now: Instant) -> bool {
        let (Some(call_id), Some(recipient)) = (
            message.message_id.as_deref(),
            message.recipient_session_id.as_ref(),
        ) else {
            return false;
        };
        let Some(Link::Live(launcher)) = self.by_launcher.get(call_id).copied() else {
            return false;
        };
        let Some(child) = self.node_for_session(recipient, now) else {
            return false;
        };
        if launcher == child {
            return true;
        }
        let Some(stand_in) = self.live.remove(&launcher) else {
            return true;
        };
        self.replaced.push((launcher, child));
        self.by_launcher
            .insert(call_id.to_owned(), Link::Live(child));
        if let Some(parent) = stand_in
            .parent
            .and_then(|parent| self.live.get_mut(&parent))
        {
            parent.children.remove(&launcher);
        }
        for grandchild in &stand_in.children {
            if let Some(node) = self.live.get_mut(grandchild) {
                node.parent = None;
            }
        }
        for grandchild in stand_in.children.iter().copied() {
            self.adopt(child, grandchild);
        }
        if let Some(parent) = stand_in.parent {
            self.adopt(parent, child);
        }
        if let Some(node) = self.live.get_mut(&child) {
            node.launcher_call_id = Some(call_id.to_owned());
            node.started_at = stand_in.started_at.min(node.started_at);
            node.work_at = node.work_at.max(stand_in.work_at);
            node.tool_count += stand_in.tool_count;
            if node.description.is_none() {
                node.description = stand_in.description;
            }
            if node.name.is_none() {
                node.name = stand_in.name;
            }
            if node.title.is_none() {
                node.title = stand_in.title;
            }
            node.touch();
        }
        true
    }

    /// What an actor is called in the transcript: `main` for the root session, the subagent's
    /// label while it runs or after it finished, else the start of its session id.
    pub(crate) fn actor_label(&self, session_id: &SessionId) -> String {
        if self.is_root(session_id) {
            return "main".to_owned();
        }
        match self.by_session.get(session_id) {
            Some(Link::Live(id)) => self.live.get(id).map(Node::label),
            Some(Link::Ended(id)) => self
                .ended
                .get(id)
                .map(|record| record.finished.label.clone()),
            None => None,
        }
        .unwrap_or_else(|| session_id.0.chars().take(8).collect())
    }

    /// One end of a peer message: its name (the sender's own, else what the tree calls it) and
    /// what runs it.
    fn party(
        &self,
        session_id: &SessionId,
        label: Option<String>,
    ) -> crate::history_cell::PeerParty {
        let (agent, model) = match self.by_session.get(session_id) {
            Some(Link::Live(id)) => self
                .live
                .get(id)
                .map(|node| (node.agent.clone(), node.model.clone()))
                .unwrap_or_default(),
            Some(Link::Ended(id)) => self
                .ended
                .get(id)
                .map(|record| (record.agent.clone(), record.finished.model.clone()))
                .unwrap_or_default(),
            None => (None, None),
        };
        crate::history_cell::PeerParty {
            name: label.unwrap_or_else(|| self.actor_label(session_id)),
            agent,
            model,
        }
    }

    /// A message between two actors, drawn as a card once it is whole: a streamed one waits for
    /// its last chunk (the next whole message, or the end of the turn).
    fn record_peer_message(&mut self, message: SubagentMessage) {
        let (Some(sender), Some(recipient)) = (
            message.sender_session_id.as_ref(),
            message.recipient_session_id.as_ref(),
        ) else {
            return;
        };
        // The harness announces the same message again as its delivery advances; the card was
        // drawn when it was first seen, and the later states are not drawn again.
        let handle = match message.message_id.as_ref().filter(|_| !message.chunk) {
            Some(id) => {
                if let Some((_, known)) = self.peer_handles.iter().find(|(seen, _)| seen == id) {
                    apply_delivery(known, &message);
                    return;
                }
                let handle =
                    crate::history_cell::PeerHandle::new(crate::history_cell::PeerDelivery::Queued);
                apply_delivery(&handle, &message);
                if self.peer_handles.len() >= 512 {
                    self.peer_handles.remove(0);
                }
                self.peer_handles.push((id.clone(), handle.clone()));
                handle
            }
            None => {
                let handle =
                    crate::history_cell::PeerHandle::new(crate::history_cell::PeerDelivery::Queued);
                apply_delivery(&handle, &message);
                handle
            }
        };
        let peer = PeerMessage {
            from: self.party(sender, message.sender_label.clone()),
            to: self.party(recipient, message.recipient_label.clone()),
            delivery: handle,
            text: message.text.clone(),
        };
        let key = message
            .message_id
            .clone()
            .unwrap_or_else(|| format!("{}>{}", peer.from.name, peer.to.name));
        if message.chunk {
            match self.peer_open.iter_mut().find(|(open, _)| *open == key) {
                Some((_, open)) => open.text.push_str(&peer.text),
                None => self.peer_open.push((key, peer)),
            }
            return;
        }
        // A whole message closes the one it completes, and any other left open.
        self.close_peer_messages();
        self.peers.push((Instant::now(), peer));
    }

    /// The streamed messages still open are complete: the turn is over, or something else was
    /// said.
    pub(crate) fn close_peer_messages(&mut self) {
        let open = std::mem::take(&mut self.peer_open);
        let now = Instant::now();
        self.peers
            .extend(open.into_iter().map(|(_, peer)| (now, peer)));
    }

    /// Peer messages that became whole since the last call, in the order they were said.
    /// Peer messages that became whole since the last call, in the order they were said. Their
    /// cards read the delivery state each frame they are on screen, so there is nothing to wait
    /// for.
    pub(crate) fn take_peer_messages(&mut self) -> Vec<PeerMessage> {
        std::mem::take(&mut self.peers)
            .into_iter()
            .map(|(_, peer)| peer)
            .collect()
    }

    /// A root-session tool call that launches a subagent, or an update to one.
    pub(crate) fn on_launcher_update(
        &mut self,
        update: &acp::SessionUpdate,
        async_launch: bool,
        now: Instant,
    ) {
        self.revision += 1;
        let Some(snapshot) = self
            .launcher_normalizer
            .push_session_update(update)
            .into_iter()
            .find_map(|event| match event {
                ClientEvent::ToolSnapshot(snapshot) => Some(snapshot),
                _ => None,
            })
        else {
            return;
        };
        let Some(id) = self.node_for_launcher(&snapshot.call_id, now) else {
            return;
        };
        let Some(node) = self.live.get_mut(&id) else {
            return;
        };
        let announced = node.session_id.is_some();
        // The launcher's title starts as the tool's name and becomes the task once the adapter
        // fills it in; an announced child carries its own description.
        if !announced {
            // What the Task/Agent tool was asked to launch: its own name when it has one, its
            // type and its task. Claude's call input carries them even while its title is still
            // the tool's name.
            let input = |key: &str| {
                snapshot
                    .raw_input
                    .as_ref()
                    .and_then(|input| input.get(key))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
            };
            if let Some(name) = input("name") {
                node.name = Some(name);
            }
            if let Some(kind) = input("subagent_type") {
                node.title = Some(kind);
            }
            if let Some(task) = input("description") {
                node.description = Some(task);
            } else if !snapshot.title.is_empty()
                && !matches!(snapshot.title.as_str(), "Task" | "Agent")
            {
                node.description = Some(snapshot.title.clone());
            }
        }
        node.async_launch |= async_launch;
        node.touch();
        // An announced child reports its own state, and an asynchronous launch finishes within
        // milliseconds whatever the child is doing.
        if announced || node.async_launch {
            return;
        }
        match snapshot.phase {
            ToolPhase::Completed => {
                let message = snapshot_text(&snapshot);
                self.finish(
                    id,
                    SubagentOutcome::Completed { stop_reason: None },
                    Some(message),
                    now,
                );
            }
            ToolPhase::Failed => {
                self.finish(id, SubagentOutcome::Failed { reason: None }, None, now);
            }
            ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress => {}
        }
    }

    /// An update delivered on a child's own session, or tagged with its launcher's id.
    pub(crate) fn on_child_update(
        &mut self,
        target: ChildTarget<'_>,
        update: &acp::SessionUpdate,
        now: Instant,
    ) {
        self.revision += 1;
        let id = match target {
            ChildTarget::Session(session_id) => self.node_for_session(session_id, now),
            ChildTarget::Launcher(call_id) => self.node_for_launcher(call_id, now),
        };
        if let Some(node) = id.and_then(|id| self.live.get_mut(&id)) {
            if matches!(
                update,
                acp::SessionUpdate::UserMessageChunk(_)
                    | acp::SessionUpdate::AgentMessageChunk(_)
                    | acp::SessionUpdate::ToolCall(_)
                    | acp::SessionUpdate::ToolCallUpdate(_)
            ) {
                node.work_at = now;
            }
            node.apply(update, &self.context.cwd, self.context.animations);
        }
    }

    /// End a running node: it leaves the tree and becomes a [`FinishedSubagent`].
    ///
    /// `message` replaces the child's own final message, for a child known only through a launcher
    /// whose result is the message.
    fn finish(
        &mut self,
        id: NodeId,
        outcome: SubagentOutcome,
        message: Option<String>,
        now: Instant,
    ) {
        let depth = self.depth_of(id);
        let Some(mut node) = self.live.remove(&id) else {
            return;
        };
        let session_id = node.session_id.take();
        if let Some(session_id) = &session_id {
            self.by_session.insert(session_id.clone(), Link::Ended(id));
        }
        if let Some(call_id) = node.launcher_call_id.take() {
            self.by_launcher.insert(call_id, Link::Ended(id));
        }
        let parent_id = node.parent;
        let waiting_for = node.children.clone();
        let nested = node.nested.clone();
        let (mut detail, final_message) = node.take_detail(&self.context.cwd);
        if let Some(previous) = self.ended.get(&id) {
            let mut prior = previous.finished.detail.clone();
            prior.retain(|cell| {
                !cell
                    .as_any()
                    .downcast_ref::<super::SubagentSummaryCell>()
                    .is_some_and(|summary| node.nested_by_id.contains_key(&summary.node_id()))
            });
            prior.extend(detail);
            detail = prior;
        }
        let finished = FinishedSubagent {
            node_id: id,
            label: node.label(),
            kind: node.kind(),
            agent: node.agent.clone(),
            model: node.model.clone(),
            outcome,
            tool_count: node.tool_count,
            elapsed: now.saturating_duration_since(node.started_at),
            message: message.unwrap_or(final_message),
            detail,
            nested: nested.clone(),
            depth,
        };
        let parent_label = node
            .parent
            .and_then(|parent| self.live.get(&parent))
            .map(Node::label);
        self.ended.insert(
            id,
            EndedRecord {
                session_id,
                agent: node.agent.clone(),
                work_at: node.work_at,
                history_parent: node.history_parent,
                revision: i64::MIN,
                finished: finished.clone(),
                identity: node.identity(),
                parent: parent_label,
                parent_id,
                ended_at: now,
            },
        );
        if !waiting_for.is_empty() {
            self.pending.insert(id, finished.clone());
            self.pending_children.insert(id, waiting_for.clone());
            self.pending_nested.insert(
                id,
                nested
                    .iter()
                    .map(|summary| (summary.node_id(), summary.clone()))
                    .collect(),
            );
        }
        while self.ended.len() > MAX_ENDED_KEPT {
            let Some(evict) = self.ended.keys().copied().find(|candidate| {
                if self.pending.contains_key(candidate) {
                    return false;
                }
                let mut ancestor = self
                    .ended
                    .get(candidate)
                    .and_then(|record| record.parent_id);
                while let Some(parent) = ancestor {
                    if self.pending.contains_key(&parent) {
                        return false;
                    }
                    ancestor = self.ended.get(&parent).and_then(|record| record.parent_id);
                }
                true
            }) else {
                break;
            };
            self.ended.remove(&evict);
        }
        if waiting_for.is_empty() {
            self.complete_finished(id, finished);
        }
    }

    fn complete_finished(&mut self, id: NodeId, finished: FinishedSubagent) {
        let parent_id = self.ended.get(&id).and_then(|record| record.parent_id);
        let Some(parent_id) = parent_id else {
            self.finished.push(finished);
            return;
        };
        let summary = Arc::new(super::SubagentSummaryCell::new(finished.clone()));
        if let Some(parent) = self.live.get_mut(&parent_id) {
            parent.children.remove(&id);
            parent.nested_by_id.insert(id, summary);
            parent.nested = parent.nested_by_id.values().cloned().collect();
            parent.touch();
            return;
        }
        if self.pending.contains_key(&parent_id) {
            self.pending_nested
                .entry(parent_id)
                .or_default()
                .insert(id, summary);
            if let Some(record) = self.ended.get_mut(&parent_id)
                && let Some(ordered) = self.pending_nested.get(&parent_id)
            {
                record.finished.nested = ordered.values().cloned().collect();
            }
            if let Some(children) = self.pending_children.get_mut(&parent_id) {
                children.remove(&id);
                if children.is_empty() {
                    self.pending_children.remove(&parent_id);
                    let ordered = self.pending_nested.remove(&parent_id).unwrap_or_default();
                    if let Some(parent) = self.pending.remove(&parent_id) {
                        let mut parent = parent;
                        parent.nested = ordered.values().cloned().collect();
                        parent.detail.retain(|cell| {
                            !cell
                                .as_any()
                                .downcast_ref::<super::SubagentSummaryCell>()
                                .is_some_and(|summary| ordered.contains_key(&summary.node_id()))
                        });
                        parent.detail.extend(
                            ordered
                                .values()
                                .cloned()
                                .map(|cell| cell as Arc<dyn HistoryCell>),
                        );
                        if let Some(record) = self.ended.get_mut(&parent_id) {
                            record.finished = parent.clone();
                        }
                        self.complete_finished(parent_id, parent);
                    }
                }
            }
            return;
        }
        // The parent was already emitted (for example after a cancelled
        // subtree); preserve visibility rather than dropping the completion.
        self.finished.push(finished);
    }

    pub(super) fn depth_of(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut seen = BTreeSet::from([id]);
        let mut current = self.live.get(&id).and_then(|node| node.parent);
        while let Some(parent) = current {
            if !seen.insert(parent) {
                break;
            }
            depth += 1;
            current = self
                .live
                .get(&parent)
                .and_then(|node| node.parent)
                .or_else(|| self.ended.get(&parent).and_then(|record| record.parent_id));
        }
        depth
    }

    /// Live nodes in display order, each with its depth: a child follows its parent, children in
    /// the order they appeared. Stops after `limit` nodes.
    #[cfg(test)]
    pub(super) fn display_order(&self, limit: usize) -> Vec<(NodeId, &Node, usize)> {
        let mut out = Vec::new();
        let mut stack: Vec<(NodeId, usize)> = Vec::new();
        for (id, node) in &self.live {
            if node
                .parent
                .is_some_and(|parent| self.live.contains_key(&parent))
            {
                continue;
            }
            stack.push((*id, self.depth_of(*id)));
            while let Some((id, depth)) = stack.pop() {
                if out.len() == limit {
                    return out;
                }
                let Some(node) = self.live.get(&id) else {
                    continue;
                };
                out.push((id, node, depth));
                stack.extend(node.children.iter().rev().map(|child| (*child, depth + 1)));
            }
        }
        out
    }

    /// The root turn ended. Children that were only ever a launcher call (no announcement) were
    /// kept running because the launcher returned at once; they end with the turn. When the turn
    /// was cancelled every running child is shown cancelled.
    pub(crate) fn on_root_turn_end(&mut self, cancelled: bool, now: Instant) {
        self.revision += 1;
        let ending: Vec<NodeId> = self
            .live
            .iter()
            .filter(|(_, node)| !node.idle && (cancelled || node.session_id.is_none()))
            .map(|(id, _)| *id)
            .collect();
        for id in ending {
            let outcome = if cancelled {
                SubagentOutcome::Cancelled
            } else {
                SubagentOutcome::Completed { stop_reason: None }
            };
            self.finish(id, outcome, None, now);
        }
    }
}

/// The text a completed tool call returned, for a child known only through its launcher.
fn snapshot_text(snapshot: &ToolSnapshot) -> String {
    snapshot
        .artifacts
        .iter()
        .find_map(|artifact| match artifact {
            Artifact::Text { text, .. } => Some(text.clone()),
            Artifact::Diff(_) => None,
        })
        .unwrap_or_default()
}
