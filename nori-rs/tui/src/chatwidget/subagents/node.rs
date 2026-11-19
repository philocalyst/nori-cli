//! One running subagent: what the live block shows and the detail its summary will carry.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use nori_protocol::acp::v1 as acp;
use nori_protocol::acp::v1::SessionId;

use ratatui::text::Line;

use super::SubagentSummaryCell;
use crate::client_tool_cell::ClientToolCell;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::presentation::ClientEvent;
use crate::presentation::ClientEventNormalizer;
use crate::subagent_switcher::Tail;
use crate::subagent_switcher::cell_lines;

/// How much of a child's final message is kept for its summary.
const MAX_FINAL_MESSAGE_BYTES: usize = 8 * 1024;
/// How much of a conversation's words the picker can search.
const MAX_SEARCH_BYTES: usize = 8 * 1024;
/// The activity row shows at most this many characters of the latest line.
const MAX_ACTIVITY_CHARS: usize = 240;
/// The latest line is looked for in at most this many trailing bytes of streamed text, so a
/// delta costs the same however long the line it continues is.
const ACTIVITY_SCAN_BYTES: usize = MAX_ACTIVITY_CHARS * 4;

/// Identity of a running subagent within its tree. Ids grow and are never reused, so ordering by
/// id is the order in which subagents appeared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(pub(crate) i64);

/// A part of a child's detail, in the order it happened.
#[derive(Debug)]
enum Detail {
    Tool {
        cell: Box<ClientToolCell>,
        revision: i64,
    },
    User(String),
    Text(String),
}

/// What a child did, kept for the transcript overlay once it finishes.
#[derive(Debug, Default)]
struct ChildTranscript {
    parts: Vec<Detail>,
    /// The last part is text that more deltas continue; a tool call ends it.
    text_open: bool,
    /// Position in `parts` of each tool call.
    tools: HashMap<String, usize>,
    normalizer: ClientEventNormalizer,
}

/// A subagent that is still running. A finished one is turned into a
/// [`super::FinishedSubagent`] and leaves the tree, so nothing here describes a finished child.
#[derive(Debug)]
pub(super) struct Node {
    /// Child session id, once known. A child known only through its launcher call has none.
    pub(super) session_id: Option<SessionId>,
    pub(super) agent: Option<String>,
    pub(super) model: Option<String>,
    /// The root tool call that launched this child, once known.
    pub(super) launcher_call_id: Option<String>,
    /// The running subagent that announced this one.
    pub(super) parent: Option<NodeId>,
    /// Original hierarchy, retained after a parent finishes.
    pub(super) history_parent: Option<NodeId>,
    /// Running subagents this one announced.
    pub(super) children: BTreeSet<NodeId>,
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    /// Known only through a launcher that reported completion at once while its work continues.
    pub(super) async_launch: bool,
    /// The session is available for follow-up work but has no running turn.
    pub(super) idle: bool,
    pub(super) started_at: Instant,
    pub(super) tool_count: usize,
    /// The latest tool title or last line of streamed text.
    pub(super) activity: String,
    /// Bumped on every change that affects what the live block shows.
    pub(super) revision: i64,
    /// When the child last changed, for ordering the subagent switcher.
    pub(super) active_at: Instant,
    /// The most recent message or tool work, excluding status-only updates.
    pub(super) work_at: Instant,
    /// Summaries of this child's own finished children, for the detail view.
    pub(super) nested: Vec<Arc<SubagentSummaryCell>>,
    pub(super) nested_by_id: BTreeMap<NodeId, Arc<SubagentSummaryCell>>,
    /// Completed-turn cells retained while a reusable provider session runs again.
    pub(super) archived: Vec<Arc<dyn HistoryCell>>,
    transcript: ChildTranscript,
    projection_epoch: i64,
    /// The tail of the conversation as plain words, for search.
    search: String,
}

