//! A tool call in the transcript.
//!
//! [`ClientToolCell`] holds the call's latest snapshot (and, for a run of reads and searches,
//! all of them) and draws it as the card that fits what it did:
//!
//! * [`explore`]: reads, searches and listings, as one line or a bento grid;
//! * [`shell`]: a command and the tail of its output;
//! * [`edit`]: a file change as a diff;
//! * [`native`]: Nori's own tools (tasks, git, goal);
//! * [`generic`]: anything else, one header deep.
//!
//! Every card is collapsed until it is expanded: its header says what happened, the body is the
//! evidence. [`evidence`] is the expanded body of last resort: the call's input and output as a
//! tree.

mod changes;
mod edit;
mod evidence;
mod explore;
mod generic;
mod native;
mod shell;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use ratatui::text::Line;

use crate::client_event_format::is_exploring_snapshot;
use crate::history_cell::HistoryCell;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Disclosure;
use crate::history_cell::chrome::Progress;
use crate::presentation::Invocation;
use crate::presentation::ToolKind;
use crate::presentation::ToolPhase;
use crate::presentation::ToolSnapshot;

pub(crate) use changes::changes_from_invocation;
pub(crate) use changes::diff_changes_from_artifacts;

#[derive(Debug)]
pub(crate) struct ClientToolCell {
    snapshot: ToolSnapshot,
    exploring_snapshots: Vec<ToolSnapshot>,
    cwd: PathBuf,
    edit_changes: HashMap<PathBuf, crate::ui_types::FileChange>,
    animations_enabled: bool,
    start_time: Option<Instant>,
    /// How long the call ran, once it stopped and was seen running.
    elapsed: Option<Duration>,
    expanded: AtomicBool,
    /// The call is proposed and waits for the user: it does not spin, it asks.
    awaiting_approval: AtomicBool,
    /// The rows of a finished call at the width and disclosure last asked for. A call that is
    /// done does not change, so the diff, the highlighting and the layout are paid for once.
    layout: Mutex<Option<CachedLayout>>,
}

#[derive(Debug)]
struct CachedLayout {
    width: u16,
    disclosure: Disclosure,
    theme: nori_tui_components::Theme,
    lines: Vec<Line<'static>>,
}

impl Clone for ClientToolCell {
    fn clone(&self) -> Self {
        Self {
            snapshot: self.snapshot.clone(),
            exploring_snapshots: self.exploring_snapshots.clone(),
            cwd: self.cwd.clone(),
            edit_changes: self.edit_changes.clone(),
            animations_enabled: self.animations_enabled,
            start_time: self.start_time,
            elapsed: self.elapsed,
            expanded: AtomicBool::new(self.expanded.load(Ordering::Relaxed)),
            awaiting_approval: AtomicBool::new(self.awaiting_approval.load(Ordering::Relaxed)),
            layout: Mutex::new(None),
        }
    }
}

