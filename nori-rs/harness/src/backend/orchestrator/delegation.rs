//! Starting an actor on a task: `delegate_task`, and everything that delegates on the user's or a
//! workflow's behalf.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::acp::v1 as acp;

use super::DEFAULT_WAIT;
use super::Inner;
use super::MAX_WAIT;
use super::Orchestrator;
use super::brief;
use super::catalog_api::CATALOG_FRESH_FOR;
use super::delivery::Outgoing;
use super::delivery::wake_for;
use super::directory::ActorId;
use super::directory::ActorKind;
use super::directory::ActorView;
use super::directory::Caller;
use super::directory::Link;
use super::directory::NewActor;
use super::directory::NewTask;
use super::directory::Presence;
use super::directory::normalize_alias;
use super::program::ActorScope;
use super::program::ProgramActor;
use super::report::TaskSnapshot;
use super::spawn::SpawnPlan;
use super::task::Delivery;
use super::task::Party;
use super::task::TaskId;
use super::task::Wake;
use super::text;
use super::worktree;

/// How long a delegation waits for the child's session to open before it reports the child as
/// still starting. Agents that install themselves on first use are slower than this; the call
/// must return before an MCP client gives up on it.
const LAUNCH_ACK_WAIT: Duration = Duration::from_secs(40);

/// What kind of work a task is, which frames the child's instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(super) enum Role {
    Implementation,
    Research,
    Review,
    Design,
    Test,
    General,
}

/// Where the child works.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(super) enum Isolation {
    /// In the parent's working directory.
    #[default]
    Shared,
    /// In a new git worktree checked out at the parent's current commit.
    Worktree,
}

/// Whether the call that starts a task waits for it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(super) enum Mode {
    /// Return at once; the result arrives in a completion wake.
    #[default]
    Background,
    /// Block until the task ends or the timeout passes.
    Wait,
}

/// A history fork: the child continues a conversation prefix instead of starting a task.
pub(super) struct ForkSpec {
    pub(super) context: String,
    /// The journal range the prefix came from: session, first and last record.
    pub(super) source: Option<(String, i64, i64)>,
    pub(super) entries: Vec<crate::transcript::TranscriptLine>,
}

/// A new actor, and the task to start it on.
pub(super) struct Delegation {
    /// The first task. An actor without one starts idle, to be sent work later.
    pub(super) task: Option<String>,
    pub(super) agent: Option<String>,
    pub(super) model: Option<String>,
    pub(super) title: Option<String>,
    pub(super) role: Option<Role>,
    pub(super) isolation: Isolation,
    pub(super) mode: Mode,
    pub(super) timeout: Option<Duration>,
    pub(super) client_request_id: Option<String>,
    /// Other settings of the child's agent, by setting and value.
    pub(super) options: Vec<(String, String)>,
    pub(super) fork: Option<ForkSpec>,
    /// The session the child is announced under, when that is not this session: a provider child
    /// whose history was forked.
    pub(super) parent_view: Option<acp::SessionId>,
    pub(super) alias: Option<String>,
    /// Who asked for it, when that is not this session's own agent.
    pub(super) requester: Option<Party>,
    /// What a program restricts the actor to.
    pub(super) scope: Option<Arc<ActorScope>>,
}

impl Delegation {
    /// An actor that waits for its first message.
    pub(super) fn idle() -> Self {
        Self {
            task: None,
            ..Self::new("")
        }
    }

    pub(super) fn new(task: impl Into<String>) -> Self {
        Self {
            task: Some(task.into()),
            agent: None,
            model: None,
            title: None,
            role: None,
            isolation: Isolation::Shared,
            mode: Mode::Background,
            timeout: None,
            client_request_id: None,
            options: Vec::new(),
            fork: None,
            parent_view: None,
            alias: None,
            requester: None,
            scope: None,
        }
    }
}

impl Orchestrator {
    /// Hand `task` to `agent` (on `model`) in the background, as a delegation of this session's:
    /// how a workflow's commit agent writes the commits while the session's agent goes on.
    pub(crate) async fn delegate_background(
        &self,
        generation: i64,
        title: String,
        task: String,
        agent: String,
        model: Option<String>,
        review: bool,
    ) -> Result<(), String> {
        let _admission = self.rewind_admission().await;
        if generation != self.admission_generation() {
            return Err("The conversation was rewound before the workflow task started".to_owned());
        }
        self.inner
            .delegate_locked(Delegation {
                agent: Some(agent),
                model,
                title: Some(title),
                role: Some(if review {
                    Role::Review
                } else {
                    Role::Implementation
                }),
                ..Delegation::new(task)
            })
            .await
            .map(drop)
    }

