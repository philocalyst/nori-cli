//! Running shell commands in the session's own persistent shell, for when Nori does not lend its
//! `bash` (an agent served to another ACP client).

use async_trait::async_trait;
use nori_acp_kit::schema::ToolKind;
use nori_bash::BashError;
use nori_bash::BashOutput;
use nori_llm::ToolSpec;
use serde_json::Value;
use serde_json::json;

use super::Check;
use super::Invocation;
use super::Tool;
use super::ToolContext;
use super::ToolOutput;
use super::string_arg;

const DEFAULT_TIMEOUT_MS: i64 = 120_000;
const MAX_TIMEOUT_MS: i64 = 600_000;
/// Bytes of one stream the model sees: its start and its end.
const MAX_STREAM_BYTES: usize = 32 * 1024;

/// The arguments a shell tool takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStyle {
    /// OpenCode's `bash`: `command`, `timeout` in milliseconds, `description`.
    Bash,
    /// Codex's `shell_command`: `command`, `workdir`, `timeout_ms`.
    ShellCommand,
}

/// A shell tool.
pub struct Shell {
    pub style: ShellStyle,
    pub name: String,
    pub description: String,
}

impl Shell {
    pub fn new(style: ShellStyle) -> Self {
        let (name, description) = match style {
            ShellStyle::Bash => (
                "bash",
                "Run a command in a persistent bash shell (`cd`, exported variables and functions persist between calls). Give a short `description` of what it does. `timeout` is in milliseconds (default 120000, at most 600000). Quote paths with spaces. Use the dedicated tools to read, search and edit files instead of `cat`, `grep`, `find` or `sed`. Chain dependent steps with `&&`. The result starts with the exit status; output beyond 32 KB per stream is cut in the middle.",
            ),
            ShellStyle::ShellCommand => (
                "shell_command",
                "Run a shell command and return its output. `workdir` sets the directory it runs in (default: the working directory); always set it rather than using `cd`. `timeout_ms` defaults to 120000. Prefer `rg` for searching. The result starts with the exit status.",
            ),
        };
        Self {
            style,
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    fn timeout_key(&self) -> &'static str {
        match self.style {
            ShellStyle::Bash => "timeout",
            ShellStyle::ShellCommand => "timeout_ms",
        }
    }
}

fn title(command: &str) -> String {
    let first = command.lines().next().unwrap_or_default();
    let mut title: String = first.chars().take(100).collect();
    if first.chars().count() > 100 || command.lines().count() > 1 {
        title.push('…');
    }
    title
}

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

fn report(run: Result<BashOutput, BashError>) -> ToolOutput {
    let (status, stdout, stderr, failed) = match run {
        Ok(output) => (
            format!("exit {}", output.exit_code),
            output.stdout,
            output.stderr,
            output.exit_code != 0,
        ),
        Err(BashError::Timeout {
            timeout_ms,
            stdout,
            stderr,
        }) => (
            format!("killed: timed out after {}s", timeout_ms / 1000),
            stdout,
            stderr,
            true,
        ),
        Err(BashError::Cancelled { stdout, stderr }) => {
            ("killed: cancelled".to_owned(), stdout, stderr, true)
        }
        Err(error) => (
            format!("failed: {error}"),
            String::new(),
            String::new(),
            true,
        ),
    };
    let mut text = status;
    if !stdout.is_empty() {
        text.push('\n');
        text.push_str(&clip(&stdout));
    }
    if !stderr.is_empty() {
        text.push_str("\nstderr:\n");
        text.push_str(&clip(&stderr));
    }
    if failed {
        ToolOutput::error(text)
    } else {
        ToolOutput::text(text)
    }
}

fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> ToolSpec {
        let schema = match self.style {
            ShellStyle::Bash => json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "timeout": {"type": "number", "description": "Timeout in milliseconds"},
                    "description": {"type": "string", "description": "What the command does, in 5-10 words"},
                },
                "required": ["command"],
            }),
            ShellStyle::ShellCommand => json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell script to run"},
                    "workdir": {"type": "string", "description": "The directory to run it in"},
                    "timeout_ms": {"type": "number", "description": "Timeout in milliseconds"},
                },
                "required": ["command"],
            }),
        };
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: schema,
        }
    }

    async fn prepare(&self, input: &Value, _context: &ToolContext) -> Result<Invocation, String> {
        let command = string_arg(input, "command")?;
        let mut invocation = Invocation::new(title(command), ToolKind::Execute);
        invocation.checks = vec![Check::new("shell", command)];
        Ok(invocation)
    }

    fn handles_cancellation(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, context: &ToolContext) -> ToolOutput {
        let Ok(command) = string_arg(&input, "command") else {
            return ToolOutput::error("The `command` argument is required.");
        };
        let command = match input["workdir"].as_str().filter(|dir| !dir.is_empty()) {
            Some(dir) => format!("(cd {} && {command})", quote(dir)),
            None => command.to_owned(),
        };
        let timeout_ms = input[self.timeout_key()]
            .as_i64()
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let mut shell = context.shell.lock().await;
        if shell.is_none() {
            match nori_bash::BashSession::new(context.cwd.clone()).await {
                Ok(created) => *shell = Some(created),
                Err(error) => {
                    return ToolOutput::error(format!("Could not start a shell: {error}"));
                }
            }
        }
        let Some(session) = shell.as_mut() else {
            return ToolOutput::error("Could not start a shell.");
        };
        let cancel = nori_bash::CancellationToken::new();
        let watcher = {
            let cancel = cancel.clone();
            let turn = context.cancel.clone();
            tokio::spawn(async move {
                turn.cancelled().await;
                cancel.cancel();
            })
        };
        let options = nori_bash::ExecOptions {
            timeout_ms: Some(timeout_ms),
            cancel: Some(cancel),
        };
        let run = session.exec_with_options(&command, options).await;
        watcher.abort();
        report(run)
    }

    fn replaced_by_host(&self) -> Option<&'static str> {
        Some("bash")
    }
}
