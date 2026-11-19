//! Nori's own tools, drawn as what they did: a delegated task, a commit, the goal.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::ClientToolCell;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Header;
use crate::history_cell::chrome::Progress;
use crate::history_cell::chrome::Target;
use crate::presentation::Artifact;
use crate::presentation::DelegationCall;
use crate::presentation::NativeCall;
use crate::presentation::NativeTone as Tone;
use crate::presentation::ToolPhase;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

/// A delegated-task call: what it did and how it came out, on one line. The child's own work is
/// a subagent, shown as one, not under the call.
pub(super) fn delegation(
    cell: &ClientToolCell,
    chrome: &Chrome,
    width: usize,
    call: &DelegationCall,
) -> Vec<Line<'static>> {
    let active = cell.is_active();
    let failed = cell.snapshot.phase == ToolPhase::Failed;
    let glyph = if active {
        chrome.glyph(Progress::Running, cell.start_time, cell.animations_enabled)
    } else if failed {
        Span::styled("⇢", chrome.coral().add_modifier(Modifier::BOLD))
    } else {
        Span::styled("⇢", chrome.cyan().add_modifier(Modifier::BOLD))
    };
    let outcome = cell
        .snapshot
        .artifacts
        .iter()
        .find_map(|artifact| match artifact {
            Artifact::Text { text, .. } => crate::presentation::delegation_outcome(text),
            Artifact::Diff(_) => None,
        });
    vec![
        chrome.header(
            Header {
                glyph,
                verb: String::new(),
                target: Target::Text(call.header(active)),
                chip: Vec::new(),
                meta: outcome
                    .map(|outcome| chrome.fact(outcome))
                    .into_iter()
                    .collect(),
            },
            width,
        ),
    ]
}

/// One of Nori's own tools: a header with a glyph for what it is and a tone on every piece of
/// text, its details hung off a small tree. Long lines wrap under their branch.
pub(super) fn native(
    cell: &ClientToolCell,
    chrome: &Chrome,
    width: usize,
    call: &NativeCall,
) -> Vec<Line<'static>> {
    let active = cell.is_active();
    let failed = cell.snapshot.phase == ToolPhase::Failed;
    let result = cell
        .snapshot
        .artifacts
        .iter()
        .find_map(|artifact| match artifact {
            Artifact::Text { text, .. } => Some(text.as_str()),
            Artifact::Diff(_) => None,
        });
    let view = crate::presentation::native_view(call, active, failed, result);
    let style = |tone: Tone| -> Style {
        match tone {
            Tone::Plain => chrome.secondary(),
            Tone::Strong => chrome.strong(),
            Tone::Accent => chrome.cyan().add_modifier(Modifier::BOLD),
            Tone::Dim => chrome.muted(),
            Tone::Added => chrome.mint(),
            Tone::Modified => chrome.cyan(),
            Tone::Deleted | Tone::Error => chrome.coral(),
        }
    };
    // The glyph says what kind of call this is; its colour says how it went.
    let glyph = if active {
        chrome.glyph(Progress::Running, cell.start_time, cell.animations_enabled)
    } else if failed {
        Span::styled(view.glyph, chrome.coral().add_modifier(Modifier::BOLD))
    } else if view.glyph == "◆" {
        Span::styled(view.glyph, chrome.mint().add_modifier(Modifier::BOLD))
    } else {
        Span::styled(view.glyph, chrome.cyan().add_modifier(Modifier::BOLD))
    };
    let mut header = vec![glyph, Span::raw(" ")];
    header.extend(
        view.header
            .into_iter()
            .map(|part| Span::styled(part.text, style(part.tone))),
    );
    let width = width.max(20);
    let mut lines = word_wrap_lines(
        [Line::from(header)],
        RtOptions::new(width).subsequent_indent("  ".into()),
    );
    let count = view.detail.len();
    for (index, detail) in view.detail.into_iter().enumerate() {
        let (connector, continuation) = if index + 1 == count {
            ("  └ ", "    ")
        } else {
            ("  ├ ", "  │ ")
        };
        let mut spans = Vec::new();
        if let Some((marker, tone)) = detail.marker {
            spans.push(Span::styled(marker.to_string(), style(tone)));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(detail.text, style(detail.tone)));
        spans.extend(
            detail
                .suffix
                .into_iter()
                .map(|part| Span::styled(part.text, style(part.tone))),
        );
        lines.extend(word_wrap_lines(
            [Line::from(spans)],
            RtOptions::new(width)
                .initial_indent(Span::styled(connector, chrome.faint()).into())
                .subsequent_indent(Span::styled(continuation, chrome.faint()).into()),
        ));
    }
    lines
}
