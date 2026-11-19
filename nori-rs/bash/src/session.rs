//! Persistent in-process Bash session backed by `brush-core` and `brush-builtins`.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use brush_builtins::BuiltinSet;
use brush_builtins::ShellBuilderExt as _;
use brush_core::openfiles::OpenFile;
use brush_core::openfiles::OpenFiles;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Notify;

/// Default execution timeout for [`BashSession::exec`] (120 seconds).
pub const DEFAULT_TIMEOUT_MS: i64 = 120_000;

const DRAIN_AFTER_ABORT_MS: u64 = 250;

/// Errors returned by [`BashSession`] operations.
#[derive(Debug, Error)]
pub enum BashError {
    #[error("failed to initialize bash session: {0}")]
    Init(String),

    #[error("bash I/O failure: {0}")]
    Io(#[from] std::io::Error),

    #[error("bash execution failed: {0}")]
    Execution(String),

    #[error("command timed out after {timeout_ms}ms")]
    Timeout {
        timeout_ms: i64,
        stdout: String,
        stderr: String,
    },

    #[error("command was cancelled")]
    Cancelled { stdout: String, stderr: String },
}

/// Structured output returned from a completed bash execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashOutput {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub cwd: PathBuf,
    pub duration_ms: i64,
}

/// One line a command wrote while it was still running; see [`BashSession::exec_streaming`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    /// Written to standard error rather than standard output.
    pub stderr: bool,
    /// The line without its line ending. A line longer than [`MAX_LINE_BYTES`] arrives in parts.
    pub text: String,
}

/// The longest stretch of output delivered as one [`OutputLine`].
pub const MAX_LINE_BYTES: u64 = 64 * 1024;

/// Cooperative cancellation signal for in-flight bash executions.
#[derive(Clone, Default, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Per-execution timeout and cancellation options.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    pub timeout_ms: Option<i64>,
    pub cancel: Option<CancellationToken>,
}

/// Persistent in-process POSIX/Bash session.
///
/// Variables, exported environment, shell functions, aliases, `ERR`/signal traps, programmable
/// completions, and the current working directory persist across [`Self::exec`] calls. An `EXIT`
/// trap does not: it runs when the call that set it ends, as it would for `bash -c`.
pub struct BashSession {
    shell: crate::tracked_tasks::SessionShell,
    default_timeout_ms: Option<i64>,
}

impl BashSession {
    /// Creates a new in-process Bash session rooted at `cwd`.
    pub async fn new(cwd: impl Into<PathBuf>) -> Result<Self, BashError> {
        let cwd = cwd.into();
        if !cwd.is_dir() {
            return Err(BashError::Init(format!(
                "working directory does not exist or is not a directory: {}",
                cwd.display()
            )));
        }

        let null_file =
            brush_core::openfiles::null().map_err(|err| BashError::Init(err.to_string()))?;
        let fds = HashMap::from([
            (OpenFiles::STDIN_FD, null_file.clone()),
            (OpenFiles::STDOUT_FD, null_file.clone()),
            (OpenFiles::STDERR_FD, null_file),
        ]);

        let shell =
            brush_core::Shell::builder_with_extensions::<crate::tracked_tasks::SessionExtensions>()
                .default_builtins(BuiltinSet::BashMode)
                .interactive(false)
                .no_editing(true)
                .profile(brush_core::ProfileLoadBehavior::Skip)
                .rc(brush_core::RcLoadBehavior::Skip)
                .kill_external_commands_on_drop(true)
                .fds(fds)
                .working_dir(cwd)
                .build()
                .await
                .map_err(|err| BashError::Init(err.to_string()))?;

        Ok(Self {
            shell,
            default_timeout_ms: Some(DEFAULT_TIMEOUT_MS),
        })
    }

    /// Returns the session's current working directory.
    pub fn cwd(&self) -> &Path {
        self.shell.working_dir()
    }

    /// Updates the session's current working directory.
    pub fn set_cwd(&mut self, dir: impl AsRef<Path>) -> Result<(), BashError> {
        self.shell
            .set_working_dir(dir)
            .map_err(|err| BashError::Execution(err.to_string()))
    }

    /// Returns the default timeout in milliseconds (`None` if disabled).
    pub const fn default_timeout_ms(&self) -> Option<i64> {
        self.default_timeout_ms
    }

