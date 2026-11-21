//! Apply the Pkl workflow policy to ACP sessions and follow-up hooks.
//! User and trusted project policies load in the background.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use nori_pkl::machine::MachineSnapshot;
use nori_workflow::HookEvent;
use nori_workflow::Workflow;
use tokio::sync::OnceCell;
use tokio::sync::RwLock;
use tokio::sync::watch;

use super::*;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;
use session_runtime_driver::CompletedTurn;

/// A workflow without an explicit limit may continue producing hook turns.
const DEFAULT_HOOK_TURNS: i64 = i64::MAX;

/// One session's workflow state.
#[derive(Default)]
pub(crate) struct WorkflowState {
    /// The workflow in force for this session.
    pub(super) active: Option<Workflow>,
    /// How failed requests are retried and where the session falls back to. Children carry the
    /// user's policy too, even though they run no workflow of their own.
    pub(super) recovery: nori_workflow::RecoveryPolicy,
    /// When the session compacts itself and how a refused checkpoint is cut down. Kept apart
    /// from `active`, which a child does not run.
    pub(super) compaction: nori_workflow::CompactionPolicy,
    /// What happens when the agent repeats one tool call.
    pub(super) loops: nori_workflow::LoopPolicy,
    /// Whether an independent reviewer checks a goal the agent calls complete.
    pub(super) goals: nori_workflow::GoalsPolicy,
    /// The agent has been told the active workflow's rules: a prompt that carried them was sent.
    told: bool,
    /// Hook turns sent since the user's last turn.
    pub(super) hook_turns: i64,
    /// The `on-stop` hook already ran since the user's last turn.
    stop_hooked: bool,
    /// The working tree when the user's current turn started.
    before: Option<TreeState>,
    /// The commit the agent was last given the `on-pr` hook for, by `pull_request`.
    pr_held_at: Option<String>,
    /// Where the branch's remote copy was when the user's current turn started.
    pub(super) pushed_before: Option<String>,
    /// The pushed commit whose checks are being watched, or were last reported.
    pub(super) ci_watching: Option<String>,
}

/// Always release prompt waiters, including if the background task is cancelled.
struct PolicyLoadCompletion(watch::Sender<bool>);
impl Drop for PolicyLoadCompletion {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// What the forge tools apply of the workflow in force.
pub(crate) struct PrPolicy {
    pub(crate) workflow: String,
    pub(crate) rules: nori_workflow::PrRules,
    /// The `on-pr` prompts, joined; empty when the workflow has none.
    pub(crate) before_pr: String,
}

/// What a turn could have changed: HEAD, and every changed file with its size and mtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeState {
    head: Option<String>,
    files: Vec<(String, i64, i128)>,
}

fn tree_state(cwd: &Path) -> Option<TreeState> {
    let repo = nori_config::GitRepo::discover(cwd)?;
    let root = repo.work_dir()?;
    let files = repo
        .changed_files()
        .into_iter()
        .map(|file| {
            let meta = std::fs::metadata(root.join(&file.path)).ok();
            let len = meta
                .as_ref()
                .map_or(-1, |meta| i64::try_from(meta.len()).unwrap_or(i64::MAX));
            let modified = meta
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(-1, |since| i128::try_from(since.as_nanos()).unwrap_or(-1));
            (file.path, len, modified)
        })
        .collect();
    Some(TreeState {
        head: repo.head_commit(),
        files,
    })
}

async fn tree_state_of(cwd: PathBuf) -> Option<TreeState> {
    tokio::task::spawn_blocking(move || tree_state(&cwd))
        .await
        .ok()
        .flatten()
}