impl Node {
    pub(super) fn projection_len(&self) -> usize {
        self.archived.len() + self.transcript.parts.len() + self.nested.len()
    }

    pub(super) fn projection_continuation(&self, index: usize) -> Option<bool> {
        if index < self.archived.len() {
            self.archived
                .get(index)
                .map(|cell| cell.is_stream_continuation())
        } else if index - self.archived.len() < self.transcript.parts.len() {
            let _index = index - self.archived.len();
            Some(false)
        } else {
            self.nested
                .get(
                    index
                        .checked_sub(self.archived.len())?
                        .checked_sub(self.transcript.parts.len())?,
                )
                .map(|cell| cell.is_stream_continuation())
        }
    }

    pub(super) fn projection_revision(&self, index: usize) -> Option<i64> {
        if let Some(cell) = self.archived.get(index) {
            return Some(
                self.projection_epoch
                    .wrapping_mul(1_000_003)
                    .wrapping_add(cell.display_lines(1).len() as i64),
            );
        }
        let index = index - self.archived.len();
        let revision = match self.transcript.parts.get(index) {
            Some(Detail::Tool { revision, .. }) => revision.wrapping_mul(3),
            Some(Detail::User(text)) => i64::try_from(text.len())
                .unwrap_or(i64::MAX)
                .wrapping_mul(3)
                .wrapping_add(1),
            Some(Detail::Text(text)) => i64::try_from(text.len())
                .unwrap_or(i64::MAX)
                .wrapping_mul(3)
                .wrapping_add(2),
            None if index - self.transcript.parts.len() < self.nested.len() => {
                self.revision.wrapping_mul(3).wrapping_add(1)
            }
            None => return None,
        };
        Some(
            self.projection_epoch
                .wrapping_mul(1_000_003)
                .wrapping_add(revision),
        )
    }

    pub(super) fn project_cell(
        &self,
        index: usize,
        cwd: &Path,
        width: u16,
        expanded: &std::collections::HashSet<String>,
    ) -> Option<crate::chatwidget::tool_projection::ToolProjection> {
        let mut projection = crate::chatwidget::tool_projection::ToolProjection::default();
        if let Some(cell) = self.archived.get(index) {
            projection.push(cell.as_ref(), width, expanded);
            return Some(projection);
        }
        let index = index - self.archived.len();
        match self.transcript.parts.get(index) {
            Some(Detail::Tool { cell, .. }) => projection.push(cell.as_ref(), width, expanded),
            Some(Detail::User(message)) => projection.push(
                &UserHistoryCell {
                    message: message.clone(),
                },
                width,
                expanded,
            ),
            Some(Detail::Text(text)) => {
                projection.push(&AgentMarkdownCell::new(text.clone(), cwd), width, expanded)
            }
            None => projection.push(
                self.nested
                    .get(index.checked_sub(self.transcript.parts.len())?)?
                    .as_ref(),
                width,
                expanded,
            ),
        }
        Some(projection)
    }

    /// The words of the conversation, for the picker's full-text search: the tail of it, kept as
    /// it happens so reading it costs a copy and not a walk through the transcript.
    pub(super) fn searchable_text(&self) -> String {
        self.search.clone()
    }

    /// Add `text` to what the picker can search, dropping the oldest words past the cap.
    fn remember(&mut self, text: &str) {
        if !self.search.is_empty() {
            self.search.push('\n');
        }
        self.search.push_str(text);
        if self.search.len() > MAX_SEARCH_BYTES {
            let mut cut = self.search.len() - MAX_SEARCH_BYTES;
            while !self.search.is_char_boundary(cut) {
                cut += 1;
            }
            self.search.drain(..cut);
        }
    }

