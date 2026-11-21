//! The rows of the `/resume` picker: what a session is, how it is searched, how it is drawn.

use std::path::PathBuf;

use nori_harness::session_index::DigestHeader;
use nori_protocol::acp::v1::SessionInfo;
use nori_tui_components::Field;
use nori_tui_components::HelixItem;
use nori_tui_components::RowContext;
use nori_tui_components::fit_snippet;
use nori_tui_components::highlight_indices;
use nori_tui_components::highlight_terms;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::helix_picker::agent_style;
use crate::helix_picker::clip;
use crate::helix_picker::compact_age;
use crate::resume_picker::SessionPickerInfo;
use crate::resume_picker::place::clip_left;
use crate::resume_picker::place::is_here;
use crate::resume_picker::place::shorten_home;
use crate::subagent_switcher::Target;

#[derive(Debug, Clone)]
pub(crate) enum ResumeAction {
    /// Start a new session instead (offered where there is no other way to).
    StartFresh,
    /// Resume a session Nori recorded. `agent` is the agent (and account) that recorded it.
    Local {
        nori_home: PathBuf,
        project_id: String,
        session_id: String,
        agent: Option<String>,
    },
    /// Resume a session only the agent knows.
    Acp {
        acp_session_id: String,
        title: Option<String>,
    },
    /// The session already open: choosing it leaves things as they are.
    Current,
    /// Insert an excerpt of the conversation into a draft.
    Reference {
        project_id: String,
        session_id: String,
        agent: String,
        target: Target,
    },
}

/// What choosing a session does, which decides the picker's title and the row's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPurpose {
    Resume,
    Reference { target: Target },
}

impl SessionPurpose {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Resume => "Sessions",
            Self::Reference { .. } => "Reference a session",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ResumeOutcome {
    Pending,
    Selected(ResumeAction),
    Cancelled { cancel_candidate: bool },
}

/// One session in the picker.
#[derive(Debug, Clone)]
pub(crate) struct ResumeEntry {
    /// Position in the list as opened; the engine's identity for the row.
    pub(crate) ordinal: i64,
    pub(crate) session_id: String,
    pub(crate) project_id: Option<String>,
    /// The id of Nori's own record of this session, when there is one, which names its digest.
    pub(crate) local_session_id: Option<String>,
    pub(crate) title: String,
    /// The title is what the agent called the session, which the index does not replace.
    pub(crate) agent_titled: bool,
    /// When anything last happened in the session (ISO 8601).
    pub(crate) active_at: String,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) user_turns: Option<i64>,
    /// The machine the conversation moved to, when it left this home.
    pub(crate) moved_to: Option<String>,
    /// Bumped whenever the digest header refreshed this entry, so its preview is rebuilt.
    pub(crate) revision: i64,
    /// Not listed: nothing was asked in it, or it is a delegated child of another session.
    pub(crate) hidden: bool,
    /// The session this picker was opened from.
    pub(crate) current: bool,
    pub(crate) action: ResumeAction,
}

impl ResumeEntry {
    pub(crate) fn from_local(
        info: &SessionPickerInfo,
        nori_home: PathBuf,
        ordinal: i64,
        purpose: SessionPurpose,
        current: bool,
    ) -> Self {
        let fallback = format!("Session {}", compact_age(&info.started_at));
        let action = match purpose {
            SessionPurpose::Resume if current => ResumeAction::Current,
            SessionPurpose::Resume => ResumeAction::Local {
                nori_home,
                project_id: info.project_id.clone(),
                session_id: info.session_id.clone(),
                agent: info.agent.clone(),
            },
            SessionPurpose::Reference { target } => ResumeAction::Reference {
                project_id: info.project_id.clone(),
                session_id: info.session_id.clone(),
                agent: info
                    .agent
                    .clone()
                    .unwrap_or_else(|| "Unknown agent".to_owned()),
                target,
            },
        };
        Self {
            ordinal,
            session_id: info.session_id.clone(),
            project_id: Some(info.project_id.clone()),
            local_session_id: Some(info.session_id.clone()),
            agent_titled: false,
            title: info
                .title
                .as_deref()
                .map(nori_harness::injected_context::user_words)
                .filter(|words| !words.is_empty())
                .unwrap_or(fallback),
            active_at: info.last_active_at.clone(),
            agent: info.agent.clone(),
            model: info.model.clone(),
            branch: info.branch.clone(),
            cwd: info.cwd.clone(),
            user_turns: info.user_turn_count,
            moved_to: info.moved_to.clone(),
            revision: 0,
            hidden: false,
            current,
            action,
        }
    }