/// Resolve the full policy before the agent receives its session tool catalog.
pub(super) async fn prepare_workflow(
    home: &Path,
    cwd: &Path,
    trusted: bool,
    child: bool,
    transcript_path: Option<PathBuf>,
) -> anyhow::Result<(Arc<Mutex<WorkflowState>>, Arc<OnceCell<MachineSnapshot>>)> {
    let global = nori_pkl::global_policy_path(home);
    let project = trusted.then(|| nori_pkl::project_policy_path(cwd));
    // Tokio waits for its blocking pool at runtime shutdown. A user-owned Pkl
    // resource may block indefinitely, so the detached worker must not use it.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    drop(
        std::thread::Builder::new()
            .name("nori-pkl-policy".into())
            .spawn(move || {
                let snapshot = transcript_path
                    .and_then(|path| {
                        let log =
                            nori_conversation::ConversationLog::open(path.with_extension("pkl"))
                                .ok()?;
                        let newest = *log
                            .visible_named(nori_conversation::Kind::Command, "pkl-machine-snapshot")
                            .last()?;
                        match &log.record(newest).ok()?.event {
                            nori_conversation::Event::Command { payload, .. } => {
                                serde_json::from_str(payload).ok()
                            }
                            _ => None,
                        }
                    })
                    .unwrap_or_else(MachineSnapshot::capture);
                let result = nori_pkl::load_policy_with_machine(
                    Some(&global),
                    project.as_deref(),
                    &snapshot,
                );
                let _ = sender.send((result, snapshot));
            })?,
    );
    let (loaded, snapshot) = receiver
        .await
        .map_err(|error| anyhow::anyhow!("Workflow policy loader stopped: {error}"))?;
    let (active, recovery, compaction, loops, goals) = match loaded {
        Ok(policy) if child => (
            policy.tools.map(|tools| Workflow {
                tools: Some(tools),
                ..Default::default()
            }),
            policy.recovery,
            policy.compaction,
            policy.loops,
            policy.goals,
        ),
        Ok(policy) => (
            Some(policy.clone()),
            policy.recovery,
            policy.compaction,
            policy.loops,
            policy.goals,
        ),
        Err(error) => return Err(anyhow::anyhow!("Workflow policy failed to load: {error}")),
    };
    let workflow = Arc::new(Mutex::new(WorkflowState {
        active,
        recovery,
        compaction,
        loops,
        goals,
        ..Default::default()
    }));
    let machine = Arc::new(OnceCell::new_with(Some(snapshot)));
    Ok((workflow, machine))
}

/// Persist the exact snapshot used to evaluate this session's policy.
pub(super) async fn record_prepared_snapshot(
    recorder: Option<&Arc<crate::transcript::TranscriptRecorder>>,
    snapshot: &OnceCell<MachineSnapshot>,
) {
    if let (Some(recorder), Some(snapshot)) = (recorder, snapshot.get())
        && (!recorder.continues_recorded_session()
            || recorder
                .recorded_command_payload("pkl-machine-snapshot")
                .await
                .is_none())
        && let Ok(payload) = serde_json::to_string(snapshot)
        && let Err(error) = recorder
            .record_command("pkl-machine-snapshot", payload)
            .await
    {
        tracing::warn!(%error, "Failed to record Pkl machine snapshot");
    }
}

impl WorkflowState {
    /// A policy loaded in the background does not replace an already active policy.
    fn install_default(&mut self, workflow: Option<Workflow>) {
        if self.active.is_none() {
            self.active = workflow;
        }
    }

    /// Hand `rules` to the prompt that is being sent, once: the agent is told them by the first
    /// prompt that goes out, not by one that was only admitted.
    pub(super) fn claim_rules(&mut self, rules: String) -> Option<String> {
        if self.told {
            return None;
        }
        self.told = true;
        Some(rules)
    }

    /// The prompt that claimed the rules never reached the agent: tell them again.
    pub(super) fn release_rules(&mut self) {
        self.told = false;
    }

    /// The pull request rules and `on-pr` hook of the workflow in force.
    pub(crate) fn pr_policy(&self) -> Option<PrPolicy> {
        let workflow = self.active.as_ref()?;
        Some(PrPolicy {
            workflow: workflow.name.clone(),
            rules: workflow.pr.clone().unwrap_or_default(),
            before_pr: workflow
                .hook_prompts(HookEvent::BeforePr)
                .collect::<Vec<_>>()
                .join("\n"),
        })
    }

    /// Whether `pull_request` holds back at `head` to give the agent the `on-pr` hook: once per
    /// commit, so the call after the agent has acted on it goes through.
    pub(crate) fn hold_pr(&mut self, head: &str) -> bool {
        if self.pr_held_at.as_deref() == Some(head) {
            return false;
        }
        self.pr_held_at = Some(head.to_owned());
        true
    }

