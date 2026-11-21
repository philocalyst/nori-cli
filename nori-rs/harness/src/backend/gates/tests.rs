//! Compiling gates and matching what each event reads.

use std::path::Path;
use std::time::Instant;

use nori_pkl::EvaluatorOptions;
use nori_pkl::catalog::Roots;
use nori_pkl::kinds::GateEvent;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::Gates;
use super::host_tool::Judged;
use super::permission::PermissionFacts;
use super::permission::Verdict;
use super::stop::ContinueCounts;
use super::stop::Decision;
use super::stop::StopFacts;

const CWD: &str = "/work/project";

/// Compiles the gate modules `sources` (file name, Pkl) as a session's catalog would.
fn gates_from(sources: &[(&str, &str)]) -> Gates {
    let home = tempfile::tempdir().expect("a home");
    let dir = home.path().join("gates");
    std::fs::create_dir_all(&dir).expect("a gates directory");
    for (name, source) in sources {
        std::fs::write(dir.join(name), source).expect("a module");
    }
    let catalog = nori_pkl::catalog::load(
        &Roots {
            user: Some(home.path().to_path_buf()),
            project: None,
        },
        &EvaluatorOptions::preconfigured(),
    );
    assert_eq!(catalog.problems, vec![]);
    let (gates, problems) = Gates::compile(&catalog);
    assert_eq!(problems, Vec::<String>::new());
    gates
}

/// The example gates the repository ships.
fn examples() -> Gates {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pkl/examples/gates");
    let sources: Vec<(String, String)> = std::fs::read_dir(dir)
        .expect("the examples")
        .map(|entry| {
            let path = entry.expect("an example").path();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&path).expect("readable"),
            )
        })
        .collect();
    gates_from(
        &sources
            .iter()
            .map(|(name, source)| (name.as_str(), source.as_str()))
            .collect::<Vec<_>>(),
    )
}

fn request(fields: acp::ToolCallUpdateFields) -> acp::RequestPermissionRequest {
    acp::RequestPermissionRequest::new(
        "session",
        acp::ToolCallUpdate::new("call", fields),
        vec![acp::PermissionOption::new(
            "once",
            "Once",
            acp::PermissionOptionKind::AllowOnce,
        )],
    )
}

fn facts_of(fields: acp::ToolCallUpdateFields) -> PermissionFacts {
    PermissionFacts::of(&request(fields), Path::new(CWD))
}

fn run(command: &str) -> PermissionFacts {
    facts_of(
        acp::ToolCallUpdateFields::new()
            .kind(acp::ToolKind::Execute)
            .title(command)
            .raw_input(json!({ "command": command })),
    )
}

fn edit(path: &str) -> PermissionFacts {
    facts_of(
        acp::ToolCallUpdateFields::new()
            .kind(acp::ToolKind::Edit)
            .title("Edit a file")
            .locations(vec![acp::ToolCallLocation::new(path)]),
    )
}

/// The verdict and which rule gave it, for a request.
fn permission(gates: &Gates, facts: &PermissionFacts) -> Option<(String, i64, Verdict)> {
    gates
        .check_permission(facts)
        .map(|hit| (hit.gate.to_owned(), hit.index, hit.rule.decision.clone()))
}

const FORCE_PUSH_REASON: &str =
    "Force-pushing is not allowed here. Push without --force, or tell the person why it is needed.";

#[test]
fn a_force_push_is_denied_however_it_is_spelled_and_any_other_push_asks() {
    let gates = examples();
    let denied = |command: &str| permission(&gates, &run(command));

    for command in [
        "git push --force origin main",
        "git push -f",
        "git push origin main --force-with-lease",
        "cd app && git push --force",
    ] {
        assert_eq!(
            denied(command),
            Some((
                "no-force-push".to_owned(),
                0,
                Verdict::Deny {
                    reason: FORCE_PUSH_REASON.to_owned()
                }
            )),
            "{command}"
        );
    }
    assert_eq!(
        denied("git push origin main"),
        Some(("no-force-push".to_owned(), 1, Verdict::Ask))
    );
    assert_eq!(denied("git status"), None);
    assert_eq!(
        denied("echo git push is a command"),
        Some(("no-force-push".to_owned(), 1, Verdict::Ask))
    );
}