    pub(super) fn new(now: Instant) -> Self {
        Self {
            session_id: None,
            agent: None,
            model: None,
            launcher_call_id: None,
            parent: None,
            history_parent: None,
            children: BTreeSet::new(),
            title: None,
            description: None,
            async_launch: false,
            idle: false,
            started_at: now,
            tool_count: 0,
            activity: String::new(),
            revision: 0,
            active_at: now,
            work_at: now,
            nested: Vec::new(),
            nested_by_id: BTreeMap::new(),
            archived: Vec::new(),
            transcript: ChildTranscript::default(),
            projection_epoch: 0,
            search: String::new(),
        }
    }

    /// The main label: what the child was asked to do, else its type.
    pub(super) fn label(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("subagent")
    }

    /// The type of the child, shown next to the task when it says something the task does not:
    /// a title that only repeats the description is not a type.
    pub(super) fn kind(&self) -> Option<&str> {
        match (&self.description, &self.title) {
            (Some(description), Some(title)) if description != title => Some(title),
            _ => None,
        }
    }

    pub(super) fn touch(&mut self) {
        self.revision += 1;
        self.active_at = Instant::now();
    }

    /// End a replayed response and return the detail cell it belongs to.
    pub(super) fn replay_response_boundary(&mut self) -> Option<usize> {
        self.transcript.text_open = false;
        self.transcript
            .parts
            .iter()
            .rposition(|part| matches!(part, Detail::Text(_)))
    }