    /// Sets the default timeout in milliseconds (`None` disables the default timeout).
    pub fn set_default_timeout_ms(&mut self, timeout_ms: Option<i64>) {
        self.default_timeout_ms = timeout_ms;
    }

    /// Returns the string value of a shell variable, if set.
    pub fn env_var(&self, name: &str) -> Option<String> {
        self.shell.env_str(name).map(std::borrow::Cow::into_owned)
    }

    /// Sets a global shell variable (optionally exported).
    pub fn set_env(&mut self, name: &str, value: &str, exported: bool) -> Result<(), BashError> {
        let mut var = brush_core::ShellVariable::new(value.to_owned());
        if exported {
            var.export();
        }
        self.shell
            .set_env_global(name, var)
            .map_err(|err| BashError::Execution(err.to_string()))
    }

    /// Returns whether a shell function named `name` is defined.
    pub fn has_function(&self, name: &str) -> bool {
        self.shell.funcs().get(name).is_some()
    }

    /// Returns whether an alias named `name` is defined.
    pub fn has_alias(&self, name: &str) -> bool {
        self.shell.aliases().contains_key(name)
    }

    /// Executes `command` using the session's default timeout.
    pub async fn exec(&mut self, command: &str) -> Result<BashOutput, BashError> {
        self.exec_with_options(command, ExecOptions::default())
            .await
    }

    /// Executes `command` with custom timeout and cancellation options.
    pub async fn exec_with_options(
        &mut self,
        command: &str,
        options: ExecOptions,
    ) -> Result<BashOutput, BashError> {
        self.exec_inner(command, options, None).await
    }

    /// Executes `command` and sends each line it writes to `lines` as it is written (waiting when
    /// `lines` is full), instead of collecting the output: the returned [`BashOutput`] has empty `stdout` and `stderr`. The
    /// call ends when the command does, or on timeout or cancellation. Without a timeout in
    /// `options` the session's default applies; a command meant to run indefinitely sets
    /// [`BashSession::set_default_timeout_ms`] to `None` first.
    pub async fn exec_streaming(
        &mut self,
        command: &str,
        options: ExecOptions,
        lines: tokio::sync::mpsc::Sender<OutputLine>,
    ) -> Result<BashOutput, BashError> {
        self.exec_inner(command, options, Some(lines)).await
    }

    async fn exec_inner(
        &mut self,
        command: &str,
        options: ExecOptions,
        lines: Option<tokio::sync::mpsc::Sender<OutputLine>>,
    ) -> Result<BashOutput, BashError> {
        if options
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(BashError::Cancelled {
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        let (stdout_rx, stdout_tx) = std::io::pipe()?;
        let (stderr_rx, stderr_tx) = std::io::pipe()?;
        let null_stdin =
            brush_core::openfiles::null().map_err(|err| BashError::Init(err.to_string()))?;

        let mut params = self.shell.default_exec_params();
        params.set_fd(OpenFiles::STDIN_FD, null_stdin);
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(stdout_tx));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(stderr_tx));

        let stdout_lines = lines.clone();
        let stdout_task = tokio::task::spawn_blocking(move || match stdout_lines {
            Some(lines) => stream_lines(stdout_rx, false, &lines),
            None => read_pipe_lossy(stdout_rx),
        });
        let stderr_task = tokio::task::spawn_blocking(move || match lines {
            Some(lines) => stream_lines(stderr_rx, true, &lines),
            None => read_pipe_lossy(stderr_rx),
        });

        let timeout_ms = options.timeout_ms.or(self.default_timeout_ms);
        let cancel = options.cancel;
        let started = Instant::now();
        let source_info = brush_core::SourceInfo::from("-c");

        let shell = &mut self.shell;
        let outcome = run_with_limits(
            async {
                let result = shell.run_string(command, &source_info, &params).await;
                // Each call is a script: its EXIT trap runs when it ends, however it ended, with
                // the call's own output streams, and is then gone, as with `bash -c`. A trap
                // that exits sets the call's status.
                if result.is_ok()
                    && let Some(handler) = shell
                        .traps()
                        .get_handler(brush_core::traps::TrapSignal::Exit)
                        .cloned()
                {
                    shell
                        .traps_mut()
                        .remove_handlers(brush_core::traps::TrapSignal::Exit);
                    return match shell
                        .run_string(&handler.command, &handler.source_info, &params)
                        .await
                    {
                        Ok(trap_result) if trap_result.is_exit() => Ok(trap_result),
                        Ok(_) => result,
                        Err(error) => Err(error),
                    };
                }
                result
            },
            timeout_ms,
            cancel.as_ref(),
        )
        .await;

        drop(params);

        // A command that timed out or was cancelled leaves no background work behind: what the
        // tool reports stopped is stopped.
        if !matches!(outcome, RunOutcome::Completed(_)) {
            self.shell.task_spawner().abort_all();
            self.shell.jobs_mut().jobs.clear();
        }

        let bounded_drain =
            !matches!(outcome, RunOutcome::Completed(_)) || !self.shell.jobs().jobs.is_empty();
        let (stdout, stderr) = drain_outputs(stdout_task, stderr_task, bounded_drain).await;
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

        match outcome {
            RunOutcome::Completed(Ok(result)) => Ok(BashOutput {
                exit_code: i64::from(u8::from(result.exit_code)),
                stdout,
                stderr,
                cwd: self.shell.working_dir().to_path_buf(),
                duration_ms,
            }),
            RunOutcome::Completed(Err(err)) => Err(BashError::Execution(err.to_string())),
            RunOutcome::TimedOut(timeout_ms) => Err(BashError::Timeout {
                timeout_ms,
                stdout,
                stderr,
            }),
            RunOutcome::Cancelled => Err(BashError::Cancelled { stdout, stderr }),
        }
    }