    pub(super) fn hook_budget(&self) -> i64 {
        self.active
            .as_ref()
            .and_then(|workflow| workflow.max_turns)
            .unwrap_or(DEFAULT_HOOK_TURNS)
    }
}

impl AcpBackend {
    /// The recovery policy programs run under, once the background policy load is done.
    pub(crate) async fn program_recovery(&self) -> nori_workflow::RecoveryPolicy {
        let mut ready = self.workflow_ready.subscribe();
        while !*ready.borrow_and_update() {
            if ready.changed().await.is_err() {
                break;
            }
        }
        self.workflow.lock().await.recovery.clone()
    }

    /// This session's Nori home, which a world's own home is seeded from.
    pub(crate) fn program_nori_home(&self) -> &Path {
        &self.nori_home
    }

    /// The session's working directory: where programs keep `.nori/runs/` and act.
    pub(crate) fn program_cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) async fn program_machine_properties(
        &self,
    ) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
        let snapshot = get_machine_snapshot(
            Arc::clone(&self.machine_snapshot),
            self.transcript_recorder_cell(),
        )
        .await?;
        Ok(snapshot.properties())
    }
    /// Agents that cannot receive Nori tools may start while policy loads.
    pub(super) fn load_workflow_without_tools(&self) {
        let completion = PolicyLoadCompletion(self.workflow_ready.clone());
        let home = self.nori_home.clone();
        let cwd = self.cwd.clone();
        let trusted = self.orchestrator.project_trusted();
        let child = self.orchestrator.is_child();
        let workflow = Arc::clone(&self.workflow);
        let machine = Arc::clone(&self.machine_snapshot);
        let recorder = self.transcript_recorder_cell();
        let events = self.backend_event_tx.clone();
        tokio::spawn(async move {
            let _completion = completion;
            let existing_path = recorder
                .read()
                .await
                .as_ref()
                .filter(|recorder| recorder.continues_recorded_session())
                .map(|recorder| recorder.transcript_path().to_path_buf());
            match prepare_workflow(&home, &cwd, trusted, child, existing_path).await {
                Ok((prepared, snapshot)) => {
                    workflow
                        .lock()
                        .await
                        .install_default(prepared.lock().await.active.clone());
                    if let Some(snapshot) = snapshot.get() {
                        let _ = machine.set(snapshot.clone());
                    }
                    let recorder = recorder.read().await.clone();
                    record_prepared_snapshot(recorder.as_ref(), &machine).await;
                }
                Err(error) => notice(&events, format!("Workflow policy: {error}")).await,
            }
        });
    }

    /// Apply active policy instructions to the first prompt.
    pub(super) async fn workflow_for_prompt(&self, prompt_text: &str) -> (String, Option<String>) {
        apply_policy_for_prompt(&self.workflow, &self.workflow_ready, prompt_text).await
    }

    /// A user turn is starting: note the working tree and what is pushed, to tell afterwards
    /// what the turn changed.
    pub(super) async fn workflow_turn_started(&self) {
        let Some(workflow) = self.workflow.lock().await.active.clone() else {
            return;
        };
        let before = tree_state_of(self.cwd.clone()).await;
        let pushed = if super::ci_watch::hooks_ci(&workflow) {
            self.pushed_commit().await
        } else {
            None
        };
        let mut state = self.workflow.lock().await;
        state.before = before;
        state.pushed_before = pushed;
    }

    /// After a turn, send the prompts the workflow hooks to what happened, if the session is idle
    /// and the turn budget allows.
    pub(super) async fn maybe_submit_workflow_hook(
        &self,
        completed_turn: &CompletedTurn,
        generation: i64,
    ) {
        if completed_turn.stop_reason != acp::StopReason::EndTurn
            || generation != self.orchestrator.admission_generation()
        {
            return;
        }
        let (workflow, before, stop_hooked) = {
            let state = self.workflow.lock().await;
            let Some(workflow) = state.active.clone() else {
                return;
            };
            if (workflow.hooks.is_empty() && workflow.commit_reviewer.is_none())
                || state.hook_turns >= state.hook_budget()
            {
                return;
            }
            (workflow, state.before.clone(), state.stop_hooked)
        };
        // Without a snapshot from the turn's start (the workflow loaded during the turn, or this
        // is not a git repository) nothing is known to have changed.
        let after = tree_state_of(self.cwd.clone()).await;
        if generation != self.orchestrator.admission_generation() {
            return;
        }
        let (changed, committed) = match (&before, &after) {
            (Some(before), Some(after)) => (before != after, before.head != after.head),
            _ => (false, false),
        };
        if changed
            && matches!(completed_turn.prompt.kind, QueuedPromptKind::User)
            && let Some(reviewer) = &workflow.commit_reviewer
        {
            self.workflow.lock().await.hook_turns += 1;
            let mut guidance = Workflow::named(&workflow.name);
            guidance.commit = workflow.commit.clone();
            if let Some(commit) = &mut guidance.commit {
                commit.agent = None;
            }
            let task = format!(
                "Review the current working-tree diff, including staged, unstaged, and untracked changes. Identify substantive problems and propose appropriately scoped commit messages for the changes. State what you reviewed and any limitations. Do not modify files, stage changes, or create commits. The working tree may include changes predating this turn; do not assume all changes belong to this task.\n\nCommit message guidance:\n{}",
                guidance.instructions()
            );
            if let Err(error) = self
                .orchestrator
                .delegate_background(
                    generation,
                    "Review changes and draft commit messages".to_owned(),
                    task,
                    reviewer.agent.clone(),
                    reviewer.model.clone(),
                    true,
                )
                .await
            {
                notice(&self.backend_event_tx, format!("Commit reviewer: {error}")).await;
            }
            let state = self.workflow.lock().await;
            if state.hook_turns >= state.hook_budget() {
                return;
            }
        }
        let events: &[HookEvent] = match completed_turn.prompt.kind {
            // Hooks answer what the user's turn did; a hook turn's own commit may still be
            // followed up, but its edits never start another round.
            QueuedPromptKind::User
            | QueuedPromptKind::TaskResults
            | QueuedPromptKind::MonitorEvents => &[
                HookEvent::AfterChange,
                HookEvent::AfterCommit,
                HookEvent::AfterTurn,
                HookEvent::BeforeStop,
            ],
            QueuedPromptKind::WorkflowHook => &[HookEvent::AfterCommit, HookEvent::BeforeStop],
            QueuedPromptKind::GoalContinuation
            | QueuedPromptKind::Compact
            | QueuedPromptKind::NativeCompact => return,
        };
        let Some(event) = events.iter().copied().find(|event| {
            let happened = match event {
                HookEvent::AfterChange => changed,
                HookEvent::AfterCommit => committed,
                HookEvent::AfterTurn => true,
                // The last word before control returns to the user, once per user turn.
                HookEvent::BeforeStop => !stop_hooked,
                // `pull_request` gives the agent this one; CI's come from its watcher.
                HookEvent::BeforePr | HookEvent::CiFailed | HookEvent::CiPassed => false,
            };
            happened && workflow.hook_prompts(*event).next().is_some()
        }) else {
            return;
        };
        let prompt = workflow.hook_prompts(event).collect::<Vec<_>>().join("\n");

        // A designated commit agent writes the commits, so the session's agent keeps going.
        if event == HookEvent::AfterChange
            && let Some(agent) = workflow
                .commit
                .as_ref()
                .and_then(|commit| commit.agent.clone())
        {
            self.workflow.lock().await.hook_turns += 1;
            let task = format!("{prompt}\n\n{}", workflow.instructions());
            if let Err(error) = self
                .orchestrator
                .delegate_background(
                    generation,
                    "Commit the changes".to_owned(),
                    task,
                    agent.agent,
                    agent.model,
                    false,
                )
                .await
            {
                notice(&self.backend_event_tx, format!("Commit agent: {error}")).await;
            }
            return;
        }

        if self.session_driver.lock().await.prompt_is_busy() {
            return;
        }
        // The hook is spent only if the ledger let its turn start.
        if self
            .send_hook_turn(
                generation,
                format!("Workflow \"{}\" · {}", workflow.name, event.form()),
                event,
                prompt,
            )
            .await
        {
            let mut state = self.workflow.lock().await;
            state.hook_turns += 1;
            state.stop_hooked |= event == HookEvent::BeforeStop;
        }
    }

    /// Offer `body` to the ledger as the hidden turn of the workflow hook `event`, announced by
    /// an info line once it has started. Returns whether the turn started.
    pub(super) async fn send_hook_turn(
        &self,
        generation: i64,
        label: String,
        event: HookEvent,
        body: String,
    ) -> bool {
        let text = format!(
            "<workflow_hook event=\"{}\">\n{body}\n</workflow_hook>",
            event.form()
        );
        let started = self
            .offer_prompt(
                Some(generation),
                QueuedPrompt {
                    event_id: format!("workflow-hook-{}", uuid::Uuid::new_v4()),
                    kind: QueuedPromptKind::WorkflowHook,
                    text: text.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                    context: Default::default(),
                    display_text: None,
                },
            )
            .await;
        if started {
            // Routine, so an info line rather than a warning.
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    nori_protocol::NoriEvent::HookOutput(nori_protocol::HookOutput {
                        message: label,
                        level: nori_protocol::HookOutputLevel::Info,
                    }),
                )))
                .await;
        }
        started
    }
}

