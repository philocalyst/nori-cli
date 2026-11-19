use super::*;
use crate::model::Hook;
use crate::recovery::RecoveryPolicy;
use pretty_assertions::assert_eq;

fn land() -> Workflow {
    Workflow {
        name: "land".to_string(),
        commit_reviewer: None,
        recovery: RecoveryPolicy::default(),
        tools: None,
        max_turns: Some(3),
        hooks: vec![
            Hook {
                event: HookEvent::BeforeStop,
                prompt: "Run the tests.".to_string(),
            },
            Hook {
                event: HookEvent::AfterChange,
                prompt: "Commit what you just changed.".to_string(),
            },
            Hook {
                event: HookEvent::BeforeStop,
                prompt: "Summarize what is left.".to_string(),
            },
        ],
        commit: Some(CommitRules {
            style: Some(CommitStyle::Conventional),
            max_files: Some(8),
            max_subject: Some(72),
            atomic: true,
            batch: Some(CommitBatch::PerChange),
            require_body: true,
            agent: Some(CommitAgent {
                agent: "claude".to_string(),
                model: Some("haiku".to_string()),
            }),
        }),
        pr: Some(PrRules {
            title: Some("<type>(<scope>): <summary>".to_string()),
            sections: vec!["Why".to_string(), "What".to_string(), "Testing".to_string()],
            max_lines: Some(400),
            max_files: Some(20),
            base: Some("main".to_string()),
            draft: true,
            upstream: Some(Upstream::TestsPass),
        }),
        instructions: vec!["Never edit generated files.".to_string()],
    }
}

#[test]
fn renders_every_rule() {
    assert_eq!(
        land().instructions(),
        "\
Follow the \"land\" workflow:
- You may run up to 3 prompt turns on your own before the user takes over.
- Commit messages follow Conventional Commits: `type(scope): summary`. Keep the subject line within 72 characters. Every commit has a body that explains why the change was made.
- Keep commits atomic: one logical change per commit that builds and passes tests on its own. A commit touches at most 8 files. Commit each logical change as soon as it is complete.
- Do not run `git commit` yourself: Nori's commit agent, claude (haiku), writes and batches the commits by these rules.
- Pull request titles follow `<type>(<scope>): <summary>`. The description has these sections, in order: \"Why\", \"What\", \"Testing\". Keep each pull request within 400 changed lines and 20 files; split larger work into a series of pull requests.
- Target the `main` branch. Open pull requests as drafts. Once the tests pass, push the branch and open the pull request.
- After a turn that changes files, Nori will tell you: Commit what you just changed.
- Before you stop, Nori will tell you: Run the tests. Summarize what is left.
- Never edit generated files.
"
    );
}

#[test]
fn renders_only_what_is_set() {
    let workflow = Workflow {
        commit: Some(CommitRules {
            style: Some(CommitStyle::Plain),
            batch: Some(CommitBatch::PerTask),
            ..CommitRules::default()
        }),
        pr: Some(PrRules {
            max_files: Some(10),
            upstream: Some(Upstream::Never),
            ..PrRules::default()
        }),
        max_turns: Some(0),
        ..Workflow::named("quiet")
    };
    assert_eq!(
        workflow.instructions(),
        "\
Follow the \"quiet\" workflow:
- Stop after each prompt turn; the user decides what happens next.
- Commit subjects are a plain imperative summary.
- Leave changes uncommitted until the task is complete, then commit them together.
- Keep each pull request within 10 files; split larger work into a series of pull requests.
- Never push or open a pull request; the user upstreams the work.
"
    );
}

#[test]
fn an_empty_workflow_renders_nothing() {
    assert_eq!(Workflow::named("empty").instructions(), "");
}

#[test]
fn hook_prompts_keep_declaration_order() {
    let workflow = land();
    assert_eq!(
        workflow
            .hook_prompts(HookEvent::BeforeStop)
            .collect::<Vec<_>>(),
        vec!["Run the tests.", "Summarize what is left."]
    );
    assert_eq!(workflow.hook_prompts(HookEvent::BeforePr).count(), 0);
}
