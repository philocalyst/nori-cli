//! The subagent tree as the switcher sees it: one entry per subagent, running or finished, and the
//! tail of any one's conversation.

use std::time::Instant;

use nori_protocol::SubagentOutcome;
use ratatui::text::Line;

use super::SPINNER_PERIOD;
use super::SubagentTree;
use super::node::NodeId;
use crate::chatwidget::ChatWidget;
use crate::history_cell::AgentMarkdownCell;
use crate::subagent_switcher::Body;
use crate::subagent_switcher::BodyQuery;
use crate::subagent_switcher::Entry;
use crate::subagent_switcher::Parent;
use crate::subagent_switcher::Roster;
use crate::subagent_switcher::Status;
use crate::subagent_switcher::SubagentKey;
use crate::subagent_switcher::Tail;
use crate::subagent_switcher::Target;
use crate::subagent_switcher::cell_lines;

impl SubagentTree {
    pub(crate) fn projection_len(&self, key: SubagentKey) -> Option<usize> {
        let id = NodeId(key.0);
        self.live
            .get(&id)
            .map(super::node::Node::projection_len)
            .or_else(|| {
                self.ended.get(&id).map(|record| {
                    record
                        .finished
                        .detail
                        .len()
                        .max(usize::from(!record.finished.message.trim().is_empty()))
                })
            })
    }

    pub(crate) fn projection_revision(&self, key: SubagentKey, index: usize) -> Option<i64> {
        let id = NodeId(key.0);
        if let Some(node) = self.live.get(&id) {
            return node.projection_revision(index);
        }
        let record = self.ended.get(&id)?;
        let len = record
            .finished
            .detail
            .len()
            .max(usize::from(!record.finished.message.trim().is_empty()));
        (index < len).then_some(record.revision)
    }

    pub(crate) fn projection_continuation(&self, key: SubagentKey, index: usize) -> Option<bool> {
        let id = NodeId(key.0);
        if let Some(node) = self.live.get(&id) {
            return node.projection_continuation(index);
        }
        let finished = &self.ended.get(&id)?.finished;
        if let Some(cell) = finished.detail.get(index) {
            Some(cell.is_stream_continuation())
        } else if index == 0 && finished.detail.is_empty() && !finished.message.trim().is_empty() {
            Some(false)
        } else {
            None
        }
    }

    pub(crate) fn project_cell(
        &self,
        key: SubagentKey,
        index: usize,
        width: u16,
        expanded: &std::collections::HashSet<String>,
    ) -> Option<crate::chatwidget::tool_projection::ToolProjection> {
        let id = NodeId(key.0);
        if let Some(node) = self.live.get(&id) {
            return node.project_cell(index, &self.context.cwd, width, expanded);
        }
        let finished = &self.ended.get(&id)?.finished;
        let mut projection = crate::chatwidget::tool_projection::ToolProjection::default();
        if let Some(cell) = finished.detail.get(index) {
            projection.push(cell.as_ref(), width, expanded);
        } else if index == 0 && finished.detail.is_empty() && !finished.message.trim().is_empty() {
            projection.push(
                &AgentMarkdownCell::new(finished.message.clone(), &self.context.cwd),
                width,
                expanded,
            );
        } else {
            return None;
        }
        Some(projection)
    }
    pub(crate) fn parent_for_key(&self, key: SubagentKey) -> Option<SubagentKey> {
        let id = NodeId(key.0);
        self.live
            .get(&id)
            .and_then(|node| node.history_parent)
            .or_else(|| self.ended.get(&id).and_then(|record| record.history_parent))
            .map(|id| SubagentKey(id.0))
    }
    pub(crate) fn agent_for_key(&self, key: SubagentKey) -> Option<&str> {
        let id = NodeId(key.0);
        self.live
            .get(&id)
            .and_then(|node| node.agent.as_deref())
            .or_else(|| {
                self.ended
                    .get(&id)
                    .and_then(|record| record.agent.as_deref())
            })
    }

