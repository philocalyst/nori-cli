use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::*;
use crate::engine::ScriptEngine;
use crate::error::ErrorKind;
use crate::error::Origin;
use crate::error::ScriptError;

fn origin() -> Origin {
    Origin::File(PathBuf::from("init.scm"))
}

fn run(source: &str) -> std::result::Result<(Registry, Option<Workflow>), ScriptError> {
    let mut engine = ScriptEngine::new().map_err(|failure| failure.locate(origin(), ""))?;
    let values = engine
        .run(source)
        .map_err(|failure| failure.locate(origin(), source))?;
    Ok((engine.registry(), values.last().and_then(workflow_of)))
}

/// The workflow `source` evaluates to.
fn eval(source: &str) -> Workflow {
    match run(source) {
        Ok((_, Some(workflow))) => workflow,
        Ok((_, None)) => panic!("{source} did not evaluate to a workflow"),
        Err(error) => panic!("{error}"),
    }
}

/// The error `source` fails with.
fn fail(source: &str) -> ScriptError {
    match run(source) {
        Ok(_) => panic!("{source} unexpectedly succeeded"),
        Err(error) => error,
    }
}

/// An evaluation error; positions point at the failing form's name.
fn eval_error(line: i64, column: i64, message: &str) -> ScriptError {
    ScriptError {
        origin: origin(),
        kind: ErrorKind::Eval,
        line: Some(line),
        column: Some(column),
        message: message.to_string(),
    }
}

