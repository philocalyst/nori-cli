//! Subagent sessions in the chat widget.
//!
//! A subagent's output is not part of the root conversation: its text, tool calls, idle state and
//! usage belong to it alone. The tree here keeps one node per running subagent and turns a
//! finished child into one summary cell for the scrollback, carrying the child's full detail for
//! the transcript overlay.
//!
//! Nodes come from two sources. Agents that implement the subagents proposal announce children
//! explicitly. For agents that do not, a root tool call that launches a subagent (`Agent`/`Task`),
//! and the root-session updates tagged with its id, stand in for the announcement.

mod cell;
mod census;
mod events;
mod lanes;
mod node;
mod projection;
mod roster;
mod tree;

pub(crate) use crate::history_cell::chrome::SPINNER_FRAMES;
pub(crate) use crate::history_cell::chrome::SPINNER_PERIOD;
pub(crate) use cell::SubagentSummaryCell;
pub(crate) use cell::format_elapsed;
pub(crate) use cell::tool_count_text;
pub(crate) use lanes::InChatSubagents;
pub(crate) use projection::project_child_history;
pub(crate) use tree::ChildTarget;
pub(crate) use tree::FinishedSubagent;
pub(crate) use tree::SubagentTree;

#[cfg(test)]
mod completion_tests;
#[cfg(test)]
mod roster_tests;
#[cfg(test)]
mod tree_tests;