    /// Continue a selected conversation prefix on a separate child session.
    pub(crate) async fn fork_history(
        &self,
        context: String,
        source: Option<(String, i64, i64)>,
        entries: Vec<crate::transcript::TranscriptLine>,
    ) -> Result<(), String> {
        self.inner
            .delegate_locked(Delegation {
                title: Some("History fork".to_owned()),
                fork: Some(ForkSpec {
                    context,
                    source,
                    entries,
                }),
                requester: Some(Party::User),
                ..Delegation::new("Continue from the conversation above.")
            })
            .await
            .map(drop)
    }

    /// Start the actor a conversation program names: idle, waiting for its first message.
    pub(crate) async fn spawn_configured_actor(
        &self,
        actor: ProgramActor,
    ) -> Result<ActorView, String> {
        // One alias is one actor: an alias that already names one returns it.
        if let Ok(existing) = self.resolve_actor(&actor.alias) {
            return Ok(existing);
        }
        let role = actor.role.as_deref().and_then(|role| {
            serde_json::from_value(serde_json::Value::String(role.to_owned())).ok()
        });
        self.inner
            .delegate(Delegation {
                agent: Some(actor.agent),
                model: actor.model,
                alias: Some(actor.alias.clone()),
                role,
                options: actor.options,
                scope: Some(Arc::new(actor.scope)),
                isolation: if actor.worktree {
                    Isolation::Worktree
                } else {
                    Isolation::Shared
                },
                ..Delegation::idle()
            })
            .await?;
        self.resolve_actor(&actor.alias)
    }

    /// The actor an address names, for the caller of this orchestrator.
    pub(crate) fn resolve_actor(&self, address: &str) -> Result<ActorView, String> {
        let caller = self.inner.caller()?;
        self.inner
            .directory()
            .with(|registry| {
                let resolved = registry.resolve(address, &caller)?;
                Ok(registry
                    .actor(&resolved.actor)
                    .map(|actor| actor.view(registry)))
            })
            .map_err(|error: super::directory::ResolveError| error.to_string())?
            .ok_or_else(|| format!("`{address}` no longer exists"))
    }

    /// Message an actor by address: one turn of work, tracked by the task it returns.
    pub(crate) async fn send_task(
        &self,
        address: &str,
        body: String,
        steer: bool,
    ) -> Result<TaskSnapshot, String> {
        self.inner
            .send_to_address(address, body, Mode::Background, None, steer, false)
            .await
    }

    /// Wait for a task (`task-N`) to end and return it with its result.
    pub(crate) async fn await_task(
        &self,
        task: &str,
        timeout: Duration,
    ) -> Result<TaskSnapshot, String> {
        let mut report = self.inner.wait_program(&[task.to_owned()], timeout).await?;
        report
            .tasks
            .pop()
            .ok_or_else(|| format!("`{task}` is not a task of this session"))
    }

    /// Stop a task (`task-N`) the caller started.
    pub(crate) async fn cancel_task_by_id(&self, task: &str) -> Result<TaskSnapshot, String> {
        self.inner.cancel(task).await
    }
}

/// A session id for the user's view of an actor, unique across every tree of delegations. The
/// `nori-task-` prefix marks an actor Nori manages.
fn view_id() -> acp::SessionId {
    acp::SessionId::new(format!("nori-task-{}", uuid::Uuid::new_v4()))
}

/// Aliases are one word, and cannot be mistaken for another address form.
fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.eq_ignore_ascii_case("root") {
        return Err("`root` is the user's session and cannot be an alias".to_owned());
    }
    if TaskId::from_address(alias).is_some() {
        return Err("an alias cannot look like a task id (`task-N`)".to_owned());
    }
    if !alias
        .chars()
        .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(
            "an alias is one word of letters, digits, `-`, `_` and `.`, such as `reviewer`"
                .to_owned(),
        );
    }
    Ok(())
}

