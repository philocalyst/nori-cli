//! The goal tools of the `nori-client` server: `get_goal`, `create_goal`, `update_goal`.

use super::*;

const DUPLICATE_CREATE_GOAL_ERROR: &str = "cannot create a new goal because this thread already has a goal; use update_goal only when the existing goal is complete";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(in crate::backend) struct CreateGoalRequest {
    /// Required. The concrete objective to pursue, stated so that "done" can be checked.
    pub(in crate::backend) objective: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(in crate::backend) struct UpdateGoalRequest {
    /// Required. `complete` only when the objective is achieved and no required work remains;
    /// `blocked` only after the same blocker has repeated and you are at an impasse.
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
        description = "Read this thread's goal (`goal`: objective, status, tokens and time used; `null` when there is none). Nori delivers goal changes itself, so call this only to inspect the state, never to poll.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(in crate::backend) async fn get_goal(&self) -> Result<CallToolResult, McpError> {
        let state = self.shared.thread_goal_state.lock().await;
        let goal = state.snapshot(thread_goal::now_seconds()).map(goal_json);
        Ok(tool_success(serde_json::json!({ "goal": goal })))
    }

    #[tool(
        description = "Start a goal for this thread, only when the user asks for one. Fails if the thread already has a goal (changing or replacing it is the user's call). Returns the new goal."
    )]
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
        description = "Set the active goal's status to `complete` (verified done, no required work left) or `blocked` (the same blocker has repeated and you are at an impasse); other statuses belong to the user. A `complete` claim is checked by an independent reviewer of the transcript, and the result says what it found: if the goal is not done yet, keep working and claim it again only when the evidence shows it is. Repeated refusals (default 3 in a row) pause the goal for the user; if the reviewer cannot run, the goal completes, marked unverified. Returns the goal."
    )]
    pub(in crate::backend) async fn update_goal(
        &self,
        Parameters(UpdateGoalRequest { status }): Parameters<UpdateGoalRequest>,
    ) -> Result<CallToolResult, McpError> {
        match status {
            GoalCompletion::Blocked => {
                let result = {
                    let now = thread_goal::now_seconds();
                    let mut state = self.shared.thread_goal_state.lock().await;
                    state.set_status(status.into(), now)
                };
                Ok(self.respond_with_goal(result).await)
            }
            // Reviewed and applied on a task of its own: an agent whose client gives up on the
            // call (Codex after a minute) must not leave the review half done.
            GoalCompletion::Complete => Ok(tokio::spawn(self.shared.clone().complete_goal())
                .await
                .unwrap_or_else(|error| {
                    tool_error(format!("the goal review stopped unexpectedly: {error}"))
                })),
        }
    }

    /// Emit a `ThreadGoalUpdated` client event and render the tool reply for a
    /// state mutation, or surface the validation error as a tool error.
    async fn respond_with_goal(
        &self,
        result: Result<thread_goal::ThreadGoalSnapshot, String>,
    ) -> CallToolResult {
        match result {
            Ok(goal) => {
                self.shared.publish_goal(&goal).await;
                tool_success(serde_json::json!({ "goal": goal_json(goal) }))
            }
            Err(err) => tool_error(err),
        }
    }
}

