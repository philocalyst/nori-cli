//! An agent's shell command: the shared [`ShellBlock`] filled from a tool call's input and
//! output.

use ratatui::text::Line;
use serde_json::Value;

use super::ClientToolCell;
use super::evidence::clean;
use super::generic::is_noise_exec_summary;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Disclosure;
use crate::history_cell::shell_block::ShellBlock;
use crate::presentation::Artifact;
use crate::presentation::Invocation;
use crate::presentation::ToolPhase;
use crate::presentation::ToolSnapshot;

pub(super) fn render(
    cell: &ClientToolCell,
    chrome: &Chrome,
    width: usize,
    disclosure: Disclosure,
) -> Vec<Line<'static>> {
    let snapshot = &cell.snapshot;
    let command = command_of(snapshot);
    let output = output_of(snapshot);
    ShellBlock {
        command: &command,
        progress: cell.progress(),
        since: cell.start_time,
        animate: cell.animations_enabled,
        elapsed: cell.elapsed,
        exit_code: output.exit_code,
        cwd: None,
        stdout: &output.stdout,
        stderr: &output.stderr,
    }
    .render(chrome, width, disclosure)
}

/// The command a call ran: its invocation, its input, or what its title says it ran.
fn command_of(snapshot: &ToolSnapshot) -> String {
    let command = match &snapshot.invocation {
        Some(Invocation::Command { command }) => Some(command.as_str()),
        _ => snapshot
            .raw_input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str),
    };
    let from_title = || {
        let title = snapshot.title.lines().next().unwrap_or_default().trim();
        title
            .strip_prefix("Running ")
            .or_else(|| title.strip_prefix("Ran "))
            .unwrap_or(title)
            .to_owned()
    };
    clean(&command.map_or_else(from_title, str::to_owned))
}

/// What a command printed and how it exited, as far as the call says.
#[derive(Default)]
struct Output {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

fn output_of(snapshot: &ToolSnapshot) -> Output {
    let active = matches!(
        snapshot.phase,
        ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress
    );
    let mut output = match snapshot.raw_output.as_ref() {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(value @ Value::Object(_)) => from_object(&value),
            Ok(_) | Err(_) => Output {
                stdout: text.clone(),
                ..Output::default()
            },
        },
        Some(value @ Value::Object(_)) => from_object(value),
        Some(_) | None => Output::default(),
    };
    // While it runs, a text artifact is the tool's description, not its output.
    if output.stdout.is_empty()
        && output.stderr.is_empty()
        && !active
        && let Some(text) = snapshot
            .artifacts
            .iter()
            .find_map(|artifact| match artifact {
                Artifact::Text { text, .. } => Some(strip_fence(text)),
                Artifact::Diff(_) => None,
            })
    {
        output.stdout = text;
    }
    if is_noise_exec_summary(output.stdout.trim()) {
        output.stdout.clear();
    }
    output
}

fn from_object(value: &Value) -> Output {
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned()
    };
    let exit_code = ["exit_code", "exitCode", "code"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_i64))
        .and_then(|code| i32::try_from(code).ok());
    Output {
        stdout: text(&[
            "stdout",
            "output",
            "aggregated_output",
            "formatted_output",
            "text",
        ]),
        stderr: text(&["stderr", "error"]),
        exit_code,
    }
}

/// A text artifact without the code fence a tool wrapped it in.
fn strip_fence(text: &str) -> String {
    let trimmed = text.trim();
    let Some(body) = trimmed.strip_prefix("```") else {
        return text.to_owned();
    };
    let body = body.split_once('\n').map_or("", |(_, rest)| rest);
    body.strip_suffix("```")
        .unwrap_or(body)
        .trim_end()
        .to_owned()
}
