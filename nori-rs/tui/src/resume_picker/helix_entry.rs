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
use crate::subagent_switcher::Target;

#[derive(Debug, Clone)]
pub(crate) enum ResumeAction {
    StartFresh,
    Local {
        nori_home: PathBuf,
        project_id: String,
        session_id: String,
    },
    Acp {
        acp_session_id: String,
        title: Option<String>,
    },
    /// Show the transcript read-only in the history.
    ViewOnly {
        nori_home: PathBuf,
        project_id: String,
        session_id: String,
    },
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
    ViewOnly,
    Reference { target: Target },
}

impl SessionPurpose {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Resume => "Sessions",
            Self::ViewOnly => "View a session",
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
    pub(crate) title: String,
    pub(crate) started_at: String,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) user_turns: Option<i64>,
    /// Bumped whenever the digest header refreshed this entry, so its preview is rebuilt.
    pub(crate) revision: i64,
    /// A delegated child of another session in the list.
    pub(crate) delegated: bool,
    /// The row it is shown under, once known.
    pub(crate) parent: Option<i64>,
    /// Not listed: nothing was asked in it, or it is a child whose parent is not known.
    pub(crate) hidden: bool,
    /// Tree guides drawn before the dot of a child row.
    pub(crate) guides: String,
    pub(crate) action: ResumeAction,
}

impl ResumeEntry {
    pub(crate) fn from_local(
        info: &SessionPickerInfo,
        agent: Option<String>,
        nori_home: PathBuf,
        ordinal: i64,
        purpose: SessionPurpose,
    ) -> Self {
        let fallback = format!("Session {}", compact_age(&info.started_at));
        let action = match purpose {
            SessionPurpose::Resume => ResumeAction::Local {
                nori_home,
                project_id: info.project_id.clone(),
                session_id: info.session_id.clone(),
            },
            SessionPurpose::ViewOnly => ResumeAction::ViewOnly {
                nori_home,
                project_id: info.project_id.clone(),
                session_id: info.session_id.clone(),
            },
            SessionPurpose::Reference { target } => ResumeAction::Reference {
                project_id: info.project_id.clone(),
                session_id: info.session_id.clone(),
                agent: agent.clone().unwrap_or_else(|| "Unknown agent".to_owned()),
                target,
            },
        };
        Self {
            ordinal,
            session_id: info.session_id.clone(),
            project_id: Some(info.project_id.clone()),
            title: info
                .first_message_preview
                .clone()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or(fallback),
            started_at: info.started_at.clone(),
            agent,
            model: None,
            branch: None,
            cwd: None,
            user_turns: info
                .user_turn_count
                .and_then(|turns| i64::try_from(turns).ok()),
            revision: 0,
            delegated: info.delegated,
            parent: None,
            hidden: info.delegated,
            guides: String::new(),
            action,
        }
    }

    pub(crate) fn from_acp(session: &SessionInfo, ordinal: i64) -> Self {
        let id = session.session_id.to_string();
        let title = session
            .title
            .clone()
            .filter(|title| !title.trim().is_empty());
        Self {
            ordinal,
            session_id: id.clone(),
            project_id: None,
            title: title.clone().unwrap_or_else(|| id.clone()),
            started_at: session.updated_at.clone().unwrap_or_default(),
            agent: None,
            model: None,
            branch: None,
            cwd: Some(session.cwd.display().to_string()).filter(|cwd| !cwd.is_empty()),
            user_turns: None,
            revision: 0,
            delegated: false,
            parent: None,
            hidden: false,
            guides: String::new(),
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
            title: "Start a new session".to_owned(),
            started_at: String::new(),
            agent: None,
            model: None,
            branch: None,
            cwd: None,
            user_turns: None,
            revision: 0,
            delegated: false,
            parent: None,
            hidden: false,
            guides: String::new(),
            action: ResumeAction::StartFresh,
        }
    }

    /// Fold in what the index knows about this session.
    pub(crate) fn apply_header(&mut self, header: &DigestHeader) {
        if let Some(title) = header.title.as_ref().filter(|title| !title.is_empty()) {
            self.title.clone_from(title);
        }
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
        2
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
        let age = compact_age(&self.started_at);
        let age_width = UnicodeWidthStr::width(age.as_str());
        let dot_style = self
            .agent
            .as_deref()
            .map_or(cx.theme.accent_mint, |agent| agent_style(cx.theme, agent));
        let guides_width = if cx.filtered {
            0
        } else {
            self.guides.chars().count()
        };
        let title_room =
            width.saturating_sub(2 + guides_width + age_width + usize::from(age_width > 0));
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
        first.insert(0, Span::styled("● ", dot_style));
        if !cx.filtered && !self.guides.is_empty() {
            first.insert(0, Span::styled(self.guides.clone(), cx.theme.faint));
        }
        first.push(Span::raw(" ".repeat(title_room.saturating_sub(used) + 1)));
        first.push(Span::styled(age, cx.theme.faint));

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
            None => clip(&self.facts(), second_room),
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
    /// `branch · 14 turns · claude-code`, whatever of it is known.
    fn facts(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(self.branch.clone());
        parts.extend(self.user_turns.map(|turns| match turns {
            1 => "1 turn".to_owned(),
            turns => format!("{turns} turns"),
        }));
        parts.extend(self.agent.clone());
        if parts.is_empty() {
            parts.extend(self.cwd.clone());
        }
        parts.join(" · ")
    }
}
