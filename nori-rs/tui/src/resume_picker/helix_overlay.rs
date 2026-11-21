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
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
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
use super::load;
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
    /// Every session the picker was opened with, in the order it lists them.
    all: Vec<ResumeEntry>,
    nori_home: Option<PathBuf>,
    channel: (UnboundedSender<Message>, UnboundedReceiver<Message>),
    background: Option<Background>,
    /// Session id to the row it is shown in.
    rows: HashMap<String, i64>,
    preview_inflight: Option<nori_tui_components::PreviewRequest<i64>>,
    cancel_candidate_on_dismiss: bool,
    outcome: ResumeOutcome,
    /// The area the picker was last drawn in.
    area: Rect,
    /// Set when the picker can switch between this project's sessions and all chats.
    switch: Option<Switch>,
}

/// The narrow scope's project and agent: the picker can switch to all chats and back.
#[derive(Debug, Clone)]
pub(crate) struct Widen {
    pub(crate) cwd: PathBuf,
    pub(crate) agent: String,
}

/// What a picker that switches scope keeps to rebuild its rows.
struct Switch {
    widen: Widen,
    /// Whether the rows are every project's and every agent's sessions.
    all_chats: bool,
    purpose: SessionPurpose,
    /// The conversation id of the session the picker was opened from.
    current: Option<String>,
    offer_new: bool,
    /// The narrow scope's words, as the bottom edge names them.
    scope: String,
    /// The agent's own sessions the narrow scope lists, shown again when switching back.
    agent_sessions: Vec<SessionInfo>,
}

/// What a resume picker is opened with.
#[derive(Debug, Clone)]
pub(crate) struct ResumeSetup {
    pub(crate) purpose: SessionPurpose,
    pub(crate) nori_home: PathBuf,
    /// Nori's records of the sessions, most recently active first.
    pub(crate) sessions: Vec<SessionPickerInfo>,
    /// Sessions the agent lists that Nori has no record of.
    pub(crate) agent_sessions: Vec<SessionInfo>,
    /// The conversation id of the session the picker was opened from, to mark its row.
    pub(crate) current: Option<String>,
    /// Pin a "Start a new session" row above the sessions.
    pub(crate) offer_new: bool,
    pub(crate) cancel_candidate_on_dismiss: bool,
    /// What the list covers, for its bottom edge: `this project`, `claude-code · this project`.
    pub(crate) scope: String,
    /// The narrow scope's project and agent, when the picker can switch to all chats and back.
    pub(crate) widen: Option<Widen>,
}

impl ResumeOverlay {
    /// A picker over one list: Nori's records and the agent's own sessions that Nori has no
    /// record of, together, most recently active first. Without `offer_new` the selection
    /// opens on the newest session; Enter resumes it.
    pub(crate) fn new(setup: ResumeSetup) -> Self {
        let ResumeSetup {
            purpose,
            nori_home,
            sessions,
            agent_sessions,
            current,
            offer_new,
            cancel_candidate_on_dismiss,
            scope,
            widen,
        } = setup;
        let entries = build_entries(
            &sessions,
            &agent_sessions,
            &nori_home,
            purpose,
            current.as_deref(),
            offer_new,
        );
        let (subtitle, empty_text) = if widen.is_some() {
            switch_text(&scope, false)
        } else {
            (scope_line(&scope), empty_text(&scope))
        };
        let switch = widen.map(|widen| Switch {
            widen,
            all_chats: false,
            purpose,
            current,
            offer_new,
            scope,
            agent_sessions,
        });
        let mut overlay = Self::build(
            entries,
            Some(nori_home),
            cancel_candidate_on_dismiss,
            purpose.title(),
            subtitle,
            empty_text,
        );
        overlay.switch = switch;
        if offer_new {
            overlay.state.select(1);
        }
        overlay
    }

