//! The native Pkl conversation-program tool.
use super::*;

const PROGRAM_HELP: &str = "A program is a Pkl module: `amends \"nori:Program\"`, `name`, `actors { [\"alias\"] { agent = \"claude-code\"; model = \"opus\" } }` and `steps { ... }` of typed steps. Steps: Send(to, message, await_reply = true, delivery = \"queue\"|\"steer\"), Await(receipt), Spawn(actor), Prompt(text), Config(key, value), Call(tool, args), Unsafe(program, args, cwd), SelectContext(start, end), Fork(actor, start, end), Rewind(head), Sequence, Par(branches), Race(branches), FanOut(over, each, steps), Loop(max_iterations, until, steps), When(check, steps, otherwise), Guard(phase, check, feedback, steps), Budget(tokens, steps), Within(world, steps), Retry(steps), Recover(steps, fallback), Emit(topic, payload), AwaitSignal(topic), Interrupt(prompt), Delay(duration), Pin(fact), Checkpoint(name). Every step may set `id`, `bind` (names its value for later `{{name}}` references and checks), `timeout` and `recovery { retry { ... } fallback { chain = ... } }`. An actor may also declare `tools` (the Nori tools it is given), `skills`, a `sandbox { allowed_tools = ...; allowed_paths = ... }` enforced on its `bash`, and a `world` (own worktree, optionally its own Nori home). `Send` to an actor the program declares starts it on first use; address an existing actor as `actor://alias`. Example: `amends \"nori:Program\"\nname = \"review\"\nactors { [\"dev\"] { agent = \"claude-code\" }; [\"rev\"] { agent = \"codex\" } }\nsteps {\n  new Send { to = \"dev\"; message = \"Implement X\"; bind = \"patch\" }\n  new Send { to = \"rev\"; message = \"Review: {{patch}}\"; bind = \"verdict\" }\n}\nresult = \"verdict\"`. Errors name the file, line and step path.";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationProgramRequest {
    /// Pkl module that amends `nori:Program`.
    pub(super) source: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationContextRequest {
    /// Inclusive visible event ID. Omit for the most recent records.
    start: Option<i64>,
    /// Inclusive visible event ID. Omit for the current head.
    end: Option<i64>,
    /// Maximum records to return, 1 to 200; default 50.
    limit: Option<i64>,
}

#[tool_router(router = conversation_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "Read recorded transcript entries, identity, current head, and visible event IDs (`start`/`end` inclusive, `limit` 1–200, default last 50). Use returned IDs with `conversation` for `select-context`, `fork`, or `rewind`.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn conversation_context(
        &self,
        Parameters(request): Parameters<ConversationContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .shared
            .orchestrator
            .conversation_context(request.start, request.end, request.limit)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        Ok(tool_success(result))
    }

    #[tool(
        description = "Run a Nori program: a Pkl module that amends `nori:Program`, declaring actors (agent sessions) and typed steps (send, await, par, race, fan_out, loop, guard, budget, retry, recover, emit, ...) with retry and fallback recovery. Every step is journaled under `.nori/runs/<run>/journal.jsonl` with a re-loadable `state.pkl`; the result names both. A `Send` waits for the actor's reply by default; set `await_reply = false` to queue it and `Await` later. Use `delegate_task` + `task_wait` for a single tracked delegation instead."
    )]
    pub(crate) async fn conversation(
        &self,
        Parameters(request): Parameters<ConversationProgramRequest>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .shared
            .orchestrator
            .execute_conversation_program(request.source)
            .await
            .map_err(|error| McpError::internal_error(format!("{error}\n{PROGRAM_HELP}"), None))?;
        Ok(tool_success(serde_json::to_value(result).map_err(
            |error| McpError::internal_error(error.to_string(), None),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_definition_and_errors_explain_real_program_steps() {
        let definition = NoriClientService::all_tools()
            .into_iter()
            .find(|tool| tool.name == "conversation")
            .unwrap();
        let description = definition.description.unwrap();
        assert!(description.contains("nori:Program"));
        for term in [
            "Send(to, message",
            "Await(receipt)",
            "bind",
            "actor://alias",
        ] {
            assert!(PROGRAM_HELP.contains(term), "{term}");
        }
    }
}