    pub(crate) fn from_acp(session: &SessionInfo, ordinal: i64) -> Self {
        let id = session.session_id.to_string();
        // An agent titles a session from the message it received, which carries whatever Nori
        // put in front of it: only the person's own words name the session.
        let title = session
            .title
            .as_deref()
            .map(nori_harness::injected_context::user_words)
            .filter(|words| !words.is_empty());
        Self {
            ordinal,
            session_id: id.clone(),
            project_id: None,
            local_session_id: None,
            agent_titled: title.is_some(),
            title: title
                .clone()
                .unwrap_or_else(|| "Untitled session".to_owned()),
            active_at: session.updated_at.clone().unwrap_or_default(),
            agent: None,
            model: None,
            branch: None,
            cwd: Some(session.cwd.display().to_string()).filter(|cwd| !cwd.is_empty()),
            user_turns: None,
            moved_to: None,
            revision: 0,
            hidden: false,
            current: false,
            action: ResumeAction::Acp {
                acp_session_id: id,
                title,
            },
        }
    }

    pub(crate) fn start_new() -> Self {
        Self {
            ordinal: 0,
            session_id: "__new__".to_owned(),
            project_id: None,
            local_session_id: None,
            agent_titled: false,
            title: "Start a new session".to_owned(),
            active_at: String::new(),
            agent: None,
            model: None,
            branch: None,
            cwd: None,
            user_turns: None,
            moved_to: None,
            revision: 0,
            hidden: false,
            current: false,
            action: ResumeAction::StartFresh,
        }
    }

    /// Fold in what the index knows about this session.
    pub(crate) fn apply_header(&mut self, header: &DigestHeader) {
        if !self.agent_titled
            && let Some(title) = header.title.as_ref().filter(|title| !title.is_empty())
        {
            self.title.clone_from(title);
        }
        self.project_id = Some(header.project_id.clone());
        self.local_session_id = Some(header.session_id.clone());
        self.agent.clone_from(&header.agent);
        self.model.clone_from(&header.model);
        self.branch.clone_from(&header.branch);
        self.cwd = Some(header.cwd.display().to_string());
        self.user_turns = Some(header.user_turns);
        self.revision = header.source.mtime_ms;
    }
}

impl HelixItem for ResumeEntry {
    type Id = i64;

    fn id(&self) -> i64 {
        self.ordinal
    }

