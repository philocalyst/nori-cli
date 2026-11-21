//! The parts of a prompt's context that belong to the session, taken when the prompt is sent.
//!
//! A prompt waits in the queue, may be refused as busy, dropped by the person or never started.
//! None of that may cost the session what it owes its next prompt: the preamble, the catalog,
//! hook output, the checkpoint, goal context and whether the workflow rules were told stay in
//! the session until a prompt is sent, and come back if the send fails before the agent has the
//! prompt.

use tokio::sync::Mutex;

use super::GoalContext;
use super::PromptContext;
use super::StaticContext;
use crate::backend::thread_goal::ThreadGoalState;
use crate::backend::workflow::WorkflowState;
use crate::normalized::session_runtime::QueuedPromptKind;

/// Where the session keeps what its next prompt carries.
pub(in crate::backend) struct SessionStores<'a> {
    /// The preamble and catalog nothing has been told yet.
    pub fixed: &'a Mutex<StaticContext>,
    /// What hooks said since a prompt last took it.
    pub hook_output: &'a Mutex<Option<String>>,
    /// The checkpoint a compaction, fork or resume left.
    pub checkpoint: &'a Mutex<Option<String>>,
    pub workflow: &'a Mutex<WorkflowState>,
    pub goals: &'a Mutex<ThreadGoalState>,
    /// The agent can reach Nori's goal tools, so goal context makes sense to it.
    pub goals_enabled: bool,
}

