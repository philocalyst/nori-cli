//! Deterministic fixture scenarios controlled via `MOCK_SCENARIO`.
//!
//! Supported values:
//! - `comprehensive`: read + bash command + split/unified diff + parallel subagents + markdown answer
//! - `rewind`: multi-turn deterministic answers with file-edit simulation for in-place rewind
//! - `subagents`: two parallel child agents running & producing searchable child transcripts
//! - `bash`: token-highlighted shell pipelines, read range, and code diff cells

use nori_protocol::acp::v1 as acp;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::sleep;

use super::MockAgent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scenario {
    Comprehensive,
    Rewind,
    Subagents,
    Bash,
}

impl Scenario {
    pub(super) fn from_env() -> Option<Self> {
        let raw = std::env::var("MOCK_SCENARIO").ok()?;
        parse_scenario(&raw)
    }
}

pub(super) fn parse_scenario(raw: &str) -> Option<Scenario> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "comprehensive" => Some(Scenario::Comprehensive),
        "rewind" => Some(Scenario::Rewind),
        "subagents" => Some(Scenario::Subagents),
        "bash" => Some(Scenario::Bash),
        _ => None,
    }
}

impl MockAgent {
    pub(super) async fn run_scenario(
        &self,
        scenario: Scenario,
        session_id: acp::SessionId,
        prompt_text: &str,
    ) -> Result<acp::PromptResponse, acp::Error> {
        match scenario {
            Scenario::Bash => self.run_bash_scenario(session_id).await,
            Scenario::Subagents => self.run_subagents_scenario(session_id).await,
            Scenario::Rewind => self.run_rewind_scenario(session_id, prompt_text).await,
            Scenario::Comprehensive => {
                self.run_comprehensive_scenario(session_id, prompt_text)
                    .await
            }
        }
    }

