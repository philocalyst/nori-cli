//! A rendered `/resume` picker over fixture sessions, for the storybook (feature `storybook`).

use std::path::PathBuf;
use std::time::Instant;

use nori_harness::session_index::DigestHeader;
use nori_harness::session_index::DigestTurn;
use nori_harness::session_index::SessionDigest;
use nori_harness::session_index::SourceStamp;
use nori_harness::session_index::TurnRole;
use nori_tui_components::Hit;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::helix_entry::ResumeAction;
use super::helix_entry::ResumeEntry;
use super::helix_overlay::ResumeOverlay;
use super::preview;
use crate::style::component_theme;

fn entry(
    ordinal: i64,
    title: &str,
    age_minutes: i64,
    agent: &str,
    model: &str,
    branch: &str,
    turns: i64,
) -> ResumeEntry {
    ResumeEntry {
        ordinal,
        session_id: format!("session-{ordinal}"),
        project_id: Some("project".to_owned()),
        local_session_id: Some(format!("session-{ordinal}")),
        title: title.to_owned(),
        agent_titled: false,
        active_at: (chrono::Utc::now() - chrono::Duration::minutes(age_minutes)).to_rfc3339(),
        agent: Some(agent.to_owned()),
        model: Some(model.to_owned()),
        branch: Some(branch.to_owned()),
        cwd: Some("/Users/dev/Projects/nori-cli".to_owned()),
        user_turns: Some(turns),
        moved_to: None,
        revision: 0,
        hidden: false,
        current: false,
        action: ResumeAction::StartFresh,
    }
}

fn digest() -> SessionDigest {
    let mut digest = digest_turns();
    digest.turns[3].output = Some(
        "test cancel_during_wake_retains_child_verdict ... ok\ntest result: ok. 18 passed"
            .to_owned(),
    );
    digest
}

fn digest_turns() -> SessionDigest {
    let turn = |role, text: &str, command: Option<&str>| DigestTurn {
        role,
        text: text.to_owned(),
        command: command.map(str::to_owned),
        output: None,
    };
    SessionDigest {
        header: DigestHeader {
            schema: 1,
            session_id: "session-1".to_owned(),
            project_id: "project".to_owned(),
            started_at: chrono::Utc::now().to_rfc3339(),
            cwd: PathBuf::from("/Users/dev/Projects/nori-cli"),
            agent: Some("codex".to_owned()),
            model: Some("gpt-6".to_owned()),
            branch: Some("instant-core".to_owned()),
            title: None,
            user_turns: 2,
            acp_session_ids: Vec::new(),
            delegated: false,
            source: SourceStamp {
                len: 0,
                mtime_ms: 0,
            },
            indexed_at: String::new(),
        },
        turns: vec![
            turn(
                TurnRole::User,
                "Fence WakeTurn delivery so a simultaneous cancel never drops a child verdict.",
                None,
            ),
            turn(
                TurnRole::Assistant,
                "I'll key the fence by (rev, attempt). A cancel that lands while a wake is queued must retain the child verdict, so the wake path peeks instead of popping.",
                None,
            ),
            turn(
                TurnRole::Tool,
                "Read wake.rs · nori-rs/harness/src/backend/orchestrator/wake.rs",
                None,
            ),
            turn(
                TurnRole::Tool,
                "Ran cargo test",
                Some("cargo test -p nori-harness --test delegated_tasks -- wake_cancel"),
            ),
            turn(
                TurnRole::Assistant,
                "Wake delivery is now losslessly fenced: peek_fenced(f) retains the verdict when cancel wins the race, and the test pins it.",
                None,
            ),
            turn(TurnRole::User, "Now wire the proptest into CI.", None),
        ],
    }
}

/// The picker at `width` x `height` with `query` typed, as plain text.
pub fn resume_specimen(width: u16, height: u16, query: &str) -> String {
    let entries = vec![
        entry(
            0,
            "Fence WakeTurn cancel delivery",
            12,
            "codex",
            "gpt-6",
            "instant-core",
            9,
        ),
        entry(
            1,
            "In-process host tools & fork IDs",
            180,
            "claude-code",
            "opus-5",
            "main",
            14,
        ),
        entry(
            2,
            "Primary scrollback SIGWINCH reflow",
            360,
            "gemini",
            "gemini-4",
            "perf/instant-core",
            6,
        ),
        entry(
            3,
            "OpenCode quota fallback & SSE",
            1500,
            "opencode",
            "deepseek-v4",
            "main",
            22,
        ),
        entry(
            4,
            "Native Rust Pkl 0.33 module capture",
            1700,
            "claude-code",
            "haiku-5",
            "pkl",
            5,
        ),
        entry(
            5,
            "Embed brush-core bash completions",
            2900,
            "codex",
            "gpt-6",
            "brush",
            31,
        ),
    ];
    let mut overlay = ResumeOverlay::specimen(entries);
    overlay.specimen_query(
        query,
        vec![(2, 3, "…the cancel path peeks the fence before it pops…")],
    );
    let area = Rect::new(0, 0, width, height);
    let theme = component_theme();
    overlay.specimen_preview(area, &digest(), &theme);
    let mut buffer = Buffer::empty(area);
    overlay.specimen_render(area, &mut buffer, &theme);
    let _ = Instant::now();
    (0..height)
        .map(|row| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl ResumeOverlay {
    pub(super) fn specimen(entries: Vec<ResumeEntry>) -> Self {
        Self::build(
            entries,
            None,
            false,
            "Sessions",
            None,
            "No sessions recorded yet.".to_owned(),
        )
    }

    pub(super) fn specimen_query(&mut self, query: &str, hits: Vec<(i64, usize, &str)>) {
        self.state.set_query(query);
        let Some(request) = self.state.take_search_request() else {
            return;
        };
        let generation = request.generation;
        self.state.set_hits(
            generation,
            hits.into_iter()
                .map(|(ordinal, count, snippet)| {
                    (
                        ordinal,
                        Hit {
                            score: 60,
                            count,
                            snippet: Some(snippet.to_owned()),
                        },
                    )
                })
                .collect(),
        );
    }

    pub(super) fn specimen_preview(
        &mut self,
        area: Rect,
        digest: &SessionDigest,
        theme: &nori_tui_components::Theme,
    ) {
        if let Some(request) = self.state.preview_request(area, Instant::now())
            && let Some(entry) = self.state.selected().cloned()
        {
            let built = preview::from_digest(&request, &entry, Some(digest), theme);
            self.state.set_preview(built);
        }
    }

    pub(super) fn specimen_render(
        &mut self,
        area: Rect,
        buffer: &mut Buffer,
        theme: &nori_tui_components::Theme,
    ) {
        self.state.render(area, buffer, theme, 0);
    }
}
