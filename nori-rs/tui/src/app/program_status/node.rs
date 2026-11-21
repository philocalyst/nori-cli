//! One record the terminal holds: the root conversation or one subagent, and what is known
//! about it. A node is turned into a [`Report`] whenever something about it changes.

use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::acp::v1::RequestId;
use nori_protocol::acp::v1::ToolCallId;

use crate::status_indicator_widget::fmt_elapsed_compact;
use crate::terminal_status::Kind;
use crate::terminal_status::Report;
use crate::terminal_status::State;

/// Where a node is in its work, when it is not waiting on the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Idle,
    Working,
    Done,
    Error,
}

/// What a blocked node waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BlockKey {
    /// An agent request (permission, question) that has not been answered.
    Request(RequestId),
    /// A sign-in the user is in the middle of.
    Auth,
}

#[derive(Clone, Debug)]
pub(super) struct Block {
    pub(super) key: BlockKey,
    pub(super) kind: Kind,
    pub(super) msg: String,
}

/// What one turn of a node has done so far; the summary of a finished turn is made of it.
#[derive(Debug)]
pub(super) struct Turn {
    pub(super) started: Instant,
    pub(super) tools: HashSet<ToolCallId>,
    pub(super) children: i64,
}

impl Turn {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            tools: HashSet::new(),
            children: 0,
        }
    }

    /// `Finished in 1m 12s · 5 tool calls · 2 subagents`.
    fn summary(&self) -> String {
        let secs = self.started.elapsed().as_secs();
        let mut parts = vec![if secs == 0 {
            "Finished".to_owned()
        } else {
            format!("Finished in {}", fmt_elapsed_compact(secs))
        }];
        for (count, one, many) in [
            (self.tools.len(), "tool call", "tool calls"),
            (
                usize::try_from(self.children).unwrap_or_default(),
                "subagent",
                "subagents",
            ),
        ] {
            match count {
                0 => {}
                1 => parts.push(format!("1 {one}")),
                _ => parts.push(format!("{count} {many}")),
            }
        }
        parts.join(" · ")
    }
}

pub(super) struct Node {
    pub(super) path: String,
    pub(super) title: Option<String>,
    /// What the node is, shown while it has no activity: a subagent's type.
    pub(super) detail: Option<String>,
    pub(super) phase: Phase,
    /// What the node is doing right now: the tool it runs, "Thinking".
    pub(super) activity: Option<String>,
    pub(super) progress: Option<i64>,
    /// What a finished turn says, or what failed.
    pub(super) outcome: Option<String>,
    /// Everything the node waits on the user for; the newest is the one shown.
    pub(super) blocks: Vec<Block>,
    pub(super) current_tool: Option<ToolCallId>,
    pub(super) turn: Turn,
    /// What the terminal holds for this node, and when it was told.
    pub(super) sent: Option<Report>,
    pub(super) sent_at: Option<Instant>,
    /// A change is waiting for the rate limit.
    pub(super) dirty: bool,
}

impl Node {
    pub(super) fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            title: None,
            detail: None,
            phase: Phase::Idle,
            activity: None,
            progress: None,
            outcome: None,
            blocks: Vec::new(),
            current_tool: None,
            turn: Turn::new(),
            sent: None,
            sent_at: None,
            dirty: false,
        }
    }

    /// The node starts a turn. Nothing carries over from the last one.
    pub(super) fn begin_turn(&mut self) {
        if self.phase == Phase::Working {
            return;
        }
        self.phase = Phase::Working;
        self.activity = None;
        self.progress = None;
        self.outcome = None;
        self.current_tool = None;
        self.turn = Turn::new();
    }

    /// The turn ended without a result to look at: cancelled, or the agent has gone quiet.
    pub(super) fn rest(&mut self) {
        self.phase = Phase::Idle;
        self.activity = None;
        self.progress = None;
        self.outcome = None;
        self.drop_request_blocks();
    }

    pub(super) fn finish(&mut self, outcome: Option<String>) {
        self.phase = Phase::Done;
        self.activity = None;
        self.progress = None;
        self.outcome = Some(outcome.unwrap_or_else(|| self.turn.summary()));
        self.drop_request_blocks();
    }

    pub(super) fn fail(&mut self, message: Option<String>) {
        self.phase = Phase::Error;
        self.activity = None;
        self.progress = None;
        self.outcome = message;
        self.drop_request_blocks();
    }

    /// A turn that ended answers every request made during it.
    fn drop_request_blocks(&mut self) {
        self.blocks
            .retain(|block| !matches!(block.key, BlockKey::Request(_)));
    }

    pub(super) fn add_block(&mut self, block: Block) {
        self.blocks.retain(|existing| existing.key != block.key);
        self.blocks.push(block);
    }

    pub(super) fn remove_block(&mut self, key: &BlockKey) {
        self.blocks.retain(|block| &block.key != key);
    }

    /// Only a working node has activity to show.
    pub(super) fn set_activity(&mut self, activity: impl Into<String>) {
        if self.phase == Phase::Working {
            self.activity = Some(activity.into());
        }
    }

    pub(super) fn report(&self) -> Report {
        let mut report = if let Some(block) = self.blocks.last() {
            let mut report = Report::new(&self.path, State::Blocked);
            report.kind = Some(block.kind);
            report.progress = self.progress;
            report.msg = Some(match self.blocks.len() {
                1 => block.msg.clone(),
                more => format!("{} (+{} more)", block.msg, more - 1),
            });
            report
        } else {
            let state = match self.phase {
                Phase::Idle => State::Idle,
                Phase::Working => State::Working,
                Phase::Done => State::Done,
                Phase::Error => State::Error,
            };
            let mut report = Report::new(&self.path, state);
            match self.phase {
                Phase::Idle => {}
                Phase::Working => {
                    report.progress = self.progress;
                    report.msg = self.activity.clone().or_else(|| self.detail.clone());
                }
                Phase::Done | Phase::Error => report.msg = self.outcome.clone(),
            }
            report
        };
        report.title = self.title.clone();
        report
    }

    /// When the node's waiting change may go out, given the rate limit.
    pub(super) fn due(&self, interval: Duration) -> Option<Instant> {
        self.sent_at.map(|at| at + interval)
    }
}