impl NoriClientShared {
    async fn publish_goal(&self, goal: &thread_goal::ThreadGoalSnapshot) {
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(nori_protocol::SessionEvent::Nori(
                nori_protocol::NoriEvent::GoalChanged(Some(goal.clone().into_client_goal())),
            )))
            .await;
    }

    async fn notice(&self, message: String) {
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(nori_protocol::SessionEvent::Nori(
                nori_protocol::NoriEvent::Notice(nori_protocol::Notice { message }),
            )))
            .await;
    }

    /// Change the goal that was reviewed, unless the person (or the agent) changed it in the
    /// meantime: what they did wins over a verdict about what the goal used to be. Announces a
    /// change of status.
    async fn settle_goal(
        &self,
        reviewed: &thread_goal::ThreadGoalSnapshot,
        change: impl FnOnce(
            &mut thread_goal::ThreadGoalState,
            i64,
        ) -> Result<thread_goal::ThreadGoalSnapshot, String>,
    ) -> Result<thread_goal::ThreadGoalSnapshot, String> {
        let now = thread_goal::now_seconds();
        let settled = {
            let mut state = self.thread_goal_state.lock().await;
            let unchanged = state.snapshot(now).is_some_and(|current| {
                current.objective == reviewed.objective
                    && current.created_at == reviewed.created_at
                    && current.status == reviewed.status
            });
            if !unchanged {
                return Err("the goal changed while it was being reviewed, so the review was dropped; call update_goal again if the goal is still done".to_owned());
            }
            change(&mut state, now)?
        };
        if settled.status != reviewed.status {
            self.publish_goal(&settled).await;
        }
        Ok(settled)
    }

    /// `update_goal` with `complete`: the agent's word when the policy has no judge, otherwise
    /// only as far as the goal judge (see [`goal_judge`]) agrees. No lock is held while the judge
    /// runs, and a judge that cannot run accepts the agent's word, marked unverified, so nothing
    /// waits on it for good.
    async fn complete_goal(self) -> CallToolResult {
        let policy = match &self.workspace {
            Some(workspace) => workspace.workflow.lock().await.goals.clone(),
            None => nori_workflow::GoalsPolicy::default(),
        };
        let (goal, disagreements) = {
            let state = self.thread_goal_state.lock().await;
            (
                state.snapshot(thread_goal::now_seconds()),
                state.judge_disagreements(),
            )
        };
        let Some(goal) = goal.filter(|goal| policy.judge && goal.status != GoalStatus::Complete)
        else {
            let result = {
                let mut state = self.thread_goal_state.lock().await;
                state.set_status(GoalStatus::Complete, thread_goal::now_seconds())
            };
            return match result {
                Ok(goal) => {
                    self.publish_goal(&goal).await;
                    tool_success(serde_json::json!({ "goal": goal_json(goal) }))
                }
                Err(error) => tool_error(error),
            };
        };

        if disagreements >= policy.max_disagreements {
            let paused = self
                .settle_goal(&goal, |state, now| {
                    state.set_status(GoalStatus::Paused, now)
                })
                .await;
            return match paused {
                Ok(goal) => {
                    self.notice(goal_judge::disagreement_notice(disagreements))
                        .await;
                    tool_error(
                        serde_json::json!({
                            "goal": goal_json(goal),
                            "message": goal_judge::disagreement_message(disagreements),
                        })
                        .to_string(),
                    )
                }
                Err(error) => tool_error(error),
            };
        }

        let judgement = goal_judge::judge(&self.orchestrator, &policy, &goal.objective).await;
        let settled = self
            .settle_goal(&goal, |state, now| {
                let status = match &judgement {
                    goal_judge::Judgement::Verdict(goal_judge::Verdict::Continue { .. }) => {
                        state.note_judge_disagreement();
                        return state
                            .snapshot(now)
                            .ok_or_else(|| "cannot update goal: no goal exists".to_owned());
                    }
                    goal_judge::Judgement::Verdict(goal_judge::Verdict::Done { .. })
                    | goal_judge::Judgement::Unverified { .. } => GoalStatus::Complete,
                    goal_judge::Judgement::Verdict(goal_judge::Verdict::Blocked { .. }) => {
                        GoalStatus::Blocked
                    }
                    goal_judge::Judgement::Verdict(goal_judge::Verdict::Paused { .. }) => {
                        GoalStatus::Paused
                    }
                };
                state.set_status(status, now)
            })
            .await;
        let goal = match settled {
            Ok(goal) => goal,
            Err(error) => return tool_error(error),
        };
        self.notice(judgement.notice()).await;
        let body = serde_json::json!({
            "goal": goal_json(goal),
            "message": judgement.agent_message(),
        });
        match judgement {
            goal_judge::Judgement::Verdict(goal_judge::Verdict::Continue { .. }) => {
                tool_error(body.to_string())
            }
            goal_judge::Judgement::Verdict(
                goal_judge::Verdict::Done { .. }
                | goal_judge::Verdict::Blocked { .. }
                | goal_judge::Verdict::Paused { .. },
            )
            | goal_judge::Judgement::Unverified { .. } => tool_success(body),
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
