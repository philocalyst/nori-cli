//! What the switcher is told about each session: one [`Entry`] per row, and the [`Body`] of
//! conversation shown for one of them.
//!
//! These are plain snapshots. The chat widget builds them from the subagent tree; the overlay
//! never reaches back into it, so the overlay can be built and tested from literals.

use std::time::Duration;
use std::time::Instant;

use ratatui::text::Line;

/// Identity of a subagent for as long as the conversation lasts. It is the tree's node id, which
/// is never reused, so a row stays the same row while its state changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SubagentKey(pub(crate) i64);

/// What a row stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    /// The root conversation.
    Main,
    Subagent(SubagentKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// The root session while no turn runs.
    Idle,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    /// The word fuzzy search matches and the preview header shows.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn is_running(self) -> bool {
        match self {
            Self::Running => true,
            Self::Idle | Self::Completed | Self::Failed | Self::Cancelled => false,
        }
    }
}

/// Who started a subagent: its name, and its row when that is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parent {
    pub(crate) label: String,
    pub(crate) target: Option<Target>,
}

/// One row of the switcher.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) target: Target,
    /// The task, or the type of subagent when no task is known.
    pub(crate) title: String,
    /// The subagent that started this one, so a nested child reads `parent › child`.
    pub(crate) parent: Option<Parent>,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) status: Status,
    /// Why it failed, when it said.
    pub(crate) detail: Option<String>,
    /// Flattened child transcript text for full-text search.
    pub(crate) transcript_text: String,
    /// How long it has run, or ran.
    pub(crate) elapsed: Duration,
    /// How long ago it ended.
    pub(crate) ended_ago: Option<Duration>,
    pub(crate) tool_count: usize,
    /// Changes whenever the conversation of this session does, and nowhere else.
    pub(crate) revision: i64,
    /// When it last did something, which orders running rows.
    pub(crate) active_at: Instant,
}

impl Entry {
    /// Whether everything the text search looks at is the same. While it is, a new roster needs
    /// no new search, only fresher times and counts.
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.target == other.target
            && self.status == other.status
            && self.title == other.title
            && self.parent == other.parent
            && self.agent == other.agent
            && self.model == other.model
            && self.detail == other.detail
    }
}

/// Everything the list needs, taken at one instant.
#[derive(Debug, Clone)]
pub(crate) struct Roster {
    /// The root session, always first.
    pub(crate) main: Entry,
    /// Every subagent the conversation had, in no particular order.
    pub(crate) subagents: Vec<Entry>,
    /// A subagent known first by the call that launched it is replaced by its own session once
    /// that is announced: `(old, new)`.
    pub(crate) replaced: Vec<(SubagentKey, SubagentKey)>,
    /// Whether the spinner animates.
    pub(crate) animations: bool,
    /// The spinner's phase, in steps of its period.
    pub(crate) tick: i64,
}

impl Roster {
    pub(crate) fn entry(&self, target: Target) -> Option<&Entry> {
        match target {
            Target::Main => Some(&self.main),
            Target::Subagent(_) => self.subagents.iter().find(|entry| entry.target == target),
        }
    }

    /// The row `target` became, following replacements.
    pub(crate) fn resolve(&self, target: Target) -> Target {
        let mut current = target;
        // Replacements never form a cycle (a new id always replaces an older one); the bound
        // only keeps a corrupt list from looping.
        for _ in 0..self.replaced.len() {
            let Target::Subagent(key) = current else {
                break;
            };
            match self.replaced.iter().find(|(old, _)| *old == key) {
                Some((_, new)) => current = Target::Subagent(*new),
                None => break,
            }
        }
        current
    }
}

/// Which conversation tail to fetch, at which size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyQuery {
    pub(crate) target: Target,
    pub(crate) width: u16,
    /// The most rows from the end wanted; `usize::MAX` for all of it.
    pub(crate) rows: usize,
}

/// The tail of one session's conversation, laid out at a width.
#[derive(Debug, Clone)]
pub(crate) struct Body {
    pub(crate) query: BodyQuery,
    /// The [`Entry::revision`] it was built from.
    pub(crate) revision: i64,
    pub(crate) lines: Vec<Line<'static>>,
}