impl SessionStores<'_> {
    /// The context of the prompt of `kind` that is being sent: `own` (what it was admitted
    /// with) and everything the session owed it. A compaction takes nothing: it is not the
    /// conversation going on.
    pub(in crate::backend) async fn take(
        &self,
        kind: QueuedPromptKind,
        own: PromptContext,
        now: i64,
    ) -> PromptContext {
        if matches!(
            kind,
            QueuedPromptKind::Compact | QueuedPromptKind::NativeCompact
        ) {
            return own;
        }
        let mut context = own;
        if let Some(rules) = context.fixed.workflow_rules.take() {
            context.fixed.workflow_rules = self.workflow.lock().await.claim_rules(rules);
        }
        {
            let mut fixed = self.fixed.lock().await;
            context.fixed.preamble = fixed.preamble.take();
            context.fixed.catalog = fixed.catalog.take();
        }
        context.dynamic.checkpoint = self.checkpoint.lock().await.take();
        context.dynamic.hook_output = self.hook_output.lock().await.take();
        if self.goals_enabled {
            let mut goals = self.goals.lock().await;
            // While the agent's native goal loop drives, it owns goal context and Nori's
            // instructions would point at the wrong control plane.
            if !goals.ext_driven() {
                let before = goals.delivery();
                context.goal = goals
                    .take_goal_context(now)
                    .map(|text| GoalContext { text, before });
            }
        }
        context
    }

    /// Give back what `taken` carried from the session, for a prompt the agent never received.
    /// What the session collected meanwhile stays: a newer checkpoint replaces an older one,
    /// hook output keeps its order.
    pub(in crate::backend) async fn restore(&self, taken: PromptContext) {
        let PromptContext {
            fixed,
            dynamic,
            goal,
        } = taken;
        if fixed.workflow_rules.is_some() {
            self.workflow.lock().await.release_rules();
        }
        {
            let mut slot = self.fixed.lock().await;
            if let Some(preamble) = fixed.preamble {
                slot.preamble.get_or_insert(preamble);
            }
            if let Some(catalog) = fixed.catalog {
                slot.catalog.get_or_insert(catalog);
            }
        }
        if let Some(checkpoint) = dynamic.checkpoint {
            self.checkpoint.lock().await.get_or_insert(checkpoint);
        }
        if let Some(hook_output) = dynamic.hook_output {
            let mut slot = self.hook_output.lock().await;
            *slot = Some(match slot.take() {
                Some(newer) => format!("{hook_output}\n{newer}"),
                None => hook_output,
            });
        }
        if let Some(goal) = goal {
            self.goals.lock().await.restore_delivery(goal.before);
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::backend::prompt_context::DynamicContext;

    /// A session that owes everything there is to owe.
    struct Session {
        fixed: Mutex<StaticContext>,
        hook_output: Mutex<Option<String>>,
        checkpoint: Mutex<Option<String>>,
        workflow: Mutex<WorkflowState>,
        goals: Mutex<ThreadGoalState>,
    }

    impl Session {
        async fn owing() -> Self {
            let mut goals = ThreadGoalState::default();
            goals
                .set_objective("ship it".into(), None, 1)
                .expect("set the goal");
            Self {
                fixed: Mutex::new(StaticContext {
                    preamble: Some("preamble".into()),
                    catalog: Some("catalog".into()),
                    workflow_rules: None,
                }),
                hook_output: Mutex::new(Some("hook said".into())),
                checkpoint: Mutex::new(Some("checkpoint".into())),
                workflow: Mutex::new(WorkflowState::default()),
                goals: Mutex::new(goals),
            }
        }

        fn stores(&self) -> SessionStores<'_> {
            SessionStores {
                fixed: &self.fixed,
                hook_output: &self.hook_output,
                checkpoint: &self.checkpoint,
                workflow: &self.workflow,
                goals: &self.goals,
                goals_enabled: true,
            }
        }
    }

    fn admitted_with_rules() -> PromptContext {
        PromptContext {
            fixed: StaticContext {
                workflow_rules: Some("rules".into()),
                ..StaticContext::default()
            },
            ..PromptContext::default()
        }
    }

    fn goal_text(context: &PromptContext) -> String {
        context
            .goal
            .as_ref()
            .map(|goal| goal.text.clone())
            .expect("goal context")
    }

    #[tokio::test]
    async fn the_prompt_that_is_sent_takes_everything_the_session_owed() {
        let session = Session::owing().await;
        let taken = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 2)
            .await;
        assert_eq!(
            taken,
            PromptContext {
                fixed: StaticContext {
                    preamble: Some("preamble".into()),
                    catalog: Some("catalog".into()),
                    workflow_rules: Some("rules".into()),
                },
                dynamic: DynamicContext {
                    checkpoint: Some("checkpoint".into()),
                    hook_output: Some("hook said".into()),
                },
                goal: taken.goal.clone(),
            }
        );
        assert!(taken.goal.is_some());
        let next = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 3)
            .await;
        assert_eq!(next, PromptContext::default());
    }

    #[tokio::test]
    async fn a_prompt_that_is_rejected_or_dropped_takes_nothing_with_it() {
        // Admission records only the prompt's own rules in the prompt. A prompt the ledger
        // refuses as busy, or the person drops from the queue, is never sent: the session is
        // as it was, and the next prompt that is sent takes it all.
        let session = Session::owing().await;
        let refused = admitted_with_rules();
        let dropped = admitted_with_rules();
        drop((refused, dropped));
        let sent = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 2)
            .await;
        assert_eq!(sent.fixed.preamble.as_deref(), Some("preamble"));
        assert_eq!(sent.fixed.catalog.as_deref(), Some("catalog"));
        assert_eq!(sent.fixed.workflow_rules.as_deref(), Some("rules"));
        assert_eq!(sent.dynamic.checkpoint.as_deref(), Some("checkpoint"));
        assert_eq!(sent.dynamic.hook_output.as_deref(), Some("hook said"));
        assert!(sent.goal.is_some());
    }

    #[tokio::test]
    async fn a_send_that_fails_before_the_agent_has_the_prompt_gives_everything_back() {
        let session = Session::owing().await;
        let taken = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 2)
            .await;
        let goal = goal_text(&taken);
        // Output the hooks produce while the send is failing comes after what was given back.
        *session.hook_output.lock().await = Some("newer".into());
        session.stores().restore(taken).await;
        let again = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 2)
            .await;
        assert_eq!(
            again,
            PromptContext {
                fixed: StaticContext {
                    preamble: Some("preamble".into()),
                    catalog: Some("catalog".into()),
                    workflow_rules: Some("rules".into()),
                },
                dynamic: DynamicContext {
                    checkpoint: Some("checkpoint".into()),
                    hook_output: Some("hook said\nnewer".into()),
                },
                goal: again.goal.clone(),
            }
        );
        assert_eq!(goal_text(&again), goal);
    }

    #[tokio::test]
    async fn a_checkpoint_made_meanwhile_is_not_replaced_by_the_one_given_back() {
        let session = Session::owing().await;
        let taken = session
            .stores()
            .take(QueuedPromptKind::User, PromptContext::default(), 2)
            .await;
        *session.checkpoint.lock().await = Some("newer checkpoint".into());
        session.stores().restore(taken).await;
        assert_eq!(
            session.checkpoint.lock().await.as_deref(),
            Some("newer checkpoint")
        );
    }

    #[tokio::test]
    async fn the_workflow_rules_are_told_once_and_again_after_a_failed_send() {
        let session = Session::owing().await;
        let first = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 2)
            .await;
        assert_eq!(first.fixed.workflow_rules.as_deref(), Some("rules"));
        let second = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 3)
            .await;
        assert_eq!(second.fixed.workflow_rules, None);
        session.stores().restore(first).await;
        let third = session
            .stores()
            .take(QueuedPromptKind::User, admitted_with_rules(), 4)
            .await;
        assert_eq!(third.fixed.workflow_rules.as_deref(), Some("rules"));
    }

    #[tokio::test]
    async fn a_compaction_takes_nothing() {
        let session = Session::owing().await;
        for kind in [QueuedPromptKind::Compact, QueuedPromptKind::NativeCompact] {
            assert_eq!(
                session
                    .stores()
                    .take(kind, PromptContext::default(), 2)
                    .await,
                PromptContext::default()
            );
        }
        let sent = session
            .stores()
            .take(
                QueuedPromptKind::GoalContinuation,
                PromptContext::default(),
                3,
            )
            .await;
        assert_eq!(sent.dynamic.checkpoint.as_deref(), Some("checkpoint"));
    }

    #[tokio::test]
    async fn goal_context_is_only_taken_when_the_agent_can_reach_the_goal_tools() {
        let session = Session::owing().await;
        let stores = SessionStores {
            goals_enabled: false,
            ..session.stores()
        };
        let taken = stores
            .take(QueuedPromptKind::User, PromptContext::default(), 2)
            .await;
        assert_eq!(taken.goal, None);
        let later = session
            .stores()
            .take(QueuedPromptKind::User, PromptContext::default(), 3)
            .await;
        assert!(later.goal.is_some());
    }
}