impl Inner {
    /// Start a task on a new actor.
    pub(super) async fn delegate(
        self: &Arc<Self>,
        mut delegation: Delegation,
    ) -> Result<TaskSnapshot, String> {
        if let Some(view) = &self.caller_view {
            delegation.parent_view = Some(view.clone());
        }
        // Keys are local to the calling session, even though controls share one directory.
        delegation.client_request_id = delegation.client_request_id.map(|id| {
            let scope = self
                .caller_view
                .as_ref()
                .map_or_else(String::new, ToString::to_string);
            format!("{}:{scope}:{id}", scope.len())
        });
        let generation = self.generation.load(Ordering::Acquire);
        let _admission = self.admission.lock().await;
        if generation != self.generation.load(Ordering::Acquire) {
            return Err("The conversation was rewound before delegation started".to_owned());
        }
        self.delegate_locked(delegation).await
    }

    pub(super) async fn delegate_locked(
        self: &Arc<Self>,
        delegation: Delegation,
    ) -> Result<TaskSnapshot, String> {
        if self.shut_down.load(Ordering::Acquire) {
            return Err("this session is ending".to_owned());
        }
        if delegation
            .task
            .as_deref()
            .is_some_and(|task| task.trim().is_empty())
        {
            return Err("`task` is empty: say what the child should do".to_owned());
        }
        if let Some(request) = &delegation.client_request_id
            && let Some(existing) = self
                .directory()
                .with(|registry| registry.by_client_request.get(request).cloned())
        {
            return self.snapshot(&existing);
        }
        let me = self.me()?;
        let requester = delegation
            .requester
            .clone()
            .unwrap_or_else(|| Party::Actor(me.clone()));
        let alias = delegation
            .alias
            .as_deref()
            .map(normalize_alias)
            .filter(|alias| !alias.is_empty())
            .map(str::to_owned);
        if let Some(alias) = &alias {
            validate_alias(alias)?;
            if let Some(snapshot) = self
                .reuse_alias(alias, &me, &requester, &delegation)
                .await?
            {
                return Ok(snapshot);
            }
        }
        let agent = delegation
            .agent
            .clone()
            .unwrap_or_else(|| self.seed.config.active_agent.clone());
        let agent_info = crate::registry::list_available_agents()
            .into_iter()
            .find(|candidate| {
                candidate.agent_name.eq_ignore_ascii_case(agent.trim())
                    || candidate.display_name.eq_ignore_ascii_case(agent.trim())
            })
            .ok_or_else(|| {
                let known: Vec<String> = crate::registry::list_available_agents()
                    .into_iter()
                    .filter(|candidate| candidate.is_installed)
                    .map(|candidate| format!("`{}`", candidate.agent_name))
                    .collect();
                format!(
                    "there is no agent `{agent}`; the agents are {}",
                    known.join(", ")
                )
            })?;
        if !agent_info.is_installed {
            return Err(format!("{} is not installed", agent_info.display_name));
        }
        let injects_model = crate::registry::get_agent_config(&agent_info.agent_name)
            .is_ok_and(|agent| agent.supports_model_injection());
        // A model the agent is known not to offer, and cannot be forced to run, fails before an
        // agent is started for it. Known from a recent reading only: a model the agent has added
        // since is the started session's to accept.
        if let Some(model) = &delegation.model {
            let remembered =
                super::catalog::remembered(&self.seed.config.nori_home, &agent_info.agent_name);
            let recent = super::catalog::now_seconds() - remembered.refreshed_at
                < CATALOG_FRESH_FOR.as_secs() as i64;
            let offered = remembered.models;
            if recent
                && !injects_model
                && !offered.is_empty()
                && super::models::find(&offered, model).is_none()
            {
                return Err(format!(
                    "{} has no model `{model}`; it offers: {}",
                    agent_info.display_name,
                    super::models::describe(&offered)
                ));
            }
        }
        let cwd = match delegation.isolation {
            Isolation::Shared => self.seed.config.cwd.clone(),
            Isolation::Worktree => worktree::create(&self.seed.config.cwd).await?,
        };
        let mut config = (*self.seed.config).clone();
        config.active_agent = agent_info.agent_name.clone();
        config.cwd = cwd.clone();
        config.approval_policy = self.approval_policy();
        // A model is forced at spawn too, for agents that only take one that way; by its value,
        // when the agent has said which models it offers.
        if let Some(model) = &delegation.model {
            let remembered =
                super::catalog::remembered(&self.seed.config.nori_home, &agent_info.agent_name)
                    .models;
            let value = super::models::find(&remembered, model)
                .map_or_else(|| model.trim().to_owned(), |offered| offered.value.clone());
            config
                .default_models
                .insert(agent_info.agent_name.clone(), value);
        }
        let is_fork = delegation.fork.is_some();
        let spec = crate::runtime::AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: self.seed.cli_version.clone(),
            session_context: None,
            initial_context: delegation.fork.as_ref().map(|fork| fork.context.clone()),
        };
        let actor_id = ActorId::new(view_id());
        let title = brief::task_title(
            delegation.title.as_deref(),
            delegation
                .task
                .as_deref()
                .or(alias.as_deref())
                .unwrap_or("actor"),
        );
        let rev = self.generation.load(Ordering::Acquire);
        let parent = self.me()?;
        let registered = self.directory().with(|registry| {
            if let Some(alias) = &alias
                && let Some(taken) = registry.aliases.get(alias)
            {
                return Err(taken.clone());
            }
            registry.register(NewActor {
                id: actor_id.clone(),
                kind: ActorKind::Managed,
                parent: Some(parent),
                announce_parent: delegation.parent_view.clone(),
                alias: alias.clone(),
                agent: agent_info.agent_name.clone(),
                agent_name: agent_info.display_name.clone(),
                model: delegation.model.clone(),
                options: delegation.options.clone(),
                role: delegation
                    .role
                    .map(|role| format!("{role:?}").to_lowercase()),
                scope: delegation.scope.clone(),
                title: title.clone(),
                worktree: (delegation.isolation == Isolation::Worktree).then(|| cwd.clone()),
                link: Link::Starting,
                owner: Some(Arc::downgrade(self)),
            });
            let task = delegation.task.as_ref().and_then(|text| {
                registry.enqueue_task(
                    &actor_id,
                    NewTask {
                        requester: requester.clone(),
                        body: text.clone(),
                        content: None,
                        brief: Some(if is_fork {
                            text.clone()
                        } else {
                            brief::child_prompt(delegation.role, text)
                        }),
                        title,
                        wake: if is_fork || requester == Party::User {
                            Wake::Never
                        } else {
                            wake_for(delegation.mode)
                        },
                        front: false,
                        cc: None,
                    },
                    rev,
                )
            });
            if let (Some(request), Some(task)) = (&delegation.client_request_id, &task) {
                registry
                    .by_client_request
                    .insert(request.clone(), task.clone());
            }
            Ok(task)
        });
        let task_id = match registered {
            Ok(task) => task,
            Err(taken) => {
                // Another delegation took the alias between the check and now.
                return self.reuse_actor(&taken, &requester, &delegation).await;
            }
        };
        if task_id.is_some() {
            self.count_running_task();
        }
        if !is_fork {
            self.announce(&actor_id, SubagentState::Running).await;
        }
        self.directory().touch();
        self.journal_registered(&actor_id).await;
        if let Some(task) = &task_id {
            self.journal_tasks(std::slice::from_ref(task), true).await;
        }
        let started = tokio::time::Instant::now();
        let wait_budget = delegation.timeout.unwrap_or(DEFAULT_WAIT).min(MAX_WAIT);
        let launch_deadline = started
            + match delegation.mode {
                Mode::Background => LAUNCH_ACK_WAIT,
                Mode::Wait => LAUNCH_ACK_WAIT.min(wait_budget),
            };
        self.spawn_child(
            &actor_id,
            SpawnPlan::fresh(
                spec,
                delegation.model.clone(),
                delegation.options.clone(),
                delegation.fork,
                delegation.parent_view,
                agent_info.agent_name.clone(),
                delegation.scope.clone(),
            ),
        );
        // Success is only reported once the child's session is open: a child that cannot start
        // is an error with its cause, not a task that will fail later.
        // A history fork is the user's own action: the view shows it ending if it cannot start.
        if !is_fork {
            self.await_launch(
                &actor_id,
                task_id.as_ref(),
                launch_deadline,
                &agent_info.display_name,
            )
            .await?;
        }
        let mut snapshot = match &task_id {
            Some(task) => self.snapshot(task)?,
            None => self.actor_snapshot(&actor_id)?,
        };
        snapshot.outcome = Some("started");
        if snapshot.actor_state == "starting" {
            snapshot.next = Some(text::STILL_STARTING.to_owned());
            return Ok(snapshot);
        }
        let Some(task_id) = task_id else {
            return Ok(snapshot);
        };
        self.after_start(
            &task_id,
            delegation.mode,
            Some(wait_budget.saturating_sub(started.elapsed())),
            snapshot,
        )
        .await
    }

    /// An alias that names a live or ended actor gets the task as a message instead of a second
    /// child. `None` when the alias names no one.
    async fn reuse_alias(
        self: &Arc<Self>,
        alias: &str,
        me: &ActorId,
        requester: &Party,
        delegation: &Delegation,
    ) -> Result<Option<TaskSnapshot>, String> {
        let caller = Caller { id: me.clone() };
        let Some(existing) = self
            .directory()
            .with(|registry| registry.resolve(alias, &caller).ok())
        else {
            return Ok(None);
        };
        if existing.actor == *me {
            return Err(format!("`{alias}` is you; pick another alias"));
        }
        self.reuse_actor(&existing.actor, requester, delegation)
            .await
            .map(Some)
    }

    /// Give `actor` the delegation's task as a message, or just report it when there is none.
    async fn reuse_actor(
        self: &Arc<Self>,
        actor: &ActorId,
        requester: &Party,
        delegation: &Delegation,
    ) -> Result<TaskSnapshot, String> {
        let Some(body) = delegation.task.clone() else {
            let mut snapshot = self.actor_snapshot(actor)?;
            snapshot.outcome = Some("reused");
            return Ok(snapshot);
        };
        let mut snapshot = self
            .send_locked(
                actor,
                Outgoing {
                    from: requester.clone(),
                    body,
                    content: None,
                    mode: delegation.mode,
                    timeout: delegation.timeout,
                    steer: false,
                    cc_parent: false,
                },
            )
            .await?;
        snapshot.outcome = snapshot.outcome.or(Some("reused"));
        Ok(snapshot)
    }

    /// Wait for a starting child's session to open. A child that failed to start is an error with
    /// the cause, and its task is not owed to anyone: the caller has the answer.
    async fn await_launch(
        &self,
        actor: &ActorId,
        task: Option<&TaskId>,
        deadline: tokio::time::Instant,
        agent_name: &str,
    ) -> Result<(), String> {
        let mut changed = self.directory().subscribe();
        loop {
            let presence = self
                .directory()
                .with(|registry| registry.actor(actor).map(super::directory::Actor::presence));
            match presence {
                Some(Presence::Starting) => {}
                Some(Presence::Ended(outcome)) => {
                    if let Some(task) = task {
                        self.directory().with(|registry| {
                            if let Some((actor, index)) = registry.task_mut(task) {
                                actor.tasks[index].delivery = Delivery::Waived;
                            }
                        });
                        self.journal_tasks(std::slice::from_ref(task), false).await;
                    }
                    let cause = match outcome {
                        SubagentOutcome::Failed { reason } => reason.unwrap_or_default(),
                        SubagentOutcome::Completed { .. }
                        | SubagentOutcome::Cancelled
                        | SubagentOutcome::Closed => "the session ended as it started".to_owned(),
                    };
                    return Err(text::launch_failure(agent_name, &cause));
                }
                Some(Presence::Idle | Presence::Working { .. }) | None => return Ok(()),
            }
            if tokio::time::timeout_at(deadline, changed.changed())
                .await
                .is_err()
            {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod fresh_context_tests {
    use super::*;

    /// A delegated child starts with its task and nothing of the caller's history; only a
    /// history fork (the user's `/fork`, a program's `Fork`) carries a conversation prefix.
    #[test]
    fn a_delegation_inherits_no_history_unless_it_is_a_fork() {
        for delegation in [Delegation::new("task"), Delegation::idle()] {
            assert!(delegation.fork.is_none());
            assert!(delegation.parent_view.is_none());
        }
    }
}
