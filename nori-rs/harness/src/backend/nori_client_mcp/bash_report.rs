//! What the `bash` tool tells the model about a run.
//!
//! The first block of every result is one status line: `exit 0`, `exit 1`, `exit 143 (signal
//! TERM)`, `killed: timed out after 120s`, `killed: cancelled`. It is a block of its own, so no
//! truncation of the output can cut it off, and anything but `exit 0` is an error result
//! (`isError`). The second block is the JSON body with the streams, each capped so a runaway
//! command cannot bury the status or flood the context.

use nori_bash::BashError;
use nori_bash::BashOutput;
use rmcp::model::CallToolResult;
use rmcp::model::Content;

/// The most bytes of one stream the model sees: the start and the end of it.
const MAX_STREAM_BYTES: usize = 64 * 1024;

/// One stream as shown: whole, or its start and end around a note of what was left out.
fn clip(text: &str) -> String {
    if text.len() <= MAX_STREAM_BYTES {
        return text.to_owned();
    }
    let half = MAX_STREAM_BYTES / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... {} bytes omitted ...]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

/// The name of the signal a conventional exit status above 128 stands for, when it is one.
fn signal_name(exit_code: i64) -> Option<&'static str> {
    Some(match exit_code.checked_sub(128)? {
        1 => "HUP",
        2 => "INT",
        3 => "QUIT",
        6 => "ABRT",
        9 => "KILL",
        11 => "SEGV",
        13 => "PIPE",
        14 => "ALRM",
        15 => "TERM",
        _ => return None,
    })
}

fn result(status: &str, body: &serde_json::Value, is_error: bool) -> CallToolResult {
    let content = vec![Content::text(status), Content::text(body.to_string())];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// The tool result for a finished (or stopped) run.
pub(super) fn report(run: Result<BashOutput, BashError>) -> CallToolResult {
    match run {
        Ok(output) => {
            let status = match signal_name(output.exit_code) {
                Some(signal) if output.exit_code != 0 => {
                    format!("exit {} (signal {signal})", output.exit_code)
                }
                _ => format!("exit {}", output.exit_code),
            };
            let body = serde_json::json!({
                "status": status,
                "exit_code": output.exit_code,
                "stdout": clip(&output.stdout),
                "stderr": clip(&output.stderr),
                "cwd": output.cwd,
                "duration_ms": output.duration_ms,
            });
            result(&status, &body, output.exit_code != 0)
        }
        Err(BashError::Timeout {
            timeout_ms,
            stdout,
            stderr,
        }) => {
            let status = format!(
                "killed: timed out after {}s (the command and its background jobs were stopped)",
                timeout_ms / 1000
            );
            let body = serde_json::json!({
                "status": status,
                "exit_code": null,
                "timed_out": true,
                "stdout": clip(&stdout),
                "stderr": clip(&stderr),
            });
            result(&status, &body, true)
        }
        Err(BashError::Cancelled { stdout, stderr }) => {
            let status = "killed: cancelled".to_owned();
            let body = serde_json::json!({
                "status": status,
                "exit_code": null,
                "cancelled": true,
                "stdout": clip(&stdout),
                "stderr": clip(&stderr),
            });
            result(&status, &body, true)
        }
        Err(error) => {
            let status = format!("failed to run: {error}");
            result(
                &status,
                &serde_json::json!({ "status": status, "exit_code": null }),
                true,
            )
        }
    }
}
