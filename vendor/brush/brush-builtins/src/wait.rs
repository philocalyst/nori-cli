use std::io::Write;

use brush_core::{ExecutionExitCode, ExecutionResult, builtins, error};

/// Wait for jobs to terminate.
#[derive(winnow_args::Args)]
#[arg(disable_help_short, disable_version_flag, disable_help_subcommand)]
pub(crate) struct WaitCommand {
    /// Wait for specified job to terminate (instead of change status).
    #[arg(short = 'f')]
    wait_for_terminate: bool,

    /// Wait for a single job to change status; if jobs are specified, waits for
    /// the first to change status, and otherwise waits for the next change.
    #[arg(short = 'n')]
    wait_for_first_or_next: bool,

    /// Name of variable to receive the job ID of the job whose status is indicated.
    #[arg(short = 'p', value_name = "VAR_NAME")]
    variable_to_receive_id: Option<String>,

    /// Process IDs or job specs to wait for.
    #[arg(positional)]
    ids: Vec<String>,
}

brush_winnow_args_adapter::impl_parse_args!(WaitCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for WaitCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if self.wait_for_terminate {
            return error::unimp("wait -f");
        }
        if self.wait_for_first_or_next {
            return error::unimp("wait -n");
        }
        if self.variable_to_receive_id.is_some() {
            return error::unimp("wait -p");
        }

        let mut result = ExecutionResult::success();

        if !self.ids.is_empty() {
            for id in &self.ids {
                if id.starts_with('%') {
                    // It's a job spec.
                    if let Some(job) = context.shell.jobs_mut().resolve_job_spec(id) {
                        // Take the job's status, but not its control flow: an `exit` in a
                        // background job ends only that job.
                        result = job.wait().await?.exit_code.into();
                    } else {
                        writeln!(
                            context.stderr(),
                            "{}: no such job: {}",
                            context.command_name,
                            id
                        )?;

                        result = ExecutionExitCode::GeneralError.into();
                    }
                } else if let Ok(pid) = id.parse::<i32>() {
                    // It's a process ID: the status of that job, and no other.
                    if let Some(job) = context.shell.jobs_mut().resolve_pid(pid) {
                        result = job.wait().await?.exit_code.into();
                    } else if let Some(status) = context.shell.jobs_mut().take_reaped_status(pid) {
                        result = brush_core::ExecutionResult::new(status);
                    } else {
                        writeln!(
                            context.stderr(),
                            "{}: pid {} is not a child of this shell",
                            context.command_name,
                            id
                        )?;
                        result = ExecutionExitCode::from(127).into();
                    }
                } else {
                    writeln!(
                        context.stderr(),
                        "{}: `{}': not a pid or valid job spec",
                        context.command_name,
                        id
                    )?;
                    result = ExecutionExitCode::GeneralError.into();
                }
            }
        } else {
            // Wait for all jobs.
            let jobs = context.shell.jobs_mut().wait_all().await?;

            if context.shell.options().enable_job_control {
                for job in jobs {
                    writeln!(context.stdout(), "{job}")?;
                }
            }
        }

        Ok(result)
    }
}
