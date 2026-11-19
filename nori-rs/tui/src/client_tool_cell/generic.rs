//! The default card for a tool call nothing more specific draws: a fetch, a search, an MCP tool.
//!
//! One header (`● Fetch https://…  · first line of what came back`), and, expanded, the evidence
//! behind it.

use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

use super::ClientToolCell;
use super::evidence::append_evidence;
use super::evidence::clean;
use crate::client_event_format::relativize_paths_in_text;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Disclosure;
use crate::history_cell::chrome::Header;
use crate::history_cell::chrome::Target;
use crate::history_cell::chrome::format_duration;
use crate::history_cell::chrome::truncate_tail;
use crate::presentation::Artifact;
use crate::presentation::ToolPhase;
use crate::presentation::ToolSnapshot;

/// The longest a summary gets, in columns, before it is cut.
const SUMMARY_MAX: usize = 100;

pub(super) fn render(
    cell: &ClientToolCell,
    chrome: &Chrome,
    width: usize,
    disclosure: Disclosure,
) -> Vec<Line<'static>> {
    let snapshot = &cell.snapshot;
    let title = relativize_paths_in_text(&title_of(snapshot), &cell.cwd);
    let (verb, target) = match title.split_once(' ') {
        Some((verb, rest)) => (verb.to_owned(), rest.trim().to_owned()),
        None => (title, String::new()),
    };
    let failed = snapshot.phase == ToolPhase::Failed;
    let mut meta = Vec::new();
    if !failed && let Some(summary) = summary_of(snapshot) {
        meta.push(chrome.fact(truncate_tail(
            &summary,
            SUMMARY_MAX.min(width.saturating_sub(8)),
        )));
    }
    if let Some(took) = cell.elapsed.and_then(format_duration) {
        meta.push(chrome.fact(took));
    }
    let chip = if failed {
        let reason = summary_of(snapshot).unwrap_or_else(|| "failed".to_owned());
        vec![Span::styled(
            truncate_tail(&reason, SUMMARY_MAX.min(width.saturating_sub(8))),
            chrome.coral(),
        )]
    } else {
        Vec::new()
    };
    let mut lines = vec![chrome.header(
        Header {
            glyph: chrome.glyph(cell.progress(), cell.start_time, cell.animations_enabled),
            verb,
            target: Target::Text(target),
            chip,
            meta,
        },
        width,
    )];
    if disclosure == Disclosure::Expanded {
        append_evidence(
            chrome,
            &mut lines,
            snapshot,
            u16::try_from(width).unwrap_or(u16::MAX),
        );
    }
    lines
}

/// The first line of the call's title, with a finished "Running x" turned into "Ran x".
fn title_of(snapshot: &ToolSnapshot) -> String {
    let raw = if is_goal_lookup(snapshot) {
        "Goal status"
    } else {
        snapshot.title.as_str()
    }
    .lines()
    .next()
    .unwrap_or("Tool")
    .trim();
    let raw = if raw.is_empty() { "Tool" } else { raw };
    let title = if snapshot.phase == ToolPhase::Completed && raw.starts_with("Running ") {
        format!("Ran {}", &raw[8..])
    } else {
        raw.to_owned()
    };
    clean(&title)
}

/// One line saying what the call returned, when it returned something worth a line.
fn summary_of(snapshot: &ToolSnapshot) -> Option<String> {
    let summary = snapshot
        .raw_output
        .as_ref()
        .and_then(|value| match value.as_str() {
            Some(text) => {
                let text = clean(text);
                match serde_json::from_str::<Value>(&text) {
                    Ok(value) => structured_summary(&value),
                    Err(_) => Some(text),
                }
            }
            None => structured_summary(value),
        })
        .or_else(|| {
            if matches!(
                snapshot.phase,
                ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress
            ) {
                return None;
            }
            snapshot
                .artifacts
                .iter()
                .find_map(|artifact| match artifact {
                    Artifact::Text { text, .. } => {
                        let text = clean(text);
                        match serde_json::from_str::<Value>(&text) {
                            Ok(value) => structured_summary(&value),
                            Err(_) => text
                                .lines()
                                .find(|line| {
                                    !line.trim().is_empty() && !line.trim_start().starts_with("```")
                                })
                                .map(str::to_owned),
                        }
                    }
                    Artifact::Diff(_) => None,
                })
        })
        .or_else(|| {
            snapshot
                .locations
                .first()
                .map(|location| location.path.display().to_string())
        })?;
    if serde_json::from_str::<Value>(&summary).is_ok() || is_noise_exec_summary(&summary) {
        return None;
    }
    summary
        .lines()
        .find(|line| !line.trim().is_empty() && !is_noise_exec_summary(line))
        .map(|line| clean(line).trim().to_owned())
}

pub(super) fn is_noise_exec_summary(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.eq_ignore_ascii_case("exit 0")
        || trimmed.eq_ignore_ascii_case("exit code 0")
        || trimmed.eq_ignore_ascii_case("exit_code: 0")
        || trimmed == "<1s"
}

/// Recognize only a complete, canonical single-call wrapper; never infer from a substring.
pub(super) fn is_goal_lookup(snapshot: &ToolSnapshot) -> bool {
    if !snapshot.title.eq_ignore_ascii_case("execute") {
        return false;
    }
    let Some(code) = snapshot
        .raw_input
        .as_ref()
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let mut compact = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut whitespace = false;
    for character in code.chars() {
        if let Some(delimiter) = quote {
            compact.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
        } else if character.is_whitespace() {
            whitespace = true;
        } else {
            if whitespace
                && compact
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && (character.is_ascii_alphanumeric() || character == '_')
            {
                compact.push(' ');
            }
            whitespace = false;
            if matches!(character, '\"' | '\'') {
                quote = Some(character);
            }
            compact.push(character);
        }
    }
    let compact = compact.strip_suffix(';').unwrap_or(&compact);
    matches!(
        compact,
        "const r=await tools[\"nori-client\"].get_goal();return r"
            | "const r=await tools['nori-client'].get_goal();return r"
    )
}

pub(super) fn structured_summary(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("message").and_then(Value::as_str))
        })
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            value.get("goal").and_then(|goal| {
                if goal.is_null() {
                    Some("No active goal".to_owned())
                } else {
                    goal.get("objective")
                        .and_then(Value::as_str)
                        .map(|objective| {
                            goal.get("status").and_then(Value::as_str).map_or_else(
                                || objective.to_owned(),
                                |status| format!("{objective} · {status}"),
                            )
                        })
                }
            })
        })
        .or_else(|| {
            value
                .get("success")
                .and_then(Value::as_bool)
                .map(|ok| if ok { "Succeeded" } else { "Failed" }.to_owned())
        })
}
