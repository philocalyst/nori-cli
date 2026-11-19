//! The `/resume` picker: the Helix engine over recorded sessions, with full-text search over
//! the on-disk index and a preview of the selected session's conversation.
//!
//! Opening is instant: rows come from transcript metadata. In the background the index brings
//! every session's digest up to date (newest first) and the rows fill in with titles, branches
//! and turn counts. A query searches the rows' fields at once and the full text of every
//! session on a worker; a session whose words are in its conversation, not its title, appears
//! with `N hits · …context…` as soon as the worker answers.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use nori_harness::session_index::Lineage;
use nori_harness::session_index::SessionIndex;
use nori_protocol::acp::v1::SessionInfo;
use nori_tui_components::FilterKey;
use nori_tui_components::HelixConfig;
use nori_tui_components::HelixEvent;
use nori_tui_components::HelixState;
use nori_tui_components::Hit;
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use super::SessionPickerInfo;
use super::background::Background;
use super::background::Message;
use super::background::Search;
pub(crate) use super::helix_entry::ResumeAction;
pub(crate) use super::helix_entry::ResumeEntry;
pub(crate) use super::helix_entry::ResumeOutcome;
pub(crate) use super::helix_entry::SessionPurpose;
use super::preview;
use crate::helix_picker::feed;
use crate::style::component_theme;
use crate::tui;
use crate::tui::TuiEvent;

const FILTERS: &[FilterKey] = &[
    FilterKey::new("agent", Some('@')),
    FilterKey::new("branch", None),
    FilterKey::new("model", None),
    FilterKey::new("cwd", None),
    FilterKey::new("id", None),
];

pub(crate) struct ResumeOverlay {
    pub(super) state: HelixState<ResumeEntry>,
    /// Every session the picker was opened with; the list shows the roots and their children.
    all: Vec<ResumeEntry>,
    nori_home: Option<PathBuf>,
    channel: (UnboundedSender<Message>, UnboundedReceiver<Message>),
    background: Option<Background>,
    /// Session id to the row it is shown in.
    rows: HashMap<String, i64>,
    preview_inflight: Option<nori_tui_components::PreviewRequest<i64>>,
    cancel_candidate_on_dismiss: bool,
    outcome: ResumeOutcome,
    interrupted: bool,
    /// The area the picker was last drawn in.
    area: Rect,
}

impl ResumeOverlay {
    /// A picker over the transcripts of this `NORI_HOME`, to resume one.
    pub(crate) fn new_local(sessions: Vec<SessionPickerInfo>, nori_home: PathBuf) -> Self {
        Self::new_sessions(
            SessionPurpose::Resume,
            sessions.into_iter().map(|info| (info, None)).collect(),
            nori_home,
        )
    }

    /// A picker over the transcripts of this `NORI_HOME`; what choosing a row does is
    /// `purpose`'s. Each session comes with the agent that recorded it, when known.
    pub(crate) fn new_sessions(
        purpose: SessionPurpose,
        sessions: Vec<(SessionPickerInfo, Option<String>)>,
        nori_home: PathBuf,
    ) -> Self {
        let entries = sessions
            .iter()
            .enumerate()
            .map(|(position, (info, agent))| {
                ResumeEntry::from_local(
                    info,
                    agent.clone(),
                    nori_home.clone(),
                    i64::try_from(position).unwrap_or(0),
                    purpose,
                )
            })
            .collect();
        Self::build(entries, Some(nori_home), false, purpose.title())
    }

    /// A picker over the sessions an agent listed itself, with "start a new session" first.
    pub(crate) fn new_acp(sessions: Vec<SessionInfo>, cancel_candidate_on_dismiss: bool) -> Self {
        let mut entries = vec![ResumeEntry::start_new()];
        entries.extend(sessions.iter().enumerate().map(|(position, session)| {
            ResumeEntry::from_acp(session, i64::try_from(position + 1).unwrap_or(0))
        }));
        Self::build(
            entries,
            None,
            cancel_candidate_on_dismiss,
            SessionPurpose::Resume.title(),
        )
    }