#[test]
fn the_fluent_chain_reads_like_the_builder_api() {
    assert_eq!(
        eval(r#"(~> (workflow "land") (turns 3) (hook-after-change "Commit it."))"#),
        Workflow {
            max_turns: Some(3),
            hooks: vec![Hook {
                event: HookEvent::AfterChange,
                prompt: "Commit it.".to_string(),
            }],
            ..Workflow::named("land")
        }
    );
}

#[test]
fn commit_review_designates_an_agent_without_changing_commit_rules() {
    let workflow = eval(
        r#"(~> (workflow "w") (commit-style 'conventional) (commit-review 'claude #:model "haiku"))"#,
    );
    assert_eq!(
        workflow,
        Workflow {
            commit: Some(CommitRules {
                style: Some(CommitStyle::Conventional),
                ..CommitRules::default()
            }),
            commit_reviewer: Some(CommitAgent {
                agent: "claude".to_owned(),
                model: Some("haiku".to_owned()),
            }),
            ..Workflow::named("w")
        }
    );
    assert!(
        workflow
            .instructions()
            .contains("draft commit messages without committing")
    );
    assert!(
        fail(r#"(~> (workflow "w") (commit-review 'claude #:model 1))"#)
            .message
            .contains("commit-review")
    );
}

#[test]
fn fallback_models_are_ordered_replaced_and_validated() {
    assert_eq!(
        eval(r#"(~> (workflow "w") (fallback-models "a") (fallback-ranked-models))"#),
        Workflow::named("w")
    );
    assert_eq!(
        eval(
            r#"(~> (workflow "w") (fallback-models "provider/first" "second") (fallback-models "third" "provider/fourth"))"#
        ),
        {
            let mut expected = Workflow::named("w");
            expected.recovery.base.fallback.chain =
                ["third", "provider/fourth"].map(Route::parse_legacy).into();
            expected.recovery.base.fallback.ranked = false;
            expected
        }
    );
    assert_eq!(
        eval(r#"(~> (workflow "w") (fallback-models "a") (fallback-models))"#),
        {
            let mut expected = Workflow::named("w");
            expected.recovery.base.fallback.ranked = false;
            expected
        }
    );
    for source in [
        r#"(~> (workflow "w") (fallback-models ""))"#,
        r#"(~> (workflow "w") (fallback-models "has space"))"#,
        r#"(~> (workflow "w") (fallback-models "a" "a"))"#,
        r#"(~> (workflow "w") (fallback-models 'model))"#,
    ] {
        assert!(fail(source).message.contains("fallback-models"));
    }
}

#[test]
fn define_workflow_registers_and_binds_the_name() {
    let (registry, _) = run(r#"
(define-workflow land
  (turns 3)
  (on-change "Commit what you just changed as focused commits; explain the why.")
  (commit-style 'conventional #:max-files 8 #:atomic #t)
  (commit-agent 'claude #:model "haiku")
  (pr #:title "<type>(<scope>): <summary>"
      #:sections '("Why" "What" "Testing")
      #:max-lines 400
      #:upstream-when 'tests-pass))
(default-workflow land)
"#)
    .expect("loads");
    let land = Workflow {
        max_turns: Some(3),
        hooks: vec![Hook {
            event: HookEvent::AfterChange,
            prompt: "Commit what you just changed as focused commits; explain the why.".to_string(),
        }],
        commit: Some(CommitRules {
            style: Some(CommitStyle::Conventional),
            max_files: Some(8),
            atomic: true,
            agent: Some(CommitAgent {
                agent: "claude".to_string(),
                model: Some("haiku".to_string()),
            }),
            ..CommitRules::default()
        }),
        pr: Some(PrRules {
            title: Some("<type>(<scope>): <summary>".to_string()),
            sections: vec!["Why".to_string(), "What".to_string(), "Testing".to_string()],
            max_lines: Some(400),
            upstream: Some(Upstream::TestsPass),
            ..PrRules::default()
        }),
        ..Workflow::named("land")
    };
    assert_eq!(
        registry,
        Registry {
            workflows: vec![land],
            default: Some("land".to_string()),
        }
    );
}

#[test]
fn every_hook_form_and_alias_maps_to_its_event() {
    let source = r#"
(~> (workflow 'hooks)
    (on-change "a") (on-turn "b") (on-commit "c") (on-pr "d") (on-stop "e")
    (hook-after-change "f") (hook-after-turn "g") (hook-after-commit "h")
    (hook-before-pr "i") (hook-before-stop "j")
    (on-ci-fail "k") (on-ci-pass "l") (hook-after-ci-fail "m") (hook-after-ci-pass "n"))
"#;
    let hook = |event, prompt: &str| Hook {
        event,
        prompt: prompt.to_string(),
    };
    assert_eq!(
        eval(source).hooks,
        vec![
            hook(HookEvent::AfterChange, "a"),
            hook(HookEvent::AfterTurn, "b"),
            hook(HookEvent::AfterCommit, "c"),
            hook(HookEvent::BeforePr, "d"),
            hook(HookEvent::BeforeStop, "e"),
            hook(HookEvent::AfterChange, "f"),
            hook(HookEvent::AfterTurn, "g"),
            hook(HookEvent::AfterCommit, "h"),
            hook(HookEvent::BeforePr, "i"),
            hook(HookEvent::BeforeStop, "j"),
            hook(HookEvent::CiFailed, "k"),
            hook(HookEvent::CiPassed, "l"),
            hook(HookEvent::CiFailed, "m"),
            hook(HookEvent::CiPassed, "n"),
        ]
    );
}

#[test]
fn turns_and_instruct() {
    assert_eq!(
        eval(
            r#"(~> (workflow "w") (turns 0) (instruct "Never edit vendor/.") (instruct "Ask first."))"#
        ),
        Workflow {
            max_turns: Some(0),
            instructions: vec!["Never edit vendor/.".to_string(), "Ask first.".to_string()],
            ..Workflow::named("w")
        }
    );
}

#[test]
fn commit_style_sets_every_option_and_merges_later_calls() {
    let workflow = eval(
        r#"
(~> (workflow "w")
    (commit-style 'plain #:max-files 3 #:max-subject 50 #:atomic #t
                  #:batch 'per-turn #:require-body #t)
    (commit-style #:max-files 5 #:batch 'per-task)
    (commit-style 'conventional))
"#,
    );
    assert_eq!(
        workflow.commit,
        Some(CommitRules {
            style: Some(CommitStyle::Conventional),
            max_files: Some(5),
            max_subject: Some(50),
            atomic: true,
            batch: Some(CommitBatch::PerTask),
            require_body: true,
            agent: None,
        })
    );
}

#[test]
fn commit_agent_and_commit_style_keep_each_other() {
    let workflow = eval(
        r#"(~> (workflow "w") (commit-agent "codex") (commit-style 'conventional #:batch 'per-change))"#,
    );
    assert_eq!(
        workflow.commit,
        Some(CommitRules {
            style: Some(CommitStyle::Conventional),
            batch: Some(CommitBatch::PerChange),
            agent: Some(CommitAgent {
                agent: "codex".to_string(),
                model: None,
            }),
            ..CommitRules::default()
        })
    );
}

#[test]
fn pr_sets_every_option_and_merges_later_calls() {
    let workflow = eval(
        r#"
(~> (workflow "w")
    (pr #:title "t" #:sections '("Why") #:max-lines 400 #:max-files 20
        #:base "main" #:draft #t #:upstream-when 'ask)
    (pr #:max-lines 200 #:upstream-when 'done))
"#,
    );
    assert_eq!(
        workflow.pr,
        Some(PrRules {
            title: Some("t".to_string()),
            sections: vec!["Why".to_string()],
            max_lines: Some(200),
            max_files: Some(20),
            base: Some("main".to_string()),
            draft: true,
            upstream: Some(Upstream::Done),
        })
    );
}

#[test]
fn use_copies_a_workflow_by_value_or_name_and_keeps_the_name() {
    let (registry, last) = run(r#"
(define-workflow base (turns 2) (on-stop "Run the tests."))
(define-workflow by-value (use base) (turns 5))
(~> (workflow "by-name") (use 'base))
"#)
    .expect("loads");
    let stop = vec![Hook {
        event: HookEvent::BeforeStop,
        prompt: "Run the tests.".to_string(),
    }];
    assert_eq!(
        registry.get("by-value"),
        Some(&Workflow {
            max_turns: Some(5),
            hooks: stop.clone(),
            ..Workflow::named("by-value")
        })
    );
    assert_eq!(
        last,
        Some(Workflow {
            max_turns: Some(2),
            hooks: stop,
            ..Workflow::named("by-name")
        })
    );
}

#[test]
fn re_registering_a_name_replaces_it_in_place() {
    let (registry, _) = run(r#"
(define-workflow a (turns 1))
(define-workflow b (turns 2))
(register-workflow (~> (workflow "a") (turns 9)))
(default-workflow "b")
"#)
    .expect("loads");
    assert_eq!(
        registry,
        Registry {
            workflows: vec![
                Workflow {
                    max_turns: Some(9),
                    ..Workflow::named("a")
                },
                Workflow {
                    max_turns: Some(2),
                    ..Workflow::named("b")
                },
            ],
            default: Some("b".to_string()),
        }
    );
}

#[test]
fn workflow_predicate() {
    let (_, _) = run(r#"(if (and (workflow? (workflow "w")) (not (workflow? 3))) 1 (error "no"))"#)
        .expect("predicate works");
}

#[test]
fn wrong_argument_types_name_the_form_and_the_value() {
    assert_eq!(
        fail(r#"(~> (workflow "w") (turns "3"))"#),
        eval_error(
            1,
            21,
            "turns: the turn count must be a whole number of at least 0, got \"3\""
        ),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (turns -1))"#),
        eval_error(
            1,
            21,
            "turns: the turn count must be a whole number of at least 0, got -1"
        ),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (on-stop ""))"#),
        eval_error(
            1,
            21,
            "on-stop: the prompt must be a non-empty string, got \"\""
        ),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (commit-style 'conventinal))"#),
        eval_error(
            1,
            21,
            "commit-style: the style must be one of 'conventional, 'plain, got 'conventinal"
        ),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (pr #:sections '(1 2)))"#),
        eval_error(1, 21, "pr: #:sections must be a list of strings, got (1 2)"),
    );
    assert_eq!(
        fail(r#"(turns 3 3)"#),
        eval_error(1, 2, "turns: expected a workflow, got 3"),
    );
}

#[test]
fn option_mistakes_are_reported() {
    assert_eq!(
        fail(r#"(~> (workflow "w") (pr #:max-flies 3))"#),
        eval_error(
            1,
            21,
            "pr: unknown option #:max-flies; expected one of #:title, #:sections, #:max-lines, #:max-files, #:base, #:draft, #:upstream-when"
        ),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (pr #:draft))"#),
        eval_error(1, 21, "pr: #:draft needs a value"),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (pr #:draft #t #:draft #f))"#),
        eval_error(1, 21, "pr: #:draft is given twice"),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (commit-agent 'claude "haiku"))"#),
        eval_error(
            1,
            21,
            "commit-agent: expected an option (#:model), got \"haiku\""
        ),
    );
}

#[test]
fn a_clause_outside_a_chain_explains_how_to_call_it() {
    assert_eq!(
        fail("(turns 3)"),
        eval_error(
            1,
            2,
            "turns: wrong number of arguments; write (turns n) inside define-workflow or a ~> chain"
        ),
    );
}

#[test]
fn unknown_names_are_rejected() {
    let error = fail("(define-workflow w (turnz 3))");
    assert_eq!(
        (error.kind, error.line, error.column),
        (ErrorKind::Eval, Some(1), Some(21))
    );
    assert!(error.message.contains("turnz"), "{}", error.message);
}

#[test]
fn defaults_and_uses_must_name_defined_workflows() {
    assert_eq!(
        fail(r#"(default-workflow "nope")"#),
        eval_error(
            1,
            2,
            "default-workflow: no workflow named \"nope\" is defined yet"
        ),
    );
    assert_eq!(
        fail(r#"(~> (workflow "w") (use 'nope))"#),
        eval_error(1, 21, "use: no workflow named \"nope\" is defined yet"),
    );
}