#[test]
fn matchers_are_all_required_and_a_negated_pattern_matches_what_it_does_not_name() {
    let gates = gates_from(&[(
        "paths.pkl",
        r#"amends "nori:Gate"
name = "paths"
on = "permission"
rules {
  new PermissionRule { kinds { "edit" }; paths { "**/.env*" }; decision = "deny"; reason = "secrets" }
  new PermissionRule { kinds { "edit" }; only_paths { "docs/**"; "*.md" }; decision = "allow" }
  new PermissionRule { kinds { "execute" }; command = new Not { regex = "^(ls|cat) " }; title = "rm"; decision = "ask" }
}
"#,
    )]);
    let verdict = |facts: &PermissionFacts| {
        permission(&gates, facts).map(|(_, rule, verdict)| (rule, verdict))
    };

    assert_eq!(
        verdict(&edit("/work/project/src/.env.local")),
        Some((
            0,
            Verdict::Deny {
                reason: "secrets".to_owned()
            }
        ))
    );
    assert_eq!(
        verdict(&edit("/work/project/docs/guide/a.txt")),
        Some((1, Verdict::Allow))
    );
    assert_eq!(
        verdict(&edit("/work/project/README.md")),
        Some((1, Verdict::Allow))
    );
    assert_eq!(
        verdict(&edit("/work/project/src/lib.rs")),
        None,
        "outside the allowed paths"
    );
    assert_eq!(
        verdict(&facts_of(
            acp::ToolCallUpdateFields::new()
                .kind(acp::ToolKind::Edit)
                .locations(vec![
                    acp::ToolCallLocation::new("/work/project/docs/a.md"),
                    acp::ToolCallLocation::new("/work/project/src/lib.rs"),
                ])
        )),
        None,
        "every path must be inside for only_paths"
    );
    assert_eq!(
        verdict(&facts_of(
            acp::ToolCallUpdateFields::new()
                .kind(acp::ToolKind::Execute)
                .title("rm")
                .raw_input(json!({ "command": "rm -rf build" }))
        )),
        Some((2, Verdict::Ask))
    );
    assert_eq!(
        verdict(&facts_of(
            acp::ToolCallUpdateFields::new()
                .kind(acp::ToolKind::Execute)
                .title("rm")
                .raw_input(json!({ "command": "ls -la" }))
        )),
        None
    );
    assert_eq!(
        verdict(&facts_of(
            acp::ToolCallUpdateFields::new()
                .kind(acp::ToolKind::Execute)
                .title("rm")
        )),
        None,
        "a call with no command never matches a command matcher"
    );
}

#[test]
fn facts_read_the_command_with_its_arguments() {
    let facts = facts_of(
        acp::ToolCallUpdateFields::new()
            .kind(acp::ToolKind::Execute)
            .title("Run")
            .raw_input(json!({ "command": "git", "args": ["push", "-f"] })),
    );
    assert_eq!(facts.subject(), "git push -f");
    let facts = edit("/work/project/sub/../a.rs");
    assert_eq!(facts.subject(), "Edit a file");
    let nothing = facts_of(acp::ToolCallUpdateFields::new());
    assert_eq!(nothing.subject(), "a tool request");
}

fn host<'g>(gates: &'g Gates, tool: &str, arguments: serde_json::Value) -> Judged<'g> {
    gates.judge_host_call(tool, arguments)
}

#[test]
fn a_host_call_is_allowed_denied_or_rewritten_by_the_first_matching_rule() {
    let gates = examples();

    // Already a dry run: let through unchanged, and the machine rule is not consulted.
    let Judged::Decided { call, decision, .. } =
        host(&gates, "bash", json!({ "command": "git push --dry-run" }))
    else {
        panic!("allowed");
    };
    assert_eq!(
        (call.arguments, call.rewritten, decision),
        (json!({ "command": "git push --dry-run" }), None, "allow")
    );

    // A bare push: a dry run now.
    let Judged::Decided {
        call,
        gate,
        rule,
        decision,
        detail,
        ..
    } = host(
        &gates,
        "bash",
        json!({ "command": "git push origin main", "machine": "mac" }),
    )
    else {
        panic!("rewritten");
    };
    assert_eq!(
        (call.arguments, gate, rule, decision, detail.as_deref()),
        (
            json!({ "command": "git push origin main --dry-run", "machine": "mac" }),
            "force-machine",
            1,
            "rewrite",
            Some("added \"--dry-run\" to command")
        )
    );
    assert_eq!(
        call.rewritten.as_deref(),
        Some(
            "Note: gate `force-machine` rewrote this call before it ran (added \"--dry-run\" to command)."
        )
    );

    // No machine named: the build machine.
    let Judged::Decided { call, .. } = host(&gates, "bash", json!({ "command": "cargo test" }))
    else {
        panic!("rewritten");
    };
    assert_eq!(
        call.arguments,
        json!({ "command": "cargo test", "machine": "h16" })
    );

    // A machine named, nothing about the command: no rule applies.
    assert!(matches!(
        host(
            &gates,
            "bash",
            json!({ "command": "cargo test", "machine": "mac" })
        ),
        Judged::Unmatched(_)
    ));
    // Another tool.
    assert!(matches!(
        host(&gates, "job_list", json!({})),
        Judged::Unmatched(_)
    ));

    let denying = gates_from(&[(
        "deny.pkl",
        r#"amends "nori:Gate"
name = "no-rm"
on = "host_tool"
rules {
  new HostToolRule { tools { "bash" }; arguments { ["command"] = "rm -rf /" }; decision = "deny"; reason = "no" }
}
"#,
    )]);
    let Judged::Refused { gate, rule, reason } =
        host(&denying, "bash", json!({ "command": "rm -rf /" }))
    else {
        panic!("refused");
    };
    assert_eq!((gate, rule, reason.as_str()), ("no-rm", 0, "no"));
}

