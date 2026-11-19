use std::time::Duration;

use nori_bash::BashError;
use nori_bash::BashSession;
use nori_bash::CancellationToken;
use nori_bash::CompletionKind;
use nori_bash::ExecOptions;
use nori_bash::HighlightKind;
use nori_bash::PathContext;
use nori_bash::PathLiteral;
use nori_bash::RedirectionKind;
use nori_bash::SubshellKind;
use nori_bash::highlight_spans;
use nori_bash::inspect_script;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn persists_functions_exports_aliases_and_cwd_across_calls() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().canonicalize().expect("canonical");
    let sub = root.join("nested");
    std::fs::create_dir_all(&sub).expect("create nested dir");

    let mut session = BashSession::new(&root).await.expect("session init");

    let out1 = session
        .exec("export NORI_TEST_VAR=alpha; greet() { printf 'hi %s in %s' \"$1\" \"$NORI_TEST_VAR\"; }; cd nested")
        .await
        .expect("setup exec");
    assert_eq!(out1.exit_code, 0);
    assert_eq!(out1.cwd, sub);

    let out2 = session
        .exec("greet world && pwd")
        .await
        .expect("follow-up exec");
    assert_eq!(out2.exit_code, 0);
    assert_eq!(out2.stdout, format!("hi world in alpha{}\n", sub.display()));
    assert_eq!(out2.stderr, "");
    assert_eq!(session.env_var("NORI_TEST_VAR").as_deref(), Some("alpha"));
    assert!(session.has_function("greet"));
}

#[tokio::test]
async fn captures_pipelines_redirections_and_nonzero_exit_codes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut session = BashSession::new(temp.path()).await.expect("session init");

    let out = session
        .exec("printf 'b\\na\\nc\\n' | sort | tr 'a-z' 'A-Z'; echo err-msg >&2; false")
        .await
        .expect("pipeline exec");
    assert_eq!(out.exit_code, 1);
    assert_eq!(out.stdout, "A\nB\nC\n");
    assert_eq!(out.stderr, "err-msg\n");

    let file_out = session
        .exec("echo written > out.txt && cat out.txt")
        .await
        .expect("redirection exec");
    assert_eq!(file_out.exit_code, 0);
    assert_eq!(file_out.stdout, "written\n");
}

#[tokio::test]
async fn enforces_timeout_and_cancellation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut session = BashSession::new(temp.path()).await.expect("session init");

    let timeout_err = session
        .exec_with_options(
            "echo before-timeout; sleep 5",
            ExecOptions {
                timeout_ms: Some(80),
                cancel: None,
            },
        )
        .await
        .expect_err("should time out");
    match timeout_err {
        BashError::Timeout {
            timeout_ms, stdout, ..
        } => {
            assert_eq!(timeout_ms, 80);
            assert!(stdout.contains("before-timeout"), "stdout was {stdout:?}");
        }
        other => panic!("expected Timeout, got {other:?}"),
    }

    let token = CancellationToken::new();
    let trigger = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        trigger.cancel();
    });

    let cancel_err = session
        .exec_with_options(
            "echo before-cancel; sleep 5",
            ExecOptions {
                timeout_ms: Some(5_000),
                cancel: Some(token),
            },
        )
        .await
        .expect_err("should cancel");
    match cancel_err {
        BashError::Cancelled { stdout, .. } => {
            assert!(stdout.contains("before-cancel"), "stdout was {stdout:?}");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }

    // Session remains usable after timeout/cancellation.
    let recovered = session.exec("echo alive").await.expect("recovered exec");
    assert_eq!(recovered.exit_code, 0);
    assert_eq!(recovered.stdout, "alive\n");
}