    async fn run_bash_scenario(
        &self,
        session_id: acp::SessionId,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let read_id = acp::ToolCallId::new("scenario-read-1");
        self.send_tool_call(
            session_id.clone(),
            acp::ToolCall::new(read_id.clone(), "Read src/lib.rs")
                .kind(acp::ToolKind::Read)
                .status(acp::ToolCallStatus::Pending)
                .raw_input(json!({"path": "src/lib.rs", "offset": 10, "limit": 4})),
        )
        .await?;
        self.send_tool_call_update(
            session_id.clone(),
            acp::ToolCallUpdate::new(
                read_id,
                acp::ToolCallUpdateFields::new()
                    .status(acp::ToolCallStatus::Completed)
                    .content(vec![acp::ToolCallContent::from(
                        "pub fn parse(input: &str) -> Result<Config, Error> {\n    toml::from_str(input)\n}\n",
                    )]),
            ),
        )
        .await?;

        let exec_id = acp::ToolCallId::new("scenario-exec-1");
        let cmd = "FOO=1 cargo test -p nori-tui -- --nocapture && git diff --stat";
        self.send_tool_call(
            session_id.clone(),
            acp::ToolCall::new(exec_id.clone(), cmd)
                .kind(acp::ToolKind::Execute)
                .status(acp::ToolCallStatus::InProgress)
                .raw_input(json!({"command": cmd})),
        )
        .await?;
        self.send_tool_call_update(
            session_id.clone(),
            acp::ToolCallUpdate::new(
                exec_id,
                acp::ToolCallUpdateFields::new()
                    .status(acp::ToolCallStatus::Completed)
                    .content(vec![acp::ToolCallContent::from(
                        "test result: ok. 12 passed; 0 failed\n",
                    )])
                    .raw_output(json!({"exit_code": 0})),
            ),
        )
        .await?;

        let edit_id = acp::ToolCallId::new("scenario-edit-1");
        self.send_tool_call(
            session_id.clone(),
            acp::ToolCall::new(edit_id.clone(), "Edit src/lib.rs")
                .kind(acp::ToolKind::Edit)
                .status(acp::ToolCallStatus::Completed)
                .content(vec![acp::ToolCallContent::Diff(acp::Diff::new(
                    "src/lib.rs",
                    "pub fn parse(input: &str) -> Result<Config, Error> {\n    toml::from_str(input.trim())\n}\n",
                ).old_text(
                    "pub fn parse(input: &str) -> Result<Config, Error> {\n    toml::from_str(input)\n}\n",
                ))]),
        )
        .await?;

        self.send_text_chunk(
            session_id,
            "Verified `src/lib.rs` and ran the targeted unit tests.",
        )
        .await?;
        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn run_subagents_scenario(
        &self,
        session_id: acp::SessionId,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let children = [
            (
                "scenario-child-1",
                "security-reviewer",
                "Audit auth middleware",
            ),
            ("scenario-child-2", "Explore", "Trace config loader callers"),
        ];
        for (child_id, agent, desc) in children {
            self.cx
                .send_notification(agent_client_protocol::UntypedMessage::new(
                    "session/update",
                    json!({
                        "sessionId": session_id.to_string(),
                        "update": {
                            "sessionUpdate": "subagent_update",
                            "sessionId": child_id,
                            "agent": agent,
                            "model": "claude-sonnet-4",
                            "title": desc,
                            "description": desc,
                            "state": {"state": "running"}
                        }
                    }),
                )?)?;
        }
        sleep(Duration::from_millis(80)).await;
        self.send_tool_call(
            acp::SessionId::new("scenario-child-1"),
            acp::ToolCall::new("child1-exec", "rg -n token_expiry src/auth.rs")
                .kind(acp::ToolKind::Execute)
                .status(acp::ToolCallStatus::Completed)
                .raw_input(json!({"command": "rg -n token_expiry src/auth.rs"})),
        )
        .await?;
        self.send_text_chunk(
            acp::SessionId::new("scenario-child-1"),
            "Found missing clock-skew check in token_expiry validation.",
        )
        .await?;
        self.send_text_chunk(
            acp::SessionId::new("scenario-child-2"),
            "Mapped 3 callers in config_loader.rs.",
        )
        .await?;
        sleep(Duration::from_millis(120)).await;
        for (child_id, _, _) in children {
            self.cx.send_notification(agent_client_protocol::UntypedMessage::new(
                "session/update",
                json!({
                    "sessionId": session_id.to_string(),
                    "update": {
                        "sessionUpdate": "subagent_update",
                        "sessionId": child_id,
                        "state": {"state": "ended", "outcome": {"outcome": "completed", "stopReason": "end_turn"}}
                    }
                }),
            )?)?;
        }
        self.send_text_chunk(
            session_id,
            "Both subagents finished: auth middleware needs a clock-skew guard; 3 config callers mapped.",
        )
        .await?;
        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn run_rewind_scenario(
        &self,
        session_id: acp::SessionId,
        prompt_text: &str,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let turn = self
            .state
            .context_prompt_index
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let reply = format!(
            "Turn {turn} received prompt: {prompt_text}\n\nReady for in-place rewind/edit test."
        );
        self.send_text_chunk(session_id, &reply).await?;
        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn run_comprehensive_scenario(
        &self,
        session_id: acp::SessionId,
        prompt_text: &str,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let _ = self.run_bash_scenario(session_id.clone()).await?;
        let _ = self.run_subagents_scenario(session_id.clone()).await?;
        self.send_text_chunk(
            session_id,
            &format!(
                "\n\nSummary for `{prompt_text}`: all checks, diffs, and child reviews completed."
            ),
        )
        .await?;
        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_known_mock_scenarios() {
        assert_eq!(
            parse_scenario("comprehensive"),
            Some(Scenario::Comprehensive)
        );
        assert_eq!(parse_scenario("REWIND"), Some(Scenario::Rewind));
        assert_eq!(parse_scenario("subagents"), Some(Scenario::Subagents));
        assert_eq!(parse_scenario("bash"), Some(Scenario::Bash));
        assert_eq!(parse_scenario("unknown"), None);
    }
}
