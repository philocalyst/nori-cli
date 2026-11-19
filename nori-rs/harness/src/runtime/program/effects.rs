//! Session-level effects of a program: prompts, configuration, context, forks, rewinds,
//! processes and host tools. Each holds the program-effect admission, so a rewind cannot race
//! it, and fails when the conversation was rewound after the program began.

use std::sync::atomic::Ordering;

use nori_conduit::ConduitError;
use nori_conduit::ConduitValue;
use nori_conduit::SandboxPolicy;
use nori_conduit::ToolOutcome;
use nori_conduit::enforce_bash_sandbox;
use tokio::sync::RwLockReadGuard;
use tokio::sync::oneshot;

use super::host::HarnessProgramHost;
use crate::backend::PromptAdmission;

fn effect_error(what: &str, error: impl std::fmt::Display) -> ConduitError {
    ConduitError::Runtime {
        message: format!("{what}: {error}"),
    }
}

impl HarnessProgramHost {
    async fn admit(&self) -> Result<RwLockReadGuard<'_, ()>, ConduitError> {
        let admission = self.backend.program_effect_admission().await;
        if self.backend.program_conversation_generation() != self.generation.load(Ordering::Acquire)
        {
            return Err(ConduitError::Runtime {
                message: "The conversation was rewound before this program could continue"
                    .to_owned(),
            });
        }
        Ok(admission)
    }

    pub(super) async fn submit_prompt(&self, text: &str) -> Result<(), ConduitError> {
        let _admission = self.admit().await?;
        let (response_tx, response_rx) = oneshot::channel();
        self.backend
            .submit_prompt(
                vec![nori_protocol::acp::v1::ContentBlock::Text(
                    nori_protocol::acp::v1::TextContent::new(text.to_owned()),
                )],
                None,
                PromptAdmission::Queue,
                response_tx,
            )
            .await;
        response_rx
            .await
            .map_err(|error| effect_error("prompt", error))?
            .map(drop)
            .map_err(|error| effect_error("prompt", error))
    }

    pub(super) async fn apply_config(&self, key: &str, value: &str) -> Result<(), ConduitError> {
        let _admission = self.admit().await?;
        self.backend
            .set_config_option(key.to_owned(), value.to_owned())
            .await
            .map_err(|error| effect_error("config", error))
    }

    pub(super) async fn context_range(&self, start: i64, end: i64) -> Result<String, ConduitError> {
        let _admission = self.admit().await?;
        let records = self
            .recorder
            .projected_fork_records(self.recorder.session_id(), start, end)
            .await
            .map_err(|error| effect_error("select_context", error))?;
        crate::transcript::fork_context::context_from_records(records)
            .map_err(|error| effect_error("select_context", error))
    }

    pub(super) async fn fork_range(
        &self,
        source: &str,
        start: i64,
        end: i64,
    ) -> Result<(), ConduitError> {
        let _admission = self.admit().await?;
        let source = if source == "self" {
            self.recorder.session_id().to_string()
        } else {
            source.to_owned()
        };
        self.backend
            .fork_history(String::new(), Some((source, start, end)))
            .await
            .map_err(|error| effect_error("fork", error))
    }

    /// A rewind takes the admission itself; the program may go on after its own rewind.
    pub(super) async fn rewind_conversation(&self, head: Option<i64>) -> Result<(), ConduitError> {
        let expected = self.generation.load(Ordering::Acquire);
        self.backend
            .rewind_program(self.recorder.session_id().to_string(), head, expected)
            .await
            .map_err(|error| effect_error("rewind", error))?;
        self.generation.store(
            expected
                .checked_add(1)
                .ok_or_else(|| ConduitError::Runtime {
                    message: "conversation generation overflow".to_owned(),
                })?,
            Ordering::Release,
        );
        Ok(())
    }

    pub(super) async fn run_process(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
    ) -> Result<ToolOutcome, ConduitError> {
        let _admission = self.admit().await?;
        let result = self
            .backend
            .execute_unsafe_program(program, args, cwd)
            .await
            .map_err(|error| ConduitError::ToolFailure {
                name: program.to_owned(),
                reason: format!("failed to start: {error}"),
            })?;
        self.recorder
            .record_command("unsafe-result", result.to_string())
            .await
            .map_err(|error| effect_error("journal", error))?;
        Ok(ToolOutcome {
            output: ConduitValue::from_json(result),
            tokens: 0,
        })
    }

    pub(super) async fn host_tool(
        &self,
        name: &str,
        args: &ConduitValue,
        sandbox: Option<&SandboxPolicy>,
    ) -> Result<ToolOutcome, ConduitError> {
        let arguments = args.to_plain_json();
        if name == "bash"
            && let Some(command) = arguments.get("command").and_then(serde_json::Value::as_str)
        {
            enforce_bash_sandbox(sandbox, self.backend.program_cwd(), command)?;
        }
        let _admission = self.admit().await?;
        let output = self
            .backend
            .execute_host_tool(name, arguments)
            .await
            .map_err(|reason| ConduitError::ToolFailure {
                name: name.to_owned(),
                reason,
            })?;
        if output.is_error {
            return Err(ConduitError::ToolFailure {
                name: name.to_owned(),
                reason: output.text,
            });
        }
        Ok(ToolOutcome {
            output: ConduitValue::String(output.text),
            tokens: 0,
        })
    }
}