async fn apply_policy_for_prompt(
    workflow: &tokio::sync::Mutex<WorkflowState>,
    ready: &watch::Sender<bool>,
    prompt_text: &str,
) -> (String, Option<String>) {
    let mut ready = ready.subscribe();
    while !*ready.borrow_and_update() {
        if ready.changed().await.is_err() {
            break;
        }
    }
    let mut state = workflow.lock().await;
    state.hook_turns = 0;
    state.stop_hooked = false;
    // The rules are this prompt's own; whether the agent still has to be told them is decided
    // when a prompt is sent (`WorkflowState::claim_rules`), so a prompt that never goes out does
    // not use them up.
    let rules = state
        .active
        .as_ref()
        .filter(|_| !state.told)
        .map(Workflow::instructions)
        .filter(|rules| !rules.is_empty())
        .map(|rules| format!("<workflow>\n{rules}</workflow>"));
    (prompt_text.to_owned(), rules)
}

async fn get_machine_snapshot(
    cell: Arc<OnceCell<MachineSnapshot>>,
    recorder_cell: Arc<RwLock<Option<Arc<crate::transcript::TranscriptRecorder>>>>,
) -> anyhow::Result<MachineSnapshot> {
    Ok(cell
        .get_or_try_init(|| async {
            let recorder = recorder_cell.read().await.clone();
            if let Some(recorder) = &recorder
                && recorder.continues_recorded_session()
                && let Some(payload) = recorder
                    .recorded_command_payload("pkl-machine-snapshot")
                    .await
            {
                return serde_json::from_str(&payload)
                    .map_err(anyhow::Error::from)
                    .map_err(|error| error.context("read recorded Pkl machine snapshot"));
            }

            let snapshot = MachineSnapshot::capture();
            if let Some(recorder) = recorder {
                recorder
                    .record_command("pkl-machine-snapshot", serde_json::to_string(&snapshot)?)
                    .await?;
            }
            Ok(snapshot)
        })
        .await?
        .clone())
}

