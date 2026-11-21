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
    // Nori's `bash` names the machine it ran on in its input.
    let machine = snapshot
        .raw_input
        .as_ref()
        .and_then(|input| input.get("machine"))
        .and_then(Value::as_str)
        .filter(|machine| !machine.is_empty() && *machine != "local");
    ShellBlock {
        command: &command,
        progress: cell.progress(),
        since: cell.start_time,
        animate: cell.animations_enabled,
        elapsed: cell.elapsed,
        exit_code: output.exit_code,
        cwd: None,
        machine,
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
        Some(Value::String(text)) => from_text(text),
        // An MCP result: Nori's `bash` puts a status line, then its JSON body, in text blocks.
        Some(value @ Value::Object(_)) => value
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .find_map(result_body)
            .map_or_else(|| from_object(value), |body| from_object(&body)),
        Some(_) | None => Output::default(),
    };
    // While it runs, a text artifact is the tool's description, not its output. Once it ends, a
    // text artifact is the result as the agent passed it on: the same JSON body, often.
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
        let from_artifact = from_text(&text);
        output.stdout = from_artifact.stdout;
        output.stderr = from_artifact.stderr;
        output.exit_code = output.exit_code.or(from_artifact.exit_code);
    }
    if is_noise_exec_summary(output.stdout.trim()) {
        output.stdout.clear();
    }
    // A command that prints its own status last (`…; echo "exit=$?"`) reports it: it is shown
    // as the command's status, not as a line of its output.
    if command_of(snapshot).contains("$?")
        && let Some((rest, code)) = trailing_status(&output.stdout)
    {
        output.exit_code = output.exit_code.or(Some(code));
        output.stdout = rest.to_owned();
    }
    output
}

/// The output without its last line when that line is an exit status the command printed
/// (`exit=0`, `exit: 1`, `EXIT_CODE=2`, `exit code 0`, `[exit 0]`), and the status.
fn trailing_status(stdout: &str) -> Option<(&str, i32)> {
    let trimmed = stdout.trim_end();
    let (rest, last) = trimmed.rsplit_once('\n').unwrap_or(("", trimmed));
    let line = last
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let value = ["exit_code", "exit code", "exitcode", "exit status", "exit"]
        .iter()
        .find_map(|label| line.strip_prefix(label))?
        .trim_start_matches(['=', ':', ' ']);
    let code = value.trim().parse::<i32>().ok()?;
    Some((rest.trim_end_matches('\n'), code))
}

/// A shell result in text: Nori's `bash` JSON body (alone, or after its status line), or what
/// the command printed.
fn from_text(text: &str) -> Output {
    match result_body(text) {
        Some(body) => from_object(&body),
        None => Output {
            stdout: text.to_owned(),
            ..Output::default()
        },
    }
}

/// The JSON body of a shell result, on its own or after a status line: an object that says what
/// the command printed or how it exited.
fn result_body(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let body = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        trimmed
            .split_once('\n')
            .and_then(|(_, body)| serde_json::from_str::<Value>(body.trim()).ok())
    })?;
    let says = ["stdout", "stderr", "exit_code", "exitCode", "output"]
        .iter()
        .any(|key| body.get(*key).is_some());
    (body.is_object() && says).then_some(body)
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::history_cell::HistoryCell;
    use crate::presentation::Invocation;
    use crate::presentation::ToolKind;
    use crate::presentation::ToolPhase;
    use crate::presentation::ToolSnapshot;

    use super::ClientToolCell;

    #[test]
    fn a_command_on_another_machine_says_where_it_ran() {
        let snapshot = ToolSnapshot {
            call_id: "call".to_owned(),
            title: "bash".to_owned(),
            kind: ToolKind::Execute,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Command {
                command: "uname -sm && pwd".to_owned(),
            }),
            artifacts: vec![],
            raw_input: Some(serde_json::json!({
                "command": "uname -sm && pwd",
                "machine": "mothership",
            })),
            raw_output: Some(serde_json::json!({
                "exit_code": 0,
                "machine": "mothership",
                "stdout": "Linux x86_64\n/home/miles\n",
                "stderr": "",
            })),
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/repo"), false);
        let text = cell
            .display_lines(60)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(text);
    }

    #[test]
    fn a_result_passed_on_as_text_shows_what_the_command_printed_not_its_json() {
        let snapshot = ToolSnapshot {
            call_id: "call".to_owned(),
            title: "bash".to_owned(),
            kind: ToolKind::Execute,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Command {
                command: "wc -l src/*.rs".to_owned(),
            }),
            artifacts: vec![crate::presentation::Artifact::Text {
                text: "exit 0\n{\"status\":\"exit 0\",\"exit_code\":0,\"stdout\":\"=== 13850\\n  120 src/a.rs\\n\",\"stderr\":\"\"}".to_owned(),
                format: crate::presentation::TextFormat::default(),
            }],
            raw_input: Some(serde_json::json!({ "command": "wc -l src/*.rs" })),
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/repo"), false);
        let text = cell
            .display_lines(60)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("exit_code"), "{text}");
        insta::assert_snapshot!(text);
    }

    #[test]
    fn a_status_the_command_prints_last_is_shown_as_its_status() {
        let snapshot = ToolSnapshot {
            call_id: "call".to_owned(),
            title: "bash".to_owned(),
            kind: ToolKind::Execute,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Command {
                command: "cargo check -q; echo \"exit=$?\"".to_owned(),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: Some(serde_json::json!({ "stdout": "Finished dev\nexit=0\n" })),
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/repo"), false);
        let text = cell
            .display_lines(60)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text
                .lines()
                .any(|line| line.contains('│') && line.contains("exit=")),
            "{text}"
        );
        insta::assert_snapshot!(text);
    }
}
