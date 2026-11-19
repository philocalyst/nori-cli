//! The workflow's CI hooks. After a turn in which the agent pushed its branch, Nori arms a `ci`
//! monitor on the pushed commit (see [`super::monitor`]): the same watcher an agent can arm
//! itself. The workflow adds its `on-ci-fail` or `on-ci-pass` prompt, which the monitor appends to
//! the verdict, so the agent is told what CI said and what to do about it, in a turn of its own
//! when the session is idle and the turn budget allows.

use nori_workflow::HookEvent;
use nori_workflow::Workflow;

use super::*;
use monitor::source::SourceEnv;
use monitor::spec::CiFollowUp;
use monitor::spec::Limits;
use monitor::spec::Rules;
use monitor::spec::Source;
use monitor::spec::Spec;
use monitor::spec::WakeMode;
use workflow::notice;

/// The label of the monitor the workflow arms; a newer push replaces it.
const LABEL: &str = "ci";

/// Whether `workflow` has a prompt for CI's verdict, so pushes are worth watching.
pub(super) fn hooks_ci(workflow: &Workflow) -> bool {
    workflow
        .hooks
        .iter()
        .any(|hook| matches!(hook.event, HookEvent::CiFailed | HookEvent::CiPassed))
}

impl AcpBackend {
    /// The commit the checked-out branch's remote copy is at, as this machine last pushed or
    /// fetched it.
    pub(super) async fn pushed_commit(&self) -> Option<String> {
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let repo = nori_config::GitRepo::discover(&cwd)?;
            repo.tracking_commit(&repo.push_remote(), &repo.branch()?)
        })
        .await
        .ok()
        .flatten()
    }

    /// After a turn: when the agent pushed during the user's turn and the workflow has CI
    /// hooks, start watching the pushed commit. A newer push replaces the watch.
    pub(super) async fn maybe_watch_ci(&self, generation: i64) {
        if generation != self.orchestrator.admission_generation() {
            return;
        }
        let before = {
            let state = self.workflow.lock().await;
            match &state.active {
                Some(workflow) if hooks_ci(workflow) => state.pushed_before.clone(),
                Some(_) | None => return,
            }
        };
        // Without a record of where the branch was, nothing is known to have been pushed.
        let (Some(before), Some(pushed)) = (before, self.pushed_commit().await) else {
            return;
        };
        if generation != self.orchestrator.admission_generation() {
            return;
        }
        let follow_up = {
            let mut state = self.workflow.lock().await;
            if pushed == before || state.ci_watching.as_ref() == Some(&pushed) {
                return;
            }
            state.ci_watching = Some(pushed.clone());
            let Some(workflow) = state.active.clone() else {
                return;
            };
            let prompt = |event| {
                let prompts = workflow.hook_prompts(event).collect::<Vec<_>>();
                (!prompts.is_empty()).then(|| prompts.join("\n"))
            };
            let (on_failed, on_passed) = (prompt(HookEvent::CiFailed), prompt(HookEvent::CiPassed));
            // Past the workflow's turn budget its prompts are not sent; the verdict still is.
            let within_budget = state.hook_turns < state.hook_budget();
            let spent = within_budget && (on_failed.is_some() || on_passed.is_some());
            if spent {
                state.hook_turns += 1;
            }
            CiFollowUp {
                workflow: workflow.name.clone(),
                on_failed: on_failed.filter(|_| within_budget),
                on_passed: on_passed.filter(|_| within_budget),
            }
        };
        // A pass nobody asked the agent to act on does not wake it; a failure always does.
        let wake = if follow_up.on_passed.is_some() {
            WakeMode::Immediate
        } else {
            WakeMode::Boundary
        };
        let source = Source::Ci {
            sha: Some(pushed),
            branch: None,
            follow_up: Some(follow_up),
        };
        let spec = Spec {
            label: LABEL.to_owned(),
            wake,
            limits: Limits {
                timeout_seconds: source.default_timeout_seconds(),
                max_events: 50,
                batch_ms: source.default_batch_ms(),
                digest_seconds: 30,
            },
            rules: Rules::default(),
            source,
        };
        let armed = async {
            let env = SourceEnv::prepare(&spec.source, &self.orchestrator, None, &self.cwd).await?;
            self.orchestrator
                .monitors()
                .start(spec, env, true)
                .await
                .map(drop)
        }
        .await;
        if let Err(error) = armed {
            notice(&self.backend_event_tx, format!("CI watch: {error}")).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn only_a_workflow_with_a_ci_prompt_watches_pushes() {
        let hook = |event| nori_workflow::Hook {
            event,
            prompt: "p".to_owned(),
        };
        let mut workflow = Workflow::named("land");
        workflow.hooks.push(hook(HookEvent::AfterChange));
        assert_eq!(hooks_ci(&workflow), false);
        workflow.hooks.push(hook(HookEvent::CiFailed));
        assert_eq!(hooks_ci(&workflow), true);
    }
}