#[test]
fn a_stop_rule_continues_a_turn_that_changed_code_without_a_passing_run_once() {
    let gates = examples();
    let mut counts = ContinueCounts::default();
    let rust = ["src/lib.rs".to_owned()];
    let docs = ["README.md".to_owned()];
    let stopped = |message: &str, changed: &[String], counts: &mut ContinueCounts| {
        gates
            .check_stop(
                &StopFacts {
                    last_message: message,
                    changed,
                },
                counts,
            )
            .map(|hit| {
                (
                    hit.gate.to_owned(),
                    hit.index,
                    matches!(hit.rule.decision, Decision::Continue { .. }),
                )
            })
    };
    let continued = Some(("tests-before-done".to_owned(), 0, true));

    assert_eq!(stopped("I made the change.", &rust, &mut counts), continued);
    assert_eq!(
        stopped("I made the change.", &rust, &mut counts),
        None,
        "spent for this task: the turn may end"
    );
    counts.clear();
    assert_eq!(
        stopped("Done. All tests pass.", &rust, &mut counts),
        None,
        "the agent says the tests passed"
    );
    assert_eq!(
        stopped("I made the change.", &[], &mut counts),
        None,
        "no file changed"
    );
    assert_eq!(
        stopped("I made the change.", &docs, &mut counts),
        None,
        "not a source file"
    );
    assert_eq!(
        stopped("I made the change.", &rust, &mut counts),
        continued,
        "a new task may be continued again"
    );
    counts.refund("tests-before-done", 0);
    assert_eq!(
        stopped("not tested", &rust, &mut counts),
        continued,
        "a continuation that could not be delivered is given back"
    );
}

#[test]
fn a_catalog_without_gates_compiles_to_none() {
    let gates = gates_from(&[]);
    assert_eq!(
        (
            gates.has_permission(),
            gates.has_host_tool(),
            gates.has_stop()
        ),
        (false, false, false)
    );
    assert_eq!(GateEvent::HostTool.as_str(), "host_tool");
}

/// A gate check is a few strings and a few regexes: it must stay far below anything a person (or
/// a tool call) could notice. Run `cargo test -p nori-harness gates -- --nocapture` to see the
/// figures; the bound is loose enough for an unoptimized build under load.
#[test]
fn checking_a_gate_costs_microseconds() {
    let mut source =
        String::from("amends \"nori:Gate\"\nname = \"many\"\non = \"permission\"\nrules {\n");
    for index in 0..40 {
        source.push_str(&format!(
            "  new PermissionRule {{ kinds {{ \"execute\" }}; command = \"\\\\bnever-{index}\\\\b.*--force\"; paths {{ \"secret-{index}/**\" }}; decision = \"deny\"; reason = \"no\" }}\n"
        ));
    }
    source.push_str("  new PermissionRule { kinds { \"execute\" }; command = \"^git status\"; decision = \"allow\" }\n}\n");
    let gates = gates_from(&[("many.pkl", &source)]);
    let facts = run("git status");
    assert_eq!(permission(&gates, &facts).map(|hit| hit.1), Some(40));

    const RUNS: u32 = 2_000;
    let started = Instant::now();
    for _ in 0..RUNS {
        std::hint::black_box(gates.check_permission(std::hint::black_box(&facts)));
    }
    let per_check = started.elapsed() / RUNS;
    let request = request(
        acp::ToolCallUpdateFields::new()
            .kind(acp::ToolKind::Execute)
            .title("git status")
            .raw_input(json!({ "command": "git status" })),
    );
    let started = Instant::now();
    for _ in 0..RUNS {
        std::hint::black_box(PermissionFacts::of(
            std::hint::black_box(&request),
            Path::new(CWD),
        ));
    }
    let per_facts = started.elapsed() / RUNS;
    println!("gates: 41 rules, check {per_check:?}, reading the request {per_facts:?}");
    assert!(per_check.as_micros() < 200, "{per_check:?}");
    assert!(per_facts.as_micros() < 200, "{per_facts:?}");
}