/// Which card draws a call.
enum Card<'a> {
    Delegation(&'a crate::presentation::DelegationCall),
    Native(&'a crate::presentation::NativeCall),
    Explore,
    Shell,
    Edit,
    Generic,
}

impl ClientToolCell {
    pub(crate) fn new(snapshot: ToolSnapshot, cwd: PathBuf, animations_enabled: bool) -> Self {
        let start_time = is_active_phase(&snapshot.phase).then(Instant::now);
        let edit_changes = changes::changes_from_snapshot(&snapshot, &cwd);
        Self {
            snapshot,
            exploring_snapshots: Vec::new(),
            cwd,
            edit_changes,
            animations_enabled,
            start_time,
            elapsed: None,
            expanded: AtomicBool::new(false),
            awaiting_approval: AtomicBool::new(false),
            layout: Mutex::new(None),
        }
    }

    fn card(&self) -> Card<'_> {
        match &self.snapshot.invocation {
            Some(Invocation::Delegation(call)) => return Card::Delegation(call),
            Some(Invocation::Native(call)) => return Card::Native(call),
            _ => {}
        }
        if self.is_exploring() {
            return Card::Explore;
        }
        match &self.snapshot.kind {
            ToolKind::Execute if !generic::is_goal_lookup(&self.snapshot) => Card::Shell,
            ToolKind::Create | ToolKind::Edit | ToolKind::Delete | ToolKind::Move => Card::Edit,
            ToolKind::Read
            | ToolKind::Search
            | ToolKind::Execute
            | ToolKind::Fetch
            | ToolKind::Think
            | ToolKind::Other(_) => Card::Generic,
        }
    }

    /// How the call is going, which decides its glyph and colour.
    pub(super) fn progress(&self) -> Progress {
        match &self.snapshot.phase {
            ToolPhase::Pending | ToolPhase::InProgress
                if self.awaiting_approval.load(Ordering::Relaxed) =>
            {
                Progress::Waiting
            }
            ToolPhase::Pending | ToolPhase::InProgress => Progress::Running,
            ToolPhase::PendingApproval => Progress::Waiting,
            ToolPhase::Completed => Progress::Done,
            ToolPhase::Failed => Progress::Failed,
        }
    }

    /// The call at the given disclosure, as rows for `width`.
    fn render(&self, width: u16, disclosure: Disclosure) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let finished = !self.is_active();
        if finished
            && let Some(cached) = self
                .layout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            && cached.width == width
            && cached.disclosure == disclosure
            && cached.theme == chrome.theme
        {
            return cached.lines.clone();
        }
        let lines = self.render_uncached(&chrome, width, disclosure);
        if finished {
            *self
                .layout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CachedLayout {
                width,
                disclosure,
                theme: chrome.theme,
                lines: lines.clone(),
            });
        }
        lines
    }

    fn render_uncached(
        &self,
        chrome: &Chrome,
        width: u16,
        disclosure: Disclosure,
    ) -> Vec<Line<'static>> {
        let chrome = *chrome;
        let width = usize::from(width).max(12);
        match self.card() {
            Card::Delegation(call) => native::delegation(self, &chrome, width, call),
            Card::Native(call) => native::native(self, &chrome, width, call),
            Card::Explore => explore::render(self, &chrome, width, disclosure),
            Card::Shell => shell::render(self, &chrome, width, disclosure),
            Card::Edit => edit::render(self, &chrome, width, disclosure),
            Card::Generic => {
                // Paths inside the working directory are never shown absolute.
                let mut lines = generic::render(self, &chrome, width, disclosure);
                let prefix = format!("{}/", self.cwd.display());
                for span in lines.iter_mut().flat_map(|line| line.spans.iter_mut()) {
                    if span.content.contains(&prefix) {
                        span.content = span.content.replace(&prefix, "").into();
                    }
                }
                lines
            }
        }
    }

    /// The call's header and, at most, the few rows that say how it came out.
    pub(crate) fn compact_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.render(width, Disclosure::Collapsed)
    }

    /// The call with all of its evidence, for the shared conversation view.
    pub(crate) fn expanded_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.render(width, Disclosure::Expanded)
    }

    /// The call is a proposal waiting for the user's answer: its card says so instead of
    /// spinning, and says what it would do, not what it did.
    pub(crate) fn set_awaiting_approval(&mut self, awaiting: bool) {
        *self
            .layout
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.awaiting_approval.store(awaiting, Ordering::Relaxed);
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.snapshot.call_id
    }

    pub(crate) fn is_active(&self) -> bool {
        is_active_phase(&self.snapshot.phase)
            || self
                .exploring_snapshots
                .iter()
                .any(|snapshot| is_active_phase(&snapshot.phase))
    }

    pub(crate) fn snapshot_kind(&self) -> &ToolKind {
        &self.snapshot.kind
    }

    /// Whether this is a call to one of Nori's delegated-task tools.
    pub(crate) fn is_delegation(&self) -> bool {
        matches!(self.snapshot.invocation, Some(Invocation::Delegation(_)))
    }

    /// A call to one of Nori's own tools, which reads as its result once it has one.
    pub(crate) fn is_native(&self) -> bool {
        matches!(self.snapshot.invocation, Some(Invocation::Native(_)))
    }

    pub(crate) fn is_exploring(&self) -> bool {
        is_exploring_snapshot(&self.snapshot) || !self.exploring_snapshots.is_empty()
    }

    /// Every call id held by this cell's exploring group, so completions that arrive after it
    /// is flushed are not merged into a later cell.
    pub(crate) fn exploring_call_ids(&self) -> Vec<String> {
        self.exploring_snapshots
            .iter()
            .map(|snapshot| snapshot.call_id.clone())
            .collect()
    }

    /// Make this an exploring cell; its own snapshot becomes the group's first.
    pub(crate) fn mark_exploring(&mut self) {
        *self
            .layout
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if self.exploring_snapshots.is_empty() {
            self.exploring_snapshots.push(self.snapshot.clone());
        }
    }

    /// Add another call to the exploring group, or update the one already in it.
    pub(crate) fn merge_exploring(&mut self, snapshot: ToolSnapshot) {
        *self
            .layout
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if let Some(existing) = self
            .exploring_snapshots
            .iter_mut()
            .find(|existing| existing.call_id == snapshot.call_id)
        {
            *existing = snapshot;
            return;
        }
        self.exploring_snapshots.push(snapshot);
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: ToolSnapshot) {
        *self
            .layout
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if self.snapshot.call_id != snapshot.call_id {
            return;
        }
        if self.start_time.is_none() && is_active_phase(&snapshot.phase) {
            self.start_time = Some(Instant::now());
        }
        if !is_active_phase(&snapshot.phase) {
            self.stop_clock();
        }
        // The answer came, or the call moved on: it is no longer waiting.
        if matches!(
            snapshot.phase,
            ToolPhase::InProgress | ToolPhase::Completed | ToolPhase::Failed
        ) {
            self.awaiting_approval.store(false, Ordering::Relaxed);
        }
        // Updates carry the real path or query; the group's entry for this call takes them too.
        if let Some(existing) = self
            .exploring_snapshots
            .iter_mut()
            .find(|existing| existing.call_id == snapshot.call_id)
        {
            *existing = snapshot.clone();
        }
        self.edit_changes = changes::changes_from_snapshot(&snapshot, &self.cwd);
        self.snapshot = snapshot;
    }

    pub(crate) fn mark_failed(&mut self) {
        *self
            .layout
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if is_active_phase(&self.snapshot.phase) {
            self.snapshot.phase = ToolPhase::Failed;
        }
        for snapshot in &mut self.exploring_snapshots {
            if is_active_phase(&snapshot.phase) {
                snapshot.phase = ToolPhase::Failed;
            }
        }
        self.stop_clock();
    }

    /// The call stopped: how long it ran is kept, and the spinner stops.
    fn stop_clock(&mut self) {
        if let Some(started) = self.start_time.take() {
            self.elapsed = Some(started.elapsed());
        }
    }
}

/// The text of a failed call's error, from its raw output.
pub(super) fn error_text(snapshot: &ToolSnapshot) -> Option<String> {
    let raw = snapshot.raw_output.as_ref()?;
    for key in ["error", "stderr", "output"] {
        if let Some(text) = raw.get(key).and_then(serde_json::Value::as_str)
            && !text.is_empty()
        {
            return Some(text.to_owned());
        }
    }
    raw.as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn is_active_phase(phase: &ToolPhase) -> bool {
    matches!(
        phase,
        ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress
    )
}

impl HistoryCell for ClientToolCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.render(
            width,
            if self.expanded.load(Ordering::Relaxed) {
                Disclosure::Expanded
            } else {
                Disclosure::Collapsed
            },
        )
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.expanded_lines(width)
    }

    fn animated(&self) -> bool {
        self.is_active()
    }

    fn expansion(&self) -> Option<bool> {
        match self.card() {
            Card::Delegation(_) | Card::Native(_) => None,
            Card::Explore | Card::Shell | Card::Edit | Card::Generic => {
                Some(self.expanded.load(Ordering::Relaxed))
            }
        }
    }

    fn set_expanded(&self, expanded: bool) {
        self.expanded.store(expanded, Ordering::Relaxed);
    }
}