    fn fields<'a>(&'a self, out: &mut Vec<Field<'a>>) {
        out.push(Field::new("title", &self.title, 0));
        if let Some(agent) = &self.agent {
            out.push(Field::new("agent", agent, 30));
        }
        if let Some(branch) = &self.branch {
            out.push(Field::new("branch", branch, 35));
        }
        if let Some(model) = &self.model {
            out.push(Field::new("model", model, 40));
        }
        if let Some(cwd) = &self.cwd {
            out.push(Field::new("cwd", cwd, 50));
        }
        out.push(Field::new("id", &self.session_id, 70));
    }

    fn row_height(&self) -> u16 {
        if self.is_new() { 1 } else { 2 }
    }

    fn revision(&self) -> i64 {
        self.revision
    }

    fn search_identity_eq(&self, other: &Self) -> bool {
        self.title == other.title
            && self.agent == other.agent
            && self.model == other.model
            && self.branch == other.branch
            && self.cwd == other.cwd
    }

    fn rows(&self, cx: &RowContext<'_>) -> Vec<Line<'static>> {
        let width = usize::from(cx.width);
        let age = if self.current {
            "current".to_owned()
        } else {
            compact_age(&self.active_at)
        };
        let age_width = UnicodeWidthStr::width(age.as_str());
        let dot_style = self
            .agent
            .as_deref()
            .map_or(cx.theme.accent_mint, |agent| agent_style(cx.theme, agent));
        let title_room = width.saturating_sub(2 + age_width + usize::from(age_width > 0));
        let base = if cx.selected {
            cx.theme.text_strong
        } else {
            Style::new()
        };
        let (mut first, used) = highlight_indices(
            &self.title,
            cx.highlights.field("title"),
            base,
            cx.theme.accent_amber,
            title_room,
        );
        first.insert(
            0,
            Span::styled(if self.current { "◆ " } else { "● " }, dot_style),
        );
        first.push(Span::raw(" ".repeat(title_room.saturating_sub(used) + 1)));
        let age_style = if self.current {
            cx.theme.accent_mint
        } else {
            cx.theme.faint
        };
        first.push(Span::styled(age, age_style));
        if self.is_new() {
            return vec![Line::from(first)];
        }

        let second_room = width.saturating_sub(2);
        let detail = match cx.hit {
            Some(hit) => {
                let count = if hit.count == 1 {
                    "1 hit".to_owned()
                } else {
                    format!("{} hits", hit.count)
                };
                match &hit.snippet {
                    Some(snippet) => {
                        let room = second_room.saturating_sub(count.chars().count() + 3);
                        format!("{count} · {}", fit_snippet(snippet, cx.terms, room))
                    }
                    None => count,
                }
            }
            None => self.details(second_room),
        };
        let second = Line::from(vec!["  ".into(), Span::styled(detail, cx.theme.faint)]);
        let second = if cx.hit.is_some() {
            highlight_terms(second, cx.terms, cx.theme.accent_amber)
        } else {
            second
        };
        vec![Line::from(first), second]
    }
}

impl ResumeEntry {
    /// The "start a new session" row: one line, nothing to say about it.
    fn is_new(&self) -> bool {
        matches!(self.action, ResumeAction::StartFresh)
    }

    /// What tells this session from its neighbours, in `room` columns: the directory when it is
    /// not the one Nori runs in (its end kept, as the end is what differs), then the branch,
    /// the turns and the agent, whatever of it is known.
    fn details(&self, room: usize) -> String {
        let mut facts: Vec<String> = Vec::new();
        facts.extend(
            self.moved_to
                .as_ref()
                .map(|machine| format!("moved to {machine}")),
        );
        facts.extend(self.branch.clone());
        facts.extend(self.user_turns.map(|turns| match turns {
            1 => "1 turn".to_owned(),
            turns => format!("{turns} turns"),
        }));
        facts.extend(self.agent.clone());
        let facts = facts.join(" · ");
        let place = self
            .cwd
            .as_deref()
            .filter(|cwd| !is_here(cwd))
            .map(shorten_home);
        match (place, facts.is_empty()) {
            (Some(place), true) => clip_left(&place, room),
            (Some(place), false) => {
                // The directory keeps what it needs; the facts take what is left of the line.
                let facts_width = UnicodeWidthStr::width(facts.as_str()).min(room / 2);
                let place = clip_left(&place, room.saturating_sub(facts_width + 3));
                let facts_room = room.saturating_sub(UnicodeWidthStr::width(place.as_str()) + 3);
                format!("{place} · {}", clip(&facts, facts_room))
            }
            (None, false) => clip(&facts, room),
            (None, true) => clip(&self.session_id, room),
        }
    }
}