#[tokio::test]
async fn completes_programmable_specs_functions_builtins_vars_and_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    std::fs::create_dir_all(root.join("src_dir")).expect("mkdir");
    std::fs::write(root.join("alpha.txt"), "x").expect("write");

    let mut session = BashSession::new(root).await.expect("session init");
    session
        .exec(
            "export NORI_COMP_VAR=hello; \
             nori_custom_fn() { :; }; \
             complete -W 'start stop status --verbose' myctl",
        )
        .await
        .expect("setup completions");

    let fn_hits = session.complete("nori_custom_", 12).await;
    assert!(
        fn_hits
            .iter()
            .any(|c| c.text == "nori_custom_fn" && c.kind == CompletionKind::Function),
        "fn_hits = {fn_hits:?}"
    );

    let builtin_hits = session.complete("exp", 3).await;
    assert!(
        builtin_hits
            .iter()
            .any(|c| c.text == "export" && c.kind == CompletionKind::Builtin),
        "builtin_hits = {builtin_hits:?}"
    );

    let var_hits = session.complete("echo $NORI_COMP_", 16).await;
    assert!(
        var_hits.iter().any(|c| {
            c.text == "$NORI_COMP_VAR"
                && c.kind == CompletionKind::Variable
                && c.detail.as_deref() == Some("hello")
        }),
        "var_hits = {var_hits:?}"
    );

    let spec_hits = session.complete("myctl st", 8).await;
    let spec_texts: Vec<&str> = spec_hits.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(spec_texts, vec!["start", "status", "stop"]);

    let spec_sta_hits = session.complete("myctl sta", 9).await;
    let spec_sta_texts: Vec<&str> = spec_sta_hits.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(spec_sta_texts, vec!["start", "status"]);

    let flag_hits = session.complete("myctl --v", 9).await;
    assert!(
        flag_hits
            .iter()
            .any(|c| c.text == "--verbose" && c.kind == CompletionKind::Flag),
        "flag_hits = {flag_hits:?}"
    );

    let file_hits = session.complete("cat al", 6).await;
    assert!(
        file_hits
            .iter()
            .any(|c| c.text == "alpha.txt" && c.kind == CompletionKind::File),
        "file_hits = {file_hits:?}"
    );

    let dir_hits = session.complete("cd src_", 7).await;
    assert!(
        dir_hits
            .iter()
            .any(|c| c.text == "src_dir" && c.kind == CompletionKind::Directory),
        "dir_hits = {dir_hits:?}"
    );
}

#[test]
fn highlights_bash_tokens_and_handles_incomplete_input() {
    let input = "if true; then git status --short 42 \"$HOME\" 'lit' > out.txt; fi # done";
    let spans = highlight_spans(input);
    let pairs: Vec<(&str, HighlightKind)> = spans
        .iter()
        .map(|s| {
            let slice = &input[s.start_byte as usize..s.end_byte as usize];
            (slice, s.kind)
        })
        .collect();

    assert!(pairs.contains(&("if", HighlightKind::Keyword)));
    assert!(pairs.contains(&("then", HighlightKind::Keyword)));
    assert!(pairs.contains(&("git", HighlightKind::Command)));
    assert!(pairs.contains(&("status", HighlightKind::Subcommand)));
    assert!(pairs.contains(&("--short", HighlightKind::Flag)));
    assert!(pairs.contains(&("42", HighlightKind::Number)));
    assert!(pairs.contains(&("$HOME", HighlightKind::Variable)));
    assert!(pairs.contains(&("'lit'", HighlightKind::String)));
    assert!(pairs.contains(&(">", HighlightKind::Operator)));
    assert!(pairs.contains(&("out.txt", HighlightKind::Argument)));
    assert!(pairs.contains(&("# done", HighlightKind::Comment)));

    let fn_spans = highlight_spans("my_fn() { echo hi; }");
    assert!(fn_spans.iter().any(|s| {
        &"my_fn() { echo hi; }"[s.start_byte as usize..s.end_byte as usize] == "my_fn"
            && s.kind == HighlightKind::FunctionDef
    }));

    let partial = highlight_spans("git commit -m \"unclosed");
    assert!(!partial.is_empty());
    assert_eq!(partial[0].kind, HighlightKind::Command);
}

#[test]
fn inspects_script_commands_subshells_redirections_and_paths() {
    let script = "cat /etc/hosts | (grep localhost > ./out.txt); echo $(whoami) >> /tmp/log.txt";
    let facts = inspect_script(script).expect("valid script");

    assert_eq!(facts.command_names, vec!["cat", "grep", "echo", "whoami"]);
    assert_eq!(facts.subshells.len(), 2);
    assert_eq!(facts.subshells[0].kind, SubshellKind::Subshell);
    assert_eq!(facts.subshells[1].kind, SubshellKind::CommandSubstitution);

    assert_eq!(facts.redirections.len(), 2);
    assert_eq!(facts.redirections[0].kind, RedirectionKind::Write);
    assert_eq!(facts.redirections[0].target, "./out.txt");
    assert!(facts.redirections[0].is_write);
    assert_eq!(facts.redirections[1].kind, RedirectionKind::Append);
    assert_eq!(facts.redirections[1].target, "/tmp/log.txt");

    assert!(facts.path_literals.contains(&PathLiteral {
        path: "/etc/hosts".to_owned(),
        context: PathContext::Argument,
    }));
    assert!(facts.path_literals.contains(&PathLiteral {
        path: "./out.txt".to_owned(),
        context: PathContext::RedirectWrite,
    }));
    assert!(facts.path_literals.contains(&PathLiteral {
        path: "/tmp/log.txt".to_owned(),
        context: PathContext::RedirectWrite,
    }));
    assert!(!facts.has_dynamic_commands);

    let dyn_facts = inspect_script("$CMD --flag").expect("dynamic parse");
    assert!(dyn_facts.has_dynamic_commands);

    assert!(inspect_script("if true; then").is_err());
}
