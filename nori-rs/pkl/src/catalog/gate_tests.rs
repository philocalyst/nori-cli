//! Gate modules through the catalog: what they decode to and what validation refuses.

use std::collections::BTreeMap;

use pretty_assertions::assert_eq;

use super::Roots;
use super::load;
use crate::EvaluatorOptions;
use crate::kinds::GateDef;
use crate::kinds::GateEvent;
use crate::kinds::gate::HostToolDecision;
use crate::kinds::gate::Not;
use crate::kinds::gate::Pattern;
use crate::kinds::gate::PermissionDecision;
use crate::kinds::gate::Rule;
use crate::kinds::gate::StopDecision;
use crate::kinds::gate::ToolKind;

fn gate_catalog(source: &str) -> super::Catalog {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("gates/under-test.pkl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, source).unwrap();
    load(
        &Roots {
            user: Some(home.path().to_path_buf()),
            project: None,
        },
        &EvaluatorOptions::preconfigured(),
    )
}

fn only_problem(source: &str) -> String {
    let catalog = gate_catalog(source);
    assert_eq!(catalog.gates, vec![], "the gate must not load");
    let [problem] = catalog.problems.as_slice() else {
        panic!("one problem: {:?}", catalog.problems);
    };
    problem.message.clone()
}

#[test]
fn a_gate_of_each_event_decodes_to_the_exact_rust_value() {
    let permission = gate_catalog(
        r#"amends "nori:Gate"
name = "under-test"
description = "one of each matcher"
on = "permission"
rules {
  new PermissionRule {
    kinds { "execute"; "edit" }
    title = "^Run"
    command = new Not { regex = "^ls" }
    paths { "src/**" }
    only_paths { "docs/*.md" }
    decision = "deny"
    reason = "no"
  }
  new PermissionRule { decision = "ask" }
}
"#,
    );
    assert_eq!(permission.problems, vec![]);
    assert_eq!(
        permission.gate("under-test").unwrap().value,
        GateDef {
            name: "under-test".to_owned(),
            description: "one of each matcher".to_owned(),
            on: GateEvent::Permission,
            rules: vec![
                Rule::Permission {
                    kinds: vec![ToolKind::Execute, ToolKind::Edit],
                    title: Some(Pattern::Matches("^Run".to_owned())),
                    command: Some(Pattern::Not(Not {
                        regex: "^ls".to_owned()
                    })),
                    paths: vec!["src/**".to_owned()],
                    only_paths: vec!["docs/*.md".to_owned()],
                    decision: PermissionDecision::Deny,
                    reason: Some("no".to_owned()),
                },
                Rule::Permission {
                    kinds: vec![],
                    title: None,
                    command: None,
                    paths: vec![],
                    only_paths: vec![],
                    decision: PermissionDecision::Ask,
                    reason: None,
                },
            ],
        }
    );

    let host_tool = gate_catalog(
        r#"amends "nori:Gate"
name = "under-test"
on = "host_tool"
rules {
  new HostToolRule {
    tools { "bash" }
    arguments { ["command"] = "^git" }
    absent { "machine" }
    decision = "rewrite"
    set { ["machine"] = "h16"; ["timeout_ms"] = 5000 }
    append { ["command"] = "--dry-run" }
  }
}
"#,
    );
    assert_eq!(host_tool.problems, vec![]);
    assert_eq!(
        host_tool.gate("under-test").unwrap().value.rules,
        vec![Rule::HostTool {
            tools: vec!["bash".to_owned()],
            arguments: BTreeMap::from([(
                "command".to_owned(),
                Pattern::Matches("^git".to_owned())
            )]),
            absent: vec!["machine".to_owned()],
            decision: HostToolDecision::Rewrite,
            reason: None,
            set: BTreeMap::from([
                ("machine".to_owned(), serde_json::json!("h16")),
                ("timeout_ms".to_owned(), serde_json::json!(5000)),
            ]),
            append: BTreeMap::from([("command".to_owned(), "--dry-run".to_owned())]),
        }]
    );

    let stop = gate_catalog(
        r#"amends "nori:Gate"
name = "under-test"
on = "stop"
rules {
  new StopRule {
    last_message = "done"
    changed { "**/*.rs" }
    decision = "continue"
    message = "keep going"
  }
  new StopRule { decision = "pass" }
}
"#,
    );
    assert_eq!(stop.problems, vec![]);
    assert_eq!(
        stop.gate("under-test").unwrap().value.rules,
        vec![
            Rule::Stop {
                last_message: Some(Pattern::Matches("done".to_owned())),
                changed: vec!["**/*.rs".to_owned()],
                decision: StopDecision::Continue,
                message: Some("keep going".to_owned()),
                max_continues: 1,
            },
            Rule::Stop {
                last_message: None,
                changed: vec![],
                decision: StopDecision::Pass,
                message: None,
                max_continues: 1,
            },
        ]
    );
}

#[test]
fn a_bad_pattern_or_glob_is_a_problem_naming_the_rule_and_field() {
    let message = only_problem(
        r#"amends "nori:Gate"
name = "under-test"
on = "permission"
rules {
  new PermissionRule { command = "(unclosed"; decision = "ask" }
}
"#,
    );
    assert!(message.contains("rules[0].command:"), "{message}");
    assert!(
        message.contains("not a valid regular expression"),
        "{message}"
    );

    let message = only_problem(
        r#"amends "nori:Gate"
name = "under-test"
on = "permission"
rules {
  new PermissionRule { paths { "src/**"; "[a-" }; decision = "ask" }
}
"#,
    );
    assert!(message.contains("rules[0].paths[1]:"), "{message}");
    assert!(message.contains("not a valid glob"), "{message}");

    let message = only_problem(
        r#"amends "nori:Gate"
name = "under-test"
on = "stop"
rules {
  new StopRule { last_message = new Not { regex = "a(" }; decision = "pass" }
}
"#,
    );
    assert!(message.contains("rules[0].last_message:"), "{message}");
}

#[test]
fn a_decision_without_what_it_needs_is_a_problem() {
    for (event, rule, field) in [
        (
            "permission",
            r#"new PermissionRule { decision = "deny" }"#,
            "rules[0].reason",
        ),
        (
            "host_tool",
            r#"new HostToolRule { decision = "deny" }"#,
            "rules[0].reason",
        ),
        (
            "host_tool",
            r#"new HostToolRule { decision = "rewrite" }"#,
            "rules[0]",
        ),
        (
            "stop",
            r#"new StopRule { decision = "continue" }"#,
            "rules[0].message",
        ),
        (
            "stop",
            r#"new StopRule { decision = "pass"; max_continues = 0 }"#,
            "rules[0].max_continues",
        ),
    ] {
        let message = only_problem(&format!(
            "amends \"nori:Gate\"\nname = \"under-test\"\non = \"{event}\"\nrules {{ {rule} }}\n"
        ));
        assert!(message.contains(&format!("{field}:")), "{rule}: {message}");
    }
}

#[test]
fn a_rule_of_another_event_and_an_empty_gate_are_problems() {
    let message = only_problem(
        r#"amends "nori:Gate"
name = "under-test"
on = "stop"
rules {
  new PermissionRule { decision = "ask" }
}
"#,
    );
    assert!(
        message.contains("rules[0]: a permission rule in a gate on \"stop\""),
        "{message}"
    );

    let message = only_problem(
        r#"amends "nori:Gate"
name = "under-test"
on = "stop"
"#,
    );
    assert!(
        message.contains("rules: a gate needs at least one rule"),
        "{message}"
    );
}

#[test]
fn a_typo_in_a_rule_names_the_field() {
    let message = only_problem(
        r#"amends "nori:Gate"
name = "under-test"
on = "stop"
rules {
  new StopRule { decision = "pass"; mesage = "x" }
}
"#,
    );
    assert!(message.contains("mesage"), "{message}");
}

#[test]
fn the_example_gates_load() {
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let catalog = load(
        &Roots {
            user: Some(examples),
            project: None,
        },
        &EvaluatorOptions::preconfigured(),
    );

    let gate_problems: Vec<_> = catalog
        .problems
        .iter()
        .filter(|problem| problem.path.to_string_lossy().contains("/gates/"))
        .collect();
    assert_eq!(gate_problems, Vec::<&super::Problem>::new());
    assert_eq!(
        catalog
            .gates
            .iter()
            .map(|gate| (gate.name.as_str(), gate.value.on, gate.value.rules.len()))
            .collect::<Vec<_>>(),
        [
            ("force-machine", GateEvent::HostTool, 3),
            ("no-force-push", GateEvent::Permission, 2),
            ("tests-before-done", GateEvent::Stop, 1),
        ]
    );
}