pub(super) async fn notice(events: &mpsc::Sender<BackendEvent>, message: String) {
    let _ = events
        .send(BackendEvent::Public(SessionEvent::Nori(
            nori_protocol::NoriEvent::Notice(nori_protocol::Notice { message }),
        )))
        .await;
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn prepared_policy_drives_catalog_and_first_prompt_from_one_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("init.pkl"),
            r#"
            instructions = List("Policy prompt")
            tools = new Dynamic {
                enabled = List("get_goal")
                instructions = List("Catalog prompt")
            }
        "#,
        )
        .unwrap();
        let (state, snapshot) = prepare_workflow(home.path(), cwd.path(), false, false, None)
            .await
            .unwrap();
        let policy = state.lock().await.active.clone().unwrap();
        assert_eq!(
            policy.tools.unwrap().enabled,
            Some(vec!["get_goal".to_owned()])
        );
        assert!(snapshot.get().is_some());
        let (ready, _) = watch::channel(true);
        let (_, rules) = apply_policy_for_prompt(&state, &ready, "hello").await;
        assert!(rules.unwrap().contains("Policy prompt"));
    }

    #[tokio::test]
    async fn admitting_a_prompt_does_not_use_up_the_rules_only_sending_it_does() {
        let mut policy = Workflow::named("project");
        policy.instructions.push("Check the tests".into());
        let state = tokio::sync::Mutex::new(WorkflowState {
            active: Some(policy),
            ..Default::default()
        });
        let (ready, _) = watch::channel(true);
        let (_, admitted) = apply_policy_for_prompt(&state, &ready, "one").await;
        let (_, admitted_again) = apply_policy_for_prompt(&state, &ready, "two").await;
        assert_eq!(admitted, admitted_again);
        let rules = admitted.expect("rules");
        assert_eq!(
            state.lock().await.claim_rules(rules.clone()),
            Some(rules.clone())
        );
        assert_eq!(state.lock().await.claim_rules(rules.clone()), None);
        assert_eq!(
            apply_policy_for_prompt(&state, &ready, "three").await.1,
            None
        );
        state.lock().await.release_rules();
        assert_eq!(state.lock().await.claim_rules(rules.clone()), Some(rules));
    }

    #[tokio::test]
    async fn invalid_configured_policy_cannot_expose_an_unfiltered_catalog() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("init.pkl"),
            "tools = new Dynamic { unknown = true }",
        )
        .unwrap();
        assert!(
            prepare_workflow(home.path(), cwd.path(), false, false, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn first_prompt_waits_for_policy_instructions() {
        let workflow = Arc::new(tokio::sync::Mutex::new(WorkflowState::default()));
        let (ready, _) = watch::channel(false);
        let prompt = {
            let workflow = Arc::clone(&workflow);
            let ready = ready.clone();
            tokio::spawn(async move { apply_policy_for_prompt(&workflow, &ready, "hello").await })
        };
        tokio::task::yield_now().await;
        assert!(!prompt.is_finished());
        let mut policy = Workflow::named("project");
        policy.instructions.push("Check the tests".into());
        workflow.lock().await.install_default(Some(policy));
        ready.send_replace(true);
        let (text, rules) = prompt.await.unwrap();
        assert_eq!(text, "hello");
        assert!(rules.unwrap().contains("Check the tests"));
    }

    #[tokio::test]
    async fn cancellation_releases_waiting_prompt() {
        let workflow = tokio::sync::Mutex::new(WorkflowState::default());
        let (ready, _) = watch::channel(false);
        let completion = PolicyLoadCompletion(ready.clone());
        let task = tokio::spawn(async move {
            let _completion = completion;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert_eq!(
            apply_policy_for_prompt(&workflow, &ready, "hello").await,
            ("hello".into(), None)
        );
    }

    #[test]
    fn late_default_load_does_not_replace_an_active_policy() {
        let active = Workflow::named("active");
        let mut state = WorkflowState {
            active: Some(active.clone()),
            ..Default::default()
        };
        state.install_default(Some(Workflow::named("default")));
        assert_eq!(state.active, Some(active));
    }

    #[test]
    fn a_tree_state_sees_edits_to_an_already_changed_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("a.txt"), "a\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        assert_eq!(tree_state(dir).map(|state| state.files), Some(Vec::new()));

        std::fs::write(dir.join("a.txt"), "b\n").expect("edit");
        let first = tree_state(dir).expect("state");
        std::fs::write(dir.join("a.txt"), "bigger\n").expect("edit again");
        let second = tree_state(dir).expect("state");
        assert_ne!(first, second, "the same file changed again");
        assert_eq!(first.head, second.head);
    }

    #[test]
    fn the_hook_budget_defaults_when_the_workflow_does_not_say() {
        let mut state = WorkflowState {
            active: Some(Workflow::named("land")),
            ..WorkflowState::default()
        };
        assert_eq!(state.hook_budget(), DEFAULT_HOOK_TURNS);
        state.active = Some(Workflow {
            max_turns: Some(1),
            ..Workflow::named("land")
        });
        assert_eq!(state.hook_budget(), 1);
    }
}
