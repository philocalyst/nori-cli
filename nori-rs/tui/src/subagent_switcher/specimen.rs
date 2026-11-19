//! A rendered actor panel over a fixture tree, for the storybook (feature `storybook`).

use std::time::Duration;
use std::time::Instant;

use nori_config::HotkeyBinding;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::Body;
use super::Entry;
use super::Parent;
use super::Roster;
use super::Status;
use super::SubagentKey;
use super::SubagentOverlay;
use super::Target;
use crate::style::component_theme;

#[allow(clippy::too_many_arguments)]
fn actor(
    key: i64,
    title: &str,
    parent: Option<(i64, &str)>,
    agent: &str,
    model: &str,
    status: Status,
    seconds: u64,
    tools: usize,
    transcript: &str,
) -> Entry {
    Entry {
        target: Target::Subagent(SubagentKey(key)),
        title: title.to_owned(),
        parent: parent.map(|(id, label)| Parent {
            label: label.to_owned(),
            target: Some(Target::Subagent(SubagentKey(id))),
        }),
        agent: Some(agent.to_owned()),
        model: Some(model.to_owned()),
        status,
        detail: None,
        transcript_text: transcript.to_owned(),
        elapsed: Duration::from_secs(seconds),
        ended_ago: (!status.is_running()).then(|| Duration::from_secs(seconds * 7)),
        tool_count: tools,
        revision: key,
        active_at: Instant::now(),
    }
}

fn roster() -> Roster {
    Roster {
        main: Entry {
            target: Target::Main,
            title: "main".to_owned(),
            parent: None,
            agent: Some("claude-code".to_owned()),
            model: Some("opus-5".to_owned()),
            status: Status::Running,
            detail: None,
            transcript_text: String::new(),
            elapsed: Duration::ZERO,
            ended_ago: None,
            tool_count: 0,
            revision: 0,
            active_at: Instant::now(),
        },
        subagents: vec![
            actor(
                1,
                "luna-wake",
                None,
                "codex",
                "gpt-6-luna",
                Status::Running,
                8,
                4,
                "cargo test -p nori-harness -- fence 5 passed, running proptest_rebind",
            ),
            actor(
                2,
                "proptest-runner",
                Some((1, "luna-wake")),
                "codex",
                "gpt-6-luna",
                Status::Running,
                3,
                1,
                "256 cases proptest fence rebind",
            ),
            actor(
                3,
                "deepseek-audit",
                None,
                "opencode",
                "deepseek-v4",
                Status::Completed,
                6,
                3,
                "Key the fence by (rev, attempt), otherwise retry reuses the prior turn token.",
            ),
            actor(
                4,
                "haiku-conduit-check",
                None,
                "claude-code",
                "haiku-5-5",
                Status::Completed,
                4,
                2,
                "schema verified",
            ),
        ],
        replaced: Vec::new(),
        animations: false,
        tick: 0,
    }
}

/// The panel at `width` x `height` with `query` typed and `selected` highlighted, as plain
/// text.
pub fn actors_specimen(width: u16, height: u16, query: &str, selected: i64) -> String {
    let roster = roster();
    let mut overlay = SubagentOverlay::switcher(&roster, HotkeyBinding::from_str("ctrl+space"));
    let area = Rect::new(0, 0, width, height);
    overlay.specimen_type(query);
    overlay.specimen_select(Target::Subagent(SubagentKey(selected)));
    if let Some(query) = overlay.body_query(area, Instant::now()) {
        let lines: Vec<Line<'static>> = [
            "▌ Implement monotonic fence on WakeTurn & nested child rebind",
            "",
            "✉ deepseek-audit → luna-wake: Key the fence by (rev, attempt).",
            "",
            "± child/phase.rs +9 −2",
            "  41 − AwaitingWake(TaskId)",
            "  41 + AwaitingFencedWake { task: TaskId, fence: LeaseFence }",
            "",
            "$ cargo test -p nori-harness -- fence_rebind",
            "  5 passed · 1 running",
            "✦ Checking 3-deep grandchild compaction…",
        ]
        .into_iter()
        .map(|line| Line::from(line.to_owned()))
        .collect();
        overlay.set_body(
            &roster,
            Body {
                query,
                revision: selected,
                lines,
            },
            area,
        );
    }
    let mut buffer = Buffer::empty(area);
    overlay.specimen_render(area, &mut buffer);
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

impl SubagentOverlay {
    pub(super) fn specimen_render(&mut self, area: Rect, buffer: &mut Buffer) {
        self.state
            .render(area, buffer, &component_theme(), self.tick);
    }

    pub(super) fn specimen_type(&mut self, query: &str) {
        self.state.set_query(query);
    }

    pub(super) fn specimen_select(&mut self, target: Target) {
        self.state.refresh();
        self.state.select(target);
    }
}