    pub(super) fn build(
        entries: Vec<ResumeEntry>,
        nori_home: Option<PathBuf>,
        cancel_candidate_on_dismiss: bool,
        title: &str,
        subtitle: Option<String>,
        empty_text: String,
    ) -> Self {
        let mut state = HelixState::new(HelixConfig {
            title: title.to_owned(),
            placeholder: "search sessions".to_owned(),
            empty_text,
            no_match_text: "No session matches. Try fewer words, or remove an @filter.".to_owned(),
            filter_keys: FILTERS,
            preview: true,
            multi: false,
            searchable: true,
            on_demand: false,
            numbered: false,
            subtitle,
        });
        let rows = rows_of(&entries);
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
            area: Rect::default(),
            switch: None,
        }
    }

    /// Add sessions the agent listed after the picker opened, in the order of their activity
    /// among the rows already shown. A session already listed is not added twice.
    pub(crate) fn add_agent_sessions(&mut self, sessions: &[SessionInfo]) {
        let mut next = self
            .all
            .iter()
            .map(|entry| entry.ordinal)
            .max()
            .unwrap_or(-1)
            + 1;
        for session in sessions {
            if self.rows.contains_key(&session.session_id.to_string()) {
                continue;
            }
            if let Some(switch) = &mut self.switch {
                // Kept for the narrow scope, which lists them whenever it is shown again.
                switch.agent_sessions.push(session.clone());
                if switch.all_chats {
                    continue;
                }
            }
            let mut entry = ResumeEntry::from_acp(session, 0);
            entry.ordinal = next;
            next += 1;
            self.rows.insert(entry.session_id.clone(), entry.ordinal);
            let active = active_ms(&entry.active_at);
            let at = self
                .all
                .iter()
                .position(|listed| {
                    !matches!(listed.action, ResumeAction::StartFresh)
                        && active_ms(&listed.active_at) < active
                })
                .unwrap_or(self.all.len());
            self.all.insert(at, entry);
        }
        self.state.set_items(arrange(&self.all));
    }

    pub(crate) fn is_done(&self) -> bool {
        !matches!(self.outcome, ResumeOutcome::Pending)
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
                    // A session the agent listed is found in Nori's records by any agent id the
                    // conversation ran in; the record's id names its row too.
                    for header in &headers {
                        if let Some(&ordinal) = header
                            .acp_session_ids
                            .iter()
                            .find_map(|acp_id| self.rows.get(acp_id))
                        {
                            self.rows.insert(header.session_id.clone(), ordinal);
                        }
                    }
                    for header in headers {
                        let Some(&ordinal) = self.rows.get(&header.session_id) else {
                            continue;
                        };
                        let Some(entry) =
                            self.all.iter_mut().find(|entry| entry.ordinal == ordinal)
                        else {
                            continue;
                        };
                        entry.apply_header(&header);
                        // The agent listed its own sessions: what the index says of them is
                        // detail, never a reason to drop one.
                        if matches!(entry.action, ResumeAction::Acp { .. }) {
                            continue;
                        }
                        // A session in which nothing was asked has nothing to resume, and a
                        // delegated child is never a conversation of its own.
                        entry.hidden = header.user_turns == 0 || header.delegated;
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
                Message::Rows {
                    all_chats,
                    sessions,
                } => {
                    // An answer to a scope that was switched away from a moment ago is stale.
                    if self
                        .switch
                        .as_ref()
                        .is_some_and(|switch| switch.all_chats == all_chats)
                    {
                        self.replace_rows(&sessions);
                    }
                }
            }
        }
        if changed {
            self.state.set_items(arrange(&self.all));
        }
    }

    /// Show `sessions` in the scope the picker is in: the agent's own sessions join them in this
    /// project's scope, and the selection goes to the first row.
    fn replace_rows(&mut self, sessions: &[SessionPickerInfo]) {
        let (Some(switch), Some(nori_home)) = (&self.switch, &self.nori_home) else {
            return;
        };
        let agent_sessions: &[SessionInfo] = if switch.all_chats {
            &[]
        } else {
            &switch.agent_sessions
        };
        let entries = build_entries(
            sessions,
            agent_sessions,
            nori_home,
            switch.purpose,
            switch.current.as_deref(),
            switch.offer_new,
        );
        self.rows = rows_of(&entries);
        self.all = entries;
        self.state.set_items(arrange(&self.all));
        if let Some(first) = self.state.items().first().map(|entry| entry.ordinal) {
            self.state.select(first);
        }
    }

    /// Switch between this project's sessions of the active agent and all chats. The rows load
    /// in the background; the picker keeps working meanwhile.
    fn toggle_scope(&mut self, tui: &tui::Tui) {
        let (Some(switch), Some(nori_home)) = (&mut self.switch, &self.nori_home) else {
            return;
        };
        switch.all_chats = !switch.all_chats;
        let all_chats = switch.all_chats;
        let (cwd, agent) = if all_chats {
            (None, None)
        } else {
            (
                Some(switch.widen.cwd.clone()),
                Some(switch.widen.agent.clone()),
            )
        };
        let (subtitle, empty) = switch_text(&switch.scope, all_chats);
        let nori_home = nori_home.clone();
        self.state.set_subtitle(subtitle);
        self.state.set_empty_text(empty);
        let tx = self.channel.0.clone();
        let frames = tui.frame_requester();
        tokio::spawn(async move {
            match load::load(&nori_home, cwd.as_deref(), agent.as_deref()).await {
                Ok(listing) => {
                    let _ = tx.send(Message::Rows {
                        all_chats,
                        sessions: listing.sessions,
                    });
                    frames.schedule_frame();
                }
                Err(error) => {
                    tracing::warn!(%error, "could not load the sessions of the scope switched to");
                }
            }
        });
        tui.frame_requester().schedule_frame();
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
                // The box takes the rows its list needs, not a whole panel.
                let rows = self.state.height_wanted(tui.terminal.size()?.width);
                tui.draw_overlay_rows(i32::from(rows), |frame| {
                    self.area = frame.area();
                    self.state.render(self.area, frame.buffer, &theme, 0);
                })?;
            }
            TuiEvent::Key(key)
                if self.switch.is_some()
                    && key.kind == KeyEventKind::Press
                    && key.code == KeyCode::Char('a')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.toggle_scope(tui);
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