    pub(super) fn replace_replay(
        &mut self,
        events: &[nori_protocol::SessionEvent],
        cwd: &Path,
        animations: bool,
    ) {
        self.projection_epoch = self.projection_epoch.wrapping_add(1);
        self.transcript = ChildTranscript::default();
        self.search.clear();
        self.tool_count = 0;
        for event in events {
            match event {
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    acp::AgentNotification::SessionNotification(notification),
                )) => self.apply(&notification.update, cwd, animations),
                nori_protocol::SessionEvent::Nori(
                    nori_protocol::NoriEvent::ConversationResponseBoundary(_),
                ) => {
                    self.replay_response_boundary();
                }
                _ => {}
            }
        }
        self.touch();
    }

    /// Absorb an update delivered on the child's session or tagged with its launcher.
    ///
    /// Costs the size of the update: text is appended, a tool call updates its own cell.
    pub(super) fn apply(&mut self, update: &acp::SessionUpdate, cwd: &Path, animations: bool) {
        match update {
            acp::SessionUpdate::UserMessageChunk(chunk) => {
                if let acp::ContentBlock::Text(text) = &chunk.content {
                    self.transcript.text_open = false;
                    self.transcript.parts.push(Detail::User(text.text.clone()));
                    self.remember(&text.text);
                    self.transcript.text_open = false;
                }
            }
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                if let acp::ContentBlock::Text(text) = &chunk.content {
                    self.push_text(&text.text);
                }
            }
            acp::SessionUpdate::ToolCall(_) | acp::SessionUpdate::ToolCallUpdate(_) => {
                self.transcript.text_open = false;
                for event in self.transcript.normalizer.push_session_update(update) {
                    let ClientEvent::ToolSnapshot(snapshot) = event else {
                        continue;
                    };
                    if !snapshot.title.is_empty() {
                        self.remember(&snapshot.title);
                        self.activity.clear();
                        self.activity
                            .extend(snapshot.title.chars().take(MAX_ACTIVITY_CHARS));
                    }
                    match self.transcript.tools.get(&snapshot.call_id) {
                        Some(position) => {
                            if let Some(Detail::Tool { cell, revision }) =
                                self.transcript.parts.get_mut(*position)
                            {
                                cell.apply_snapshot(snapshot);
                                *revision = revision.wrapping_add(1);
                            }
                        }
                        None => {
                            self.tool_count += 1;
                            self.transcript
                                .tools
                                .insert(snapshot.call_id.clone(), self.transcript.parts.len());
                            self.transcript.parts.push(Detail::Tool {
                                cell: Box::new(ClientToolCell::new(
                                    snapshot,
                                    cwd.to_path_buf(),
                                    animations,
                                )),
                                revision: 1,
                            });
                        }
                    }
                    self.touch();
                }
            }
            // A child's thoughts, plan, usage, mode and metadata updates are never the root's
            // and are not shown live. (The schema's variant set depends on its features.)
            _ => {}
        }
    }

    fn push_text(&mut self, delta: &str) {
        // Streamed words extend the line they continue.
        self.search.push_str(delta);
        if self.search.len() > MAX_SEARCH_BYTES * 2 {
            let mut cut = self.search.len() - MAX_SEARCH_BYTES;
            while !self.search.is_char_boundary(cut) {
                cut += 1;
            }
            self.search.drain(..cut);
        }
        match self.transcript.parts.last_mut() {
            Some(Detail::Text(text)) if self.transcript.text_open => text.push_str(delta),
            _ => self.transcript.parts.push(Detail::Text(delta.to_owned())),
        }
        self.transcript.text_open = true;
        let Some(Detail::Text(text)) = self.transcript.parts.last() else {
            return;
        };
        // The last line may be split across deltas; the text so far has it whole.
        let mut tail_start = text.len().saturating_sub(ACTIVITY_SCAN_BYTES);
        while !text.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let Some(line) = text[tail_start..]
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
        else {
            return;
        };
        self.activity.clear();
        self.activity
            .extend(line.trim().chars().take(MAX_ACTIVITY_CHARS));
        self.touch();
    }

    /// The last `rows` rows of what the child did so far, laid out at `width`, as the transcript
    /// overlay would show them. Costs the rows asked for, not the length of the transcript.
    pub(super) fn detail_lines(&self, cwd: &Path, width: u16, rows: usize) -> Vec<Line<'static>> {
        let mut tail = Tail::new(rows);
        for cell in self.archived.iter().rev() {
            if tail.is_full() {
                break;
            }
            tail.push_front(cell_lines(cell.as_ref(), width));
        }
        for cell in self.nested.iter().rev() {
            if tail.is_full() {
                break;
            }
            tail.push_front(cell_lines(cell.as_ref(), width));
        }
        for part in self.transcript.parts.iter().rev() {
            if tail.is_full() {
                break;
            }
            tail.push_front(match part {
                Detail::Tool { cell, .. } => cell_lines(cell.as_ref(), width),
                Detail::User(text) => UserHistoryCell {
                    message: text.clone(),
                }
                .display_lines(width),
                Detail::Text(text) => cell_lines(&AgentMarkdownCell::new(text.clone(), cwd), width),
            });
        }
        tail.finish()
    }

    /// Take the child's detail as cells for the transcript overlay, and the final message: the
    /// text it streamed after its last tool call.
    pub(super) fn take_detail(&mut self, cwd: &Path) -> (Vec<Arc<dyn HistoryCell>>, String) {
        let parts = std::mem::take(&mut self.transcript.parts);
        let final_message = match parts.last() {
            Some(Detail::Text(text)) if self.transcript.text_open => {
                let mut end = text.len().min(MAX_FINAL_MESSAGE_BYTES);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text[..end].to_owned()
            }
            Some(Detail::Text(_) | Detail::Tool { .. } | Detail::User(_)) | None => String::new(),
        };
        let mut detail: Vec<Arc<dyn HistoryCell>> = parts
            .into_iter()
            .map(|part| -> Arc<dyn HistoryCell> {
                match part {
                    Detail::Tool { cell, .. } => Arc::new(*cell),
                    Detail::User(message) => Arc::new(UserHistoryCell { message }),
                    Detail::Text(text) => Arc::new(AgentMarkdownCell::new(text, cwd)),
                }
            })
            .collect();
        detail.extend(
            self.nested
                .drain(..)
                .map(|cell| cell as Arc<dyn HistoryCell>),
        );
        (detail, final_message)
    }
}

/// The working directory and animation setting every child's cells are built with.
#[derive(Debug, Clone)]
pub(super) struct CellContext {
    pub(super) cwd: PathBuf,
    pub(super) animations: bool,
}
