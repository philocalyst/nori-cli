//! Tests for the sandbox policy.

use std::path::Path;

use crate::ConduitError;
use crate::SandboxPolicy;
use crate::enforce_bash_sandbox;
use crate::narrow_sandbox;

#[test]
fn bash_ast_sandbox_deterministically_blocks_escape_and_disallowed_commands() {
    let root = Path::new("/workspace/project");
    let sandbox = SandboxPolicy {
        allowed_tools: vec!["bash:git".to_owned(), "bash:cargo-stock".to_owned()],
        allowed_paths: vec!["/workspace/project".to_owned()],
    };

    enforce_bash_sandbox(Some(&sandbox), root, "git status && cargo-stock test")
        .expect("allowed commands pass");

    let cmd_err = enforce_bash_sandbox(Some(&sandbox), root, "git status && rm -rf /tmp/x")
        .expect_err("rm blocked");
    assert!(matches!(cmd_err, ConduitError::SandboxViolation { .. }));

    let redir_err =
        enforce_bash_sandbox(Some(&sandbox), root, "git status > ../../etc/escaped.txt")
            .expect_err("redirection outside workspace blocked");
    assert!(matches!(redir_err, ConduitError::SandboxViolation { .. }));
}

#[test]
fn a_nested_sandbox_never_widens_the_outer_one() {
    let outer = SandboxPolicy {
        allowed_tools: vec!["bash:git".to_owned(), "bash:cargo".to_owned()],
        allowed_paths: vec!["/workspace/project/src".to_owned()],
    };
    let inner = SandboxPolicy {
        allowed_tools: vec!["bash:git".to_owned(), "bash:rm".to_owned()],
        allowed_paths: vec!["/workspace/project/src/deep".to_owned(), "/etc".to_owned()],
    };
    let narrowed = narrow_sandbox(Some(&outer), &inner);
    assert_eq!(narrowed.allowed_tools, vec!["bash:git".to_owned()]);
    assert_eq!(
        narrowed.allowed_paths,
        vec!["/workspace/project/src/deep".to_owned()]
    );
}