    pub(crate) fn session_for_key(
        &self,
        key: SubagentKey,
    ) -> Option<nori_protocol::acp::v1::SessionId> {
        let id = NodeId(key.0);
        self.live
            .get(&id)
            .and_then(|node| node.session_id.clone())
            .or_else(|| {
                self.ended
                    .get(&id)
                    .and_then(|record| record.session_id.clone())
            })
    }

    /// Every subagent so far: the running ones and the finished ones.
    pub(crate) fn entries(&self, now: Instant) -> (Vec<Entry>, Vec<(SubagentKey, SubagentKey)>) {
        let mut entries = Vec::with_capacity(self.live.len() + self.ended.len());
        for (id, node) in &self.live {
            let parent_label = node
                .parent
                .and_then(|parent| self.live.get(&parent))
                .map(|parent| parent.label().to_owned())
                .or_else(|| {
                    node.history_parent
                        .and_then(|parent| self.ended.get(&parent))
                        .map(|record| record.finished.label.clone())
                });
            let parent = parent_label.map(|label| Parent {
                label,
                target: node
                    .parent
                    .or(node.history_parent)
                    .map(|parent| Target::Subagent(key(parent))),
            });
            entries.push(Entry {
                target: Target::Subagent(key(*id)),
                title: node.label().to_owned(),
                parent,
                agent: node
                    .agent
                    .clone()
                    .or_else(|| node.kind().map(str::to_owned)),
                model: node.model.clone(),
                status: if node.idle {
                    Status::Idle
                } else {
                    Status::Running
                },
                detail: None,
                transcript_text: node.searchable_text(),
                elapsed: now.saturating_duration_since(node.started_at),
                ended_ago: None,
                tool_count: node.tool_count,
                revision: node.revision,
                active_at: node.work_at,
            });
        }
        for (id, record) in &self.ended {
            if self.live.contains_key(id) {
                // A completed native child can resume the same ACP session.
                // Its live row owns the stable key while the ended record
                // remains the archived transcript.
                continue;
            }
            let finished = &record.finished;
            let (status, detail) = match &finished.outcome {
                SubagentOutcome::Completed { .. } => (Status::Completed, None),
                SubagentOutcome::Failed { reason } => (Status::Failed, reason.clone()),
                SubagentOutcome::Cancelled | SubagentOutcome::Closed => (Status::Cancelled, None),
            };
            entries.push(Entry {
                target: Target::Subagent(key(*id)),
                title: finished.label.clone(),
                parent: record.parent.clone().map(|label| Parent {
                    label,
                    target: record
                        .history_parent
                        .map(|parent| Target::Subagent(key(parent))),
                }),
                agent: record.agent.clone().or_else(|| finished.kind.clone()),
                model: finished.model.clone(),
                status,
                detail,
                transcript_text: finished.message.clone(),
                elapsed: finished.elapsed,
                ended_ago: Some(now.saturating_duration_since(record.ended_at)),
                tool_count: finished.tool_count,
                revision: record.revision,
                active_at: record.work_at,
            });
        }
        let replaced = self
            .replaced
            .iter()
            .map(|(old, new)| (key(*old), key(*new)))
            .collect();
        (entries, replaced)
    }

    /// The last `rows` rows of one subagent's conversation at `width`, and the revision they show.
    pub(crate) fn body(
        &self,
        subagent: SubagentKey,
        width: u16,
        rows: usize,
    ) -> Option<(i64, Vec<Line<'static>>)> {
        let id = NodeId(subagent.0);
        if let Some(node) = self.live.get(&id) {
            return Some((
                node.revision,
                node.detail_lines(&self.context.cwd, width, rows),
            ));
        }
        let finished = &self.ended.get(&id)?.finished;
        let mut tail = Tail::new(rows);
        for cell in finished.detail.iter().rev() {
            if tail.is_full() {
                break;
            }
            tail.push_front(cell_lines(cell.as_ref(), width));
        }
        // A child known only through its launcher has no detail, only the result.
        if finished.detail.is_empty() && !finished.message.trim().is_empty() {
            tail.push_front(cell_lines(
                &AgentMarkdownCell::new(finished.message.clone(), &self.context.cwd),
                width,
            ));
        }
        Some((self.ended.get(&id)?.revision, tail.finish()))
    }
}