    pub(crate) const fn shell(&self) -> &crate::tracked_tasks::SessionShell {
        &self.shell
    }

    pub(crate) const fn shell_mut(&mut self) -> &mut crate::tracked_tasks::SessionShell {
        &mut self.shell
    }
}

enum RunOutcome {
    Completed(Result<brush_core::ExecutionResult, brush_core::Error>),
    TimedOut(i64),
    Cancelled,
}

async fn run_with_limits(
    exec_future: impl std::future::Future<
        Output = Result<brush_core::ExecutionResult, brush_core::Error>,
    >,
    timeout_ms: Option<i64>,
    cancel: Option<&CancellationToken>,
) -> RunOutcome {
    let sleep_fut = async {
        match timeout_ms {
            Some(ms) => {
                let millis = u64::try_from(ms.max(0)).unwrap_or(u64::MAX);
                tokio::time::sleep(Duration::from_millis(millis)).await;
                ms
            }
            None => std::future::pending().await,
        }
    };

    let cancel_fut = async {
        match cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };

    tokio::select! {
        biased;
        () = cancel_fut => RunOutcome::Cancelled,
        ms = sleep_fut => RunOutcome::TimedOut(ms),
        res = exec_future => RunOutcome::Completed(res),
    }
}

fn read_pipe_lossy(mut reader: std::io::PipeReader) -> String {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Send what `reader` yields to `lines`, one line at a time, until it closes or nobody listens.
/// Nothing is kept, so the returned text is empty.
fn stream_lines(
    reader: std::io::PipeReader,
    stderr: bool,
    lines: &tokio::sync::mpsc::Sender<OutputLine>,
) -> String {
    use std::io::BufRead as _;

    let mut reader = std::io::BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut buffer)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        while matches!(buffer.last(), Some(b'\n' | b'\r')) {
            buffer.pop();
        }
        let text = String::from_utf8_lossy(&buffer).into_owned();
        // Blocks while the receiver is behind, so a command that writes faster than it is read
        // waits on its pipe instead of filling memory.
        if lines.blocking_send(OutputLine { stderr, text }).is_err() {
            break;
        }
    }
    String::new()
}

async fn drain_outputs(
    stdout_task: tokio::task::JoinHandle<String>,
    stderr_task: tokio::task::JoinHandle<String>,
    bounded: bool,
) -> (String, String) {
    let join_all = async {
        let (stdout, stderr) = tokio::join!(stdout_task, stderr_task);
        (stdout.unwrap_or_default(), stderr.unwrap_or_default())
    };
    if bounded {
        tokio::time::timeout(Duration::from_millis(DRAIN_AFTER_ABORT_MS), join_all)
            .await
            .unwrap_or_default()
    } else {
        join_all.await
    }
}
