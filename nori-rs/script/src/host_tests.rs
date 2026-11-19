use std::time::Instant;

use pretty_assertions::assert_eq;

use super::*;
use crate::model::Workflow;

#[test]
fn builtin_fallback_defaults_are_steel_and_can_be_overridden() {
    let host = ScriptHost::spawn_with_defaults(None, None).expect("host");
    let loaded = host.loaded().expect("defaults load");
    let workflow = loaded.default_workflow().expect("default workflow");
    assert_eq!(workflow.name, "nori-default");
    assert!(workflow.recovery.base.fallback.ranked);
    assert!(workflow.recovery.base.fallback.chain.is_empty());

    let inline = host
        .apply_inline(",(fallback-models \"provider/model\")\nhello")
        .expect("inline");
    let workflow = inline.workflow.expect("workflow");
    assert!(!workflow.recovery.base.fallback.ranked);
    assert_eq!(
        workflow.recovery.base.fallback.chain,
        vec![nori_workflow::Route::parse_legacy("provider/model")]
    );
    let inline = host
        .apply_inline(",(fallback-models)\nhello")
        .expect("inline");
    let workflow = inline.workflow.expect("workflow");
    assert!(!workflow.recovery.base.fallback.ranked);
    assert!(workflow.recovery.base.fallback.chain.is_empty());

    let dir = tempfile::tempdir().expect("tempdir");
    let global = script(
        &dir,
        "init.scm",
        "(define-workflow custom (use 'nori-default) (fallback-models)) (default-workflow custom)",
    );
    let host = ScriptHost::spawn_with_defaults(Some(global), None).expect("host");
    let loaded = host.loaded().expect("user policy loads");
    let workflow = loaded.default_workflow().expect("custom default");
    assert_eq!(workflow.name, "custom");
    assert!(!workflow.recovery.base.fallback.ranked);
    assert!(workflow.recovery.base.fallback.chain.is_empty());
}

fn script(dir: &tempfile::TempDir, name: &str, text: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("write");
    path
}

#[test]
fn the_host_loads_and_applies_inline_dsl_to_the_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let global = script(
        &dir,
        "init.scm",
        "(define-workflow land (turns 3))\n(default-workflow land)",
    );
    let host = ScriptHost::spawn(Some(global), None).expect("spawn");
    let land = Workflow {
        max_turns: Some(3),
        ..Workflow::named("land")
    };
    assert_eq!(
        host.loaded()
            .map(|loaded| loaded.default_workflow().cloned()),
        Ok(Some(land.clone()))
    );
    assert_eq!(
        host.apply_inline("Go.\n,(turns 1)"),
        Ok(InlinePrompt {
            prompt: "Go.".to_string(),
            workflow: Some(Workflow {
                max_turns: Some(1),
                ..land
            }),
        })
    );
    // Handles are cheap clones of one thread.
    let clone = host;
    assert_eq!(
        clone.apply_inline("No DSL here."),
        Ok(InlinePrompt {
            prompt: "No DSL here.".to_string(),
            workflow: None,
        })
    );
}

#[test]
fn inline_dsl_still_works_after_a_failed_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let global = script(&dir, "init.scm", "(turns)");
    let host = ScriptHost::spawn(Some(global.clone()), None).expect("spawn");
    let error = host.loaded().expect_err("load fails");
    assert_eq!(error.origin, Origin::File(global));
    assert_eq!(
        host.apply_inline(",(turns 2)").map(|turn| turn.workflow),
        Ok(Some(Workflow {
            max_turns: Some(2),
            ..Workflow::named(crate::INLINE_WORKFLOW)
        }))
    );
}

/// Not an assertion on speed, which varies by machine and build profile:
/// prints what loading and inline evaluation cost. Run with `--nocapture`.
#[test]
fn timings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let global = script(&dir, "init.scm", include_str!("../examples/init.scm"));
    let started = Instant::now();
    let host = ScriptHost::spawn(Some(global.clone()), None).expect("spawn");
    let spawned = started.elapsed();
    host.loaded().expect("loads");
    let first_load = started.elapsed();
    let started = Instant::now();
    let runs: u32 = 10;
    for _ in 0..runs {
        host.apply_inline("Go.\n,(turns 1)\n,(on-stop \"Check.\")")
            .expect("applies");
    }
    let inline = started.elapsed() / runs;
    // A second engine on the same thread reuses Steel's per-thread kernel.
    let started = Instant::now();
    crate::load(Some(&global), None).expect("loads");
    let fresh_thread_load = started.elapsed();
    let started = Instant::now();
    crate::load(Some(&global), None).expect("loads");
    let warm_thread_load = started.elapsed();
    eprintln!(
        "nori-script timings: spawn {spawned:?}, first load {first_load:?}, inline on a warm host {inline:?}, load on a fresh test thread {fresh_thread_load:?}, second load on that thread {warm_thread_load:?}"
    );
}