fn key(id: NodeId) -> SubagentKey {
    SubagentKey(id.0)
}

impl ChatWidget {
    pub(crate) fn child_projection_len(&self, key: SubagentKey) -> Option<usize> {
        self.subagents.projection_len(key)
    }
    pub(crate) fn child_projection_revision(&self, key: SubagentKey, index: usize) -> Option<i64> {
        self.subagents.projection_revision(key, index)
    }
    pub(crate) fn child_projection_continuation(
        &self,
        key: SubagentKey,
        index: usize,
    ) -> Option<bool> {
        self.subagents.projection_continuation(key, index)
    }
    pub(crate) fn project_child_cell(
        &self,
        key: SubagentKey,
        index: usize,
        width: u16,
        expanded: &std::collections::HashSet<String>,
    ) -> Option<crate::chatwidget::tool_projection::ToolProjection> {
        self.subagents.project_cell(key, index, width, expanded)
    }
    pub(crate) fn replace_subagent_history(
        &mut self,
        key: SubagentKey,
        events: &[nori_protocol::SessionEvent],
    ) {
        let id = NodeId(key.0);
        let context = self.subagents.context.clone();
        self.subagents.revision += 1;
        if let Some(node) = self.subagents.live.get_mut(&id) {
            node.replace_replay(events, &context.cwd, context.animations);
        } else if let Some(ended) = self.subagents.ended.get_mut(&id) {
            ended.finished.detail =
                super::projection::project_child_history(events, &context.cwd).0;
            ended.revision = ended.revision.saturating_add(1);
        }
    }

    pub(crate) fn subagent_session(
        &self,
        key: SubagentKey,
    ) -> Option<nori_protocol::acp::v1::SessionId> {
        self.subagents.session_for_key(key)
    }

    /// The roster for the subagent switcher, with the root session first.
    pub(crate) fn subagent_roster(&self, now: Instant) -> Roster {
        let (subagents, replaced) = self.subagents.entries(now);
        let agent = self.bottom_pane.agent_display_name().to_owned();
        let model = self
            .agent_config
            .option_for_category(&nori_protocol::acp::v1::SessionConfigOptionCategory::Model)
            .map(crate::nori::agent_config_state::AgentConfigOption::display_value);
        let main = Entry {
            target: Target::Main,
            title: "main".to_owned(),
            parent: None,
            agent: (!agent.is_empty()).then_some(agent),
            model,
            status: if self.bottom_pane.is_task_running() {
                Status::Running
            } else {
                Status::Idle
            },
            detail: None,
            transcript_text: String::new(),
            elapsed: std::time::Duration::ZERO,
            ended_ago: None,
            tool_count: 0,
            revision: 0,
            active_at: now,
        };
        Roster {
            main,
            subagents,
            replaced,
            animations: self.subagents.animations(),
            tick: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| {
                    i64::try_from(since.as_millis() / SPINNER_PERIOD.as_millis().max(1))
                        .unwrap_or(0)
                }),
        }
    }

    /// The conversation tail of one subagent. The root session's is built by the app, which owns
    /// its transcript.
    pub(crate) fn subagent_body(&self, query: BodyQuery) -> Option<Body> {
        let Target::Subagent(subagent) = query.target else {
            return None;
        };
        let (revision, lines) = self.subagents.body(subagent, query.width, query.rows)?;
        Some(Body {
            query,
            revision,
            lines,
        })
    }

    /// Keep animation frames coming while a subagent runs and the switcher is up.
    pub(crate) fn subagent_overlay_tick(&mut self) {
        self.subagent_pre_draw_tick();
    }
}