/// The rows of a picker: Nori's records and the agent's own sessions, most recently active first,
/// with the start row first when `offer_new`. Each row's ordinal is its position.
fn build_entries(
    sessions: &[SessionPickerInfo],
    agent_sessions: &[SessionInfo],
    nori_home: &Path,
    purpose: SessionPurpose,
    current: Option<&str>,
    offer_new: bool,
) -> Vec<ResumeEntry> {
    let mut rows: Vec<(i64, ResumeEntry)> = Vec::new();
    for info in sessions {
        let is_current = current == Some(info.session_id.as_str());
        let entry = ResumeEntry::from_local(info, nori_home.to_path_buf(), 0, purpose, is_current);
        rows.push((active_ms(&info.last_active_at), entry));
    }
    for session in agent_sessions {
        let entry = ResumeEntry::from_acp(session, 0);
        rows.push((active_ms(&entry.active_at), entry));
    }
    // Most recent activity first; a stable sort keeps the records' order among equals.
    rows.sort_by_key(|(active, _)| std::cmp::Reverse(*active));
    let mut entries = Vec::with_capacity(rows.len() + 1);
    if offer_new {
        entries.push(ResumeEntry::start_new());
    }
    entries.extend(rows.into_iter().map(|(_, entry)| entry));
    for (position, entry) in entries.iter_mut().enumerate() {
        entry.ordinal = i64::try_from(position).unwrap_or(0);
    }
    entries
}

/// Session id to the row it is shown in.
fn rows_of(entries: &[ResumeEntry]) -> HashMap<String, i64> {
    entries
        .iter()
        .map(|entry| (entry.session_id.clone(), entry.ordinal))
        .collect()
}

/// The bottom edge and the empty text of a picker that switches between this project and all
/// chats: the narrow scope names the key that widens it, the wide one the key that narrows it.
fn switch_text(scope: &str, all_chats: bool) -> (Option<String>, String) {
    if all_chats {
        (
            Some("all chats · ^A this project".to_owned()),
            empty_text("all chats"),
        )
    } else {
        (Some(format!("{scope} · ^A all chats")), empty_text(scope))
    }
}

/// The entries as the list shows them: in their order, without the ones that are not listed.
fn arrange(all: &[ResumeEntry]) -> Vec<ResumeEntry> {
    all.iter().filter(|entry| !entry.hidden).cloned().collect()
}

/// When a session was last active, in epoch milliseconds (0 when unknown, which lists last).
fn active_ms(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso).map_or(0, |time| time.timestamp_millis())
}

/// What the list holds, on the frame's bottom edge under the list: short enough to fit there.
/// Rows are always newest first, which the ages on the right show.
fn scope_line(scope: &str) -> Option<String> {
    (!scope.is_empty()).then(|| scope.to_owned())
}

fn empty_text(scope: &str) -> String {
    if scope.is_empty() {
        "No sessions recorded yet.".to_owned()
    } else {
        format!("No sessions recorded yet for {scope}.")
    }
}

#[cfg(test)]
mod tests {
    use nori_tui_components::Theme;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;

    use super::*;

    fn info(session_id: &str, last_active_at: &str) -> SessionPickerInfo {
        SessionPickerInfo {
            session_id: session_id.to_owned(),
            project_id: "project".to_owned(),
            started_at: last_active_at.to_owned(),
            last_active_at: last_active_at.to_owned(),
            agent: Some("claude-code".to_owned()),
            cwd: Some("/work/app".to_owned()),
            model: None,
            branch: None,
            title: Some(format!("Title {session_id}")),
            user_turn_count: Some(1),
            moved_to: None,
        }
    }

