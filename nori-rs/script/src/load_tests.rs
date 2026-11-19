use pretty_assertions::assert_eq;

use super::*;
use crate::model::Hook;
use crate::model::HookEvent;

struct Home {
    _dir: tempfile::TempDir,
    global: PathBuf,
    project: PathBuf,
}

/// A NORI_HOME and a repository with the given scripts (`None` leaves the
/// file absent).
fn home(global: Option<&str>, project: Option<&str>) -> Home {
    let dir = tempfile::tempdir().expect("tempdir");
    let global_path = global_script_path(&dir.path().join("home"));
    let project_path = project_script_path(&dir.path().join("repo"));
    for (path, text) in [(&global_path, global), (&project_path, project)] {
        if let Some(text) = text {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write");
        }
    }
    Home {
        _dir: dir,
        global: global_path,
        project: project_path,
    }
}

#[test]
fn paths_follow_the_helix_layout() {
    assert_eq!(
        global_script_path(Path::new("/home/me/.nori")),
        PathBuf::from("/home/me/.nori/init.scm")
    );
    assert_eq!(
        project_script_path(Path::new("/src/app")),
        PathBuf::from("/src/app/.nori/nori.scm")
    );
}

#[test]
fn missing_scripts_load_nothing() {
    let home = home(None, None);
    assert_eq!(
        load(Some(&home.global), Some(&home.project)),
        Ok(Loaded::default())
    );
    assert_eq!(load(None, None), Ok(Loaded::default()));
}

#[test]
fn fallback_policy_loads_from_global_and_can_be_replaced_by_project() {
    let home = home(
        Some(
            r#"(define-workflow standard (fallback-models "provider/a" "provider/b")) (default-workflow standard)"#,
        ),
        Some(
            r#"(define-workflow standard (use 'standard) (fallback-models "provider/c" "provider/a")) (default-workflow standard)"#,
        ),
    );
    let global = load(Some(&home.global), None).expect("global loads");
    let project = load(Some(&home.global), Some(&home.project)).expect("project loads");
    assert_eq!(
        global
            .default_workflow()
            .expect("default")
            .recovery
            .base
            .fallback
            .chain,
        ["provider/a", "provider/b"].map(nori_workflow::Route::parse_legacy)
    );
    assert_eq!(
        project
            .default_workflow()
            .expect("default")
            .recovery
            .base
            .fallback
            .chain,
        ["provider/c", "provider/a"].map(nori_workflow::Route::parse_legacy)
    );
}

#[test]
fn the_project_script_sees_and_overrides_the_global_one() {
    let home = home(
        Some(
            r#"
(define-workflow land (turns 3) (on-change "Commit it."))
(define-workflow explore (turns 10))
(default-workflow explore)
"#,
        ),
        Some(
            r#"
(define-workflow land (use 'land) (turns 1))
(default-workflow land)
"#,
        ),
    );
    let loaded = load(Some(&home.global), Some(&home.project)).expect("loads");
    assert_eq!(
        loaded.workflows(),
        &[
            Workflow {
                max_turns: Some(1),
                hooks: vec![Hook {
                    event: HookEvent::AfterChange,
                    prompt: "Commit it.".to_string(),
                }],
                ..Workflow::named("land")
            },
            Workflow {
                max_turns: Some(10),
                ..Workflow::named("explore")
            },
        ]
    );
    assert_eq!(
        loaded
            .default_workflow()
            .map(|workflow| workflow.name.as_str()),
        Some("land")
    );
    assert_eq!(
        loaded.script_paths().collect::<Vec<_>>(),
        vec![home.global.as_path(), home.project.as_path()]
    );
}

#[test]
fn errors_name_the_file_line_and_column() {
    let home = home(
        Some("(define-workflow ok (turns 1))\n"),
        Some("(define-workflow bad\n  (turns 'many))\n"),
    );
    assert_eq!(
        load(Some(&home.global), Some(&home.project)),
        Err(ScriptError {
            origin: Origin::File(home.project.clone()),
            kind: ErrorKind::Eval,
            line: Some(2),
            column: Some(4),
            message: "turns: the turn count must be a whole number of at least 0, got 'many"
                .to_string(),
        })
    );
}

#[test]
fn a_syntax_error_is_located() {
    let home = home(Some("(define-workflow w\n  (turns 1)"), None);
    assert_eq!(
        load(Some(&home.global), None),
        Err(ScriptError {
            origin: Origin::File(home.global.clone()),
            kind: ErrorKind::Syntax,
            line: Some(1),
            column: Some(1),
            message: "this form is never closed".to_string(),
        })
    );
}

#[test]
fn an_unreadable_script_is_an_io_error() {
    let home = home(None, None);
    std::fs::create_dir_all(&home.global).expect("a directory where the file should be");
    let error = load(Some(&home.global), None).expect_err("unreadable");
    assert_eq!(
        (error.origin, error.kind),
        (Origin::File(home.global), ErrorKind::Io)
    );
}