    pub(super) fn build(
        entries: Vec<ResumeEntry>,
        nori_home: Option<PathBuf>,
        cancel_candidate_on_dismiss: bool,
        title: &str,
    ) -> Self {
        let mut state = HelixState::new(HelixConfig {
            title: title.to_owned(),
            placeholder: "search sessions".to_owned(),
            empty_text: "No sessions recorded for this project yet.".to_owned(),
            no_match_text: "No session matches. Try fewer words, or remove an @filter.".to_owned(),
            filter_keys: FILTERS,
            preview: true,
            multi: false,
            searchable: true,
            on_demand: false,
            numbered: false,
            subtitle: None,
        });
        let rows = entries
            .iter()
            .map(|entry| (entry.session_id.clone(), entry.ordinal))
            .collect();
        state.set_items(arrange(&entries));
        Self {
            state,
            all: entries,
            nori_home,
            channel: tokio::sync::mpsc::unbounded_channel(),
            background: None,
            rows,
            preview_inflight: None,
            cancel_candidate_on_dismiss,
            outcome: ResumeOutcome::Pending,
            interrupted: false,
            area: Rect::default(),
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        !matches!(self.outcome, ResumeOutcome::Pending)
    }

    /// Whether the picker was closed with Ctrl-C, which a host that can quit takes to mean it.
    pub(crate) fn was_interrupted(&self) -> bool {
        self.interrupted
    }

    pub(crate) fn take_outcome(&mut self) -> ResumeOutcome {
        std::mem::replace(&mut self.outcome, ResumeOutcome::Pending)
    }

    fn start(&mut self, tui: &tui::Tui) {
        if self.background.is_some() {
            return;
        }
        if let Some(nori_home) = &self.nori_home {
            // This picker's sessions are indexed before the rest of the history.
            let first = self
                .state
                .items()
                .iter()
                .map(|entry| entry.session_id.clone())
                .collect();
            self.background = Some(Background::start(
                SessionIndex::open(nori_home),
                first,
                self.channel.0.clone(),
                tui.frame_requester(),
            ));
        }
    }

    fn drain(&mut self) {
        let mut changed = false;
        while let Ok(message) = self.channel.1.try_recv() {
            match message {
                Message::Headers(headers) => {
                    let lineage = Lineage::of(&headers);
                    for header in headers {
                        let Some(&ordinal) = self.rows.get(&header.session_id) else {
                            continue;
                        };
                        let child = lineage.is_child(&header);
                        let parent = lineage
                            .parent_of(&header)
                            .and_then(|parent| self.rows.get(parent).copied());
                        let Some(entry) =
                            self.all.iter_mut().find(|entry| entry.ordinal == ordinal)
                        else {
                            continue;
                        };
                        entry.apply_header(&header);
                        entry.delegated = child;
                        entry.parent = parent;
                        // A session in which nothing was asked has nothing to resume, and a
                        // child whose parent is not listed has no way to be opened.
                        entry.hidden = header.user_turns == 0 || (child && parent.is_none());
                    }
                    changed = true;
                }
                Message::Progress(progress) => {
                    let status = (progress.done < progress.total)
                        .then(|| format!("indexing {} / {}", progress.done, progress.total));
                    self.state.set_status(status);
                }
                Message::Hits { generation, hits } => {
                    let hits: HashMap<i64, Hit> = hits
                        .into_iter()
                        .filter_map(|hit| {
                            let ordinal = *self.rows.get(&hit.session_id)?;
                            Some((
                                ordinal,
                                Hit {
                                    score: hit.score,
                                    count: usize::try_from(hit.turns_matched).unwrap_or(0),
                                    snippet: Some(hit.snippet),
                                },
                            ))
                        })
                        .collect();
                    self.state.set_hits(generation, hits);
                }
                Message::Preview(preview) => {
                    self.preview_inflight = None;
                    self.state.set_preview(preview);
                }
            }
        }
        if changed {
            self.state.set_items(arrange(&self.all));
        }
    }

    /// Hand the engine's pending questions to the background: full-text search and previews.
    fn serve(&mut self, area: Rect) {
        if let (Some(request), Some(background)) =
            (self.state.take_search_request(), &self.background)
        {
            background.search(Search {
                generation: request.generation,
                terms: request.terms,
                scope: self.rows.keys().cloned().collect::<HashSet<_>>(),
            });
        }
        let Some(request) = self.state.preview_request(area, Instant::now()) else {
            return;
        };
        if self.preview_inflight.as_ref() == Some(&request) {
            return;
        }
        let Some(entry) = self.state.selected().cloned() else {
            return;
        };
        let theme = component_theme();
        match &self.background {
            Some(background) if entry.project_id.is_some() => {
                self.preview_inflight = Some(request.clone());
                background.load_preview(request, entry, theme);
            }
            Some(_) | None => {
                let built = preview::from_entry(&request, &entry, &theme);
                self.state.set_preview(built);
            }
        }
    }

    fn apply(&mut self, event: HelixEvent<i64>) {
        match event {
            HelixEvent::Idle
            | HelixEvent::Redraw
            | HelixEvent::Action(..)
            | HelixEvent::Composed(..)
            | HelixEvent::SubmitMany(_)
            | HelixEvent::Secondary => {}
            HelixEvent::Submit(ordinal) => {
                if let Some(entry) = self
                    .state
                    .items()
                    .iter()
                    .find(|entry| entry.ordinal == ordinal)
                {
                    self.outcome = ResumeOutcome::Selected(entry.action.clone());
                }
            }
            HelixEvent::Interrupt => {
                self.interrupted = true;
                self.outcome = ResumeOutcome::Cancelled {
                    cancel_candidate: self.cancel_candidate_on_dismiss,
                };
            }
            HelixEvent::Cancel => {
                self.outcome = ResumeOutcome::Cancelled {
                    cancel_candidate: self.cancel_candidate_on_dismiss,
                };
            }
        }
    }

    pub(crate) fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> std::io::Result<()> {
        self.start(tui);
        match event {
            TuiEvent::Draw => {
                self.drain();
                // What the preview needs is the size of the area the picker was drawn in, which
                // starts wherever the stage's region does, not at the top of the screen.
                if self.area.is_empty() {
                    let size = tui.terminal.size()?;
                    self.area = Rect::new(0, 0, size.width, size.height);
                }
                self.serve(self.area);
                let theme = component_theme();
                tui.draw_overlay(|frame| {
                    self.area = frame.area();
                    self.state.render(frame.area(), frame.buffer, &theme, 0);
                })?;
            }
            TuiEvent::Key(_) | TuiEvent::Paste(_) | TuiEvent::Mouse(_) => {
                if let Some(helix) = feed(&mut self.state, &event) {
                    self.apply(helix);
                }
                tui.frame_requester().schedule_frame();
            }
        }
        Ok(())
    }
}

/// The entries as the list shows them: roots in their order, each followed by its children
/// (and theirs) with tree guides. A child opens its root, so it carries the root's action.
fn arrange(all: &[ResumeEntry]) -> Vec<ResumeEntry> {
    fn walk(
        parent: &ResumeEntry,
        inherited: &str,
        all: &[ResumeEntry],
        out: &mut Vec<ResumeEntry>,
        depth: usize,
    ) {
        let children: Vec<&ResumeEntry> = all
            .iter()
            .filter(|entry| !entry.hidden && entry.parent == Some(parent.ordinal))
            .collect();
        let count = children.len();
        for (index, child) in children.into_iter().enumerate() {
            let last = index + 1 == count;
            let mut shown = child.clone();
            shown.guides = format!("{inherited}{}", if last { "└─ " } else { "├─ " });
            shown.action = parent.action.clone();
            out.push(shown);
            // A tree deeper than this is a loop in the data, not a conversation.
            if depth < 8 {
                let next = format!("{inherited}{}", if last { "   " } else { "│  " });
                walk(child, &next, all, out, depth + 1);
            }
        }
    }
    let mut out = Vec::with_capacity(all.len());
    for root in all
        .iter()
        .filter(|entry| !entry.hidden && entry.parent.is_none())
    {
        out.push(root.clone());
        walk(root, "", all, &mut out, 0);
    }
    out
}
