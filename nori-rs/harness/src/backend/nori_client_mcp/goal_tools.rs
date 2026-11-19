//! The goal tools of the `nori-client` server: `get_goal`, `create_goal`, `update_goal`.

use super::*;

const DUPLICATE_CREATE_GOAL_ERROR: &str = "cannot create a new goal because this thread already has a goal; use update_goal only when the existing goal is complete";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(in crate::backend) struct CreateGoalRequest {
    /// Required. The concrete objective to start pursuing. This starts a new
    /// active goal only when no goal is currently defined; if a goal already
    /// exists, this tool fails.
    pub(in crate::backend) objective: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(in crate::backend) struct UpdateGoalRequest {
    /// Required. Set to `complete` only when the objective is achieved and no
    /// required work remains. Set to `blocked` only after the same blocking
    /// condition has repeated and the agent is at an impasse.
    pub(in crate::backend) status: GoalCompletion,
}

/// The only goal transitions the agent may drive through `update_goal`. Other
/// statuses (active/paused/usage_limited/budget_limited) are owned by the user
/// or the backend, so they are intentionally not representable here.
#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "lowercase")]
pub(in crate::backend) enum GoalCompletion {
    Complete,
    Blocked,
}

impl From<GoalCompletion> for GoalStatus {
    fn from(value: GoalCompletion) -> Self {
        match value {
            GoalCompletion::Complete => GoalStatus::Complete,
            GoalCompletion::Blocked => GoalStatus::Blocked,
        }
    }
}

#[tool_router(router = goal_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "Read this thread's current goal state when needed; do not poll.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(in crate::backend) async fn get_goal(&self) -> Result<CallToolResult, McpError> {
        let state = self.shared.thread_goal_state.lock().await;
        let goal = state.snapshot(thread_goal::now_seconds()).map(goal_json);
        Ok(tool_success(serde_json::json!({ "goal": goal })))
    }

    #[tool(description = "Start a thread goal when requested; fails if one already exists.")]
    pub(in crate::backend) async fn create_goal(
        &self,
        Parameters(CreateGoalRequest { objective }): Parameters<CreateGoalRequest>,
    ) -> Result<CallToolResult, McpError> {
        let objective = objective.trim().to_string();
        let result = {
            let now = thread_goal::now_seconds();
            let mut state = self.shared.thread_goal_state.lock().await;
            if state.snapshot(now).is_some() {
                return Ok(tool_error(DUPLICATE_CREATE_GOAL_ERROR));
            }
            state.set_objective(objective, Some(GoalStatus::Active), now)
        };
        Ok(self.respond_with_goal(result).await)
    }

    #[tool(
        description = "Set the active goal status to `complete` (verified done) or `blocked` (recurring impasse)."
    )]
    pub(in crate::backend) async fn update_goal(
        &self,
        Parameters(UpdateGoalRequest { status }): Parameters<UpdateGoalRequest>,
    ) -> Result<CallToolResult, McpError> {
        let result = {
            let now = thread_goal::now_seconds();
            let mut state = self.shared.thread_goal_state.lock().await;
            state.set_status(status.into(), now)
        };
        Ok(self.respond_with_goal(result).await)
    }

    /// Emit a `ThreadGoalUpdated` client event and render the tool reply for a
    /// state mutation, or surface the validation error as a tool error.
    async fn respond_with_goal(
        &self,
        result: Result<thread_goal::ThreadGoalSnapshot, String>,
    ) -> CallToolResult {
        match result {
            Ok(goal) => {
                let _ = self
                    .shared
                    .backend_event_tx
                    .send(BackendEvent::Public(nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::GoalChanged(Some(
                            goal.clone().into_client_goal(),
                        )),
                    )))
                    .await;
                tool_success(serde_json::json!({ "goal": goal_json(goal) }))
            }
            Err(err) => tool_error(err),
        }
    }
}

fn goal_json(goal: thread_goal::ThreadGoalSnapshot) -> serde_json::Value {
    serde_json::json!({
        "objective": goal.objective,
        "status": status_label(goal.status),
        "tokens_used": goal.tokens_used,
        "token_budget": null,
        "tokens_remaining": null,
        "time_used_seconds": goal.time_used_seconds,
        "created_at": goal.created_at,
        "updated_at": goal.updated_at,
    })
}

fn status_label(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::UsageLimited => "usage_limited",
        GoalStatus::BudgetLimited => "budget_limited",
        GoalStatus::Complete => "complete",
    }
}