    /// The `/resume` picker's setup: the narrow scope is this project's sessions of `claude-code`.
    fn switching_setup(sessions: Vec<SessionPickerInfo>) -> ResumeSetup {
        ResumeSetup {
            purpose: SessionPurpose::Resume,
            nori_home: PathBuf::from("/nori"),
            sessions,
            agent_sessions: Vec::new(),
            current: None,
            offer_new: false,
            cancel_candidate_on_dismiss: false,
            scope: "claude-code · this project".to_owned(),
            widen: Some(Widen {
                cwd: PathBuf::from("/work/app"),
                agent: "claude-code".to_owned(),
            }),
        }
    }

    fn listed_ids(overlay: &ResumeOverlay) -> Vec<String> {
        overlay
            .state
            .items()
            .iter()
            .map(|entry| entry.session_id.clone())
            .collect()
    }

    /// The picker as it draws, flattened row by row.
    fn drawn(overlay: &mut ResumeOverlay) -> String {
        let area = Rect::new(0, 0, 200, 12);
        let mut buf = Buffer::empty(area);
        overlay.state.render(area, &mut buf, &Theme::default(), 0);
        buf.content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn switching_picker_opens_with_the_narrow_scope_text() {
        let mut overlay =
            ResumeOverlay::new(switching_setup(vec![info("a", "2026-11-18T10:00:00Z")]));
        assert!(drawn(&mut overlay).contains("claude-code · this project · ^A all chats"));

        let mut empty = ResumeOverlay::new(switching_setup(Vec::new()));
        assert!(
            drawn(&mut empty).contains("No sessions recorded yet for claude-code · this project.")
        );
    }

    #[test]
    fn a_short_list_draws_a_box_only_as_tall_as_its_rows() {
        let mut overlay = ResumeOverlay::new(switching_setup(vec![
            info("a", "2026-11-18T10:00:00Z"),
            info("b", "2026-11-18T12:00:00Z"),
        ]));
        // Two sessions of two lines each, the prompt, the rule under it and the two borders.
        let height = overlay.state.height_wanted(36);
        assert_eq!(height, 8);

        let area = Rect::new(0, 0, 36, height);
        let mut buf = Buffer::empty(area);
        overlay.state.render(area, &mut buf, &Theme::default(), 0);
        assert_eq!(
            (buf[(0, 0)].symbol(), buf[(0, height - 1)].symbol()),
            ("╭", "╰")
        );
    }

    #[test]
    fn switch_text_names_the_key_that_switches_back() {
        assert_eq!(
            (
                switch_text("claude-code · this project", false),
                switch_text("claude-code · this project", true),
            ),
            (
                (
                    Some("claude-code · this project · ^A all chats".to_owned()),
                    "No sessions recorded yet for claude-code · this project.".to_owned(),
                ),
                (
                    Some("all chats · ^A this project".to_owned()),
                    "No sessions recorded yet for all chats.".to_owned(),
                ),
            )
        );
    }

    #[test]
    fn rows_for_the_scope_switched_to_replace_the_list() {
        let mut overlay =
            ResumeOverlay::new(switching_setup(vec![info("a", "2026-11-18T10:00:00Z")]));
        overlay
            .switch
            .as_mut()
            .expect("a switching picker")
            .all_chats = true;

        overlay
            .channel
            .0
            .send(Message::Rows {
                all_chats: true,
                sessions: vec![
                    info("b", "2026-11-18T12:00:00Z"),
                    info("c", "2026-11-19T09:00:00Z"),
                ],
            })
            .expect("the picker is listening");
        overlay.drain();

        assert_eq!(listed_ids(&overlay), vec!["c".to_owned(), "b".to_owned()]);
        assert_eq!(
            overlay
                .state
                .selected()
                .map(|entry| entry.session_id.clone()),
            Some("c".to_owned())
        );
    }

    #[test]
    fn rows_for_the_other_scope_are_ignored() {
        let mut overlay =
            ResumeOverlay::new(switching_setup(vec![info("a", "2026-11-18T10:00:00Z")]));
        // The picker is in the narrow scope; a late answer for all chats must not replace it.
        overlay
            .channel
            .0
            .send(Message::Rows {
                all_chats: true,
                sessions: vec![info("b", "2026-11-18T12:00:00Z")],
            })
            .expect("the picker is listening");
        overlay.drain();

        assert_eq!(listed_ids(&overlay), vec!["a".to_owned()]);
    }
}
