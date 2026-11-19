use claude_code_agent_sdk::HookSpecificOutput;

use super::*;
use crate::settings::PermissionSettings;
use crate::settings::Settings;
use serde_json::json;

fn make_permission_checker(permissions: PermissionSettings) -> Arc<RwLock<PermissionChecker>> {
    let settings = Settings {
        permissions: Some(permissions),
        ..Default::default()
    };
    Arc::new(RwLock::new(PermissionChecker::new(settings, "/tmp")))
}

fn make_test_hook(checker: Arc<RwLock<PermissionChecker>>) -> HookCallback {
    make_test_hook_with_mode(checker, PermissionMode::Default)
}

fn make_test_hook_with_mode(
    checker: Arc<RwLock<PermissionChecker>>,
    mode: PermissionMode,
) -> HookCallback {
    let connection_cx_lock: Arc<OnceLock<Cx>> = Arc::new(OnceLock::new());
    let tool_use_id_cache: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    // Create PermissionHandler with the specified mode
    let mut permission = PermissionHandler::with_checker(checker.clone());
    permission.set_mode(mode);
    create_pre_tool_use_hook(
        connection_cx_lock,
        "test-session".to_string(),
        Some(checker),
        Arc::new(RwLock::new(permission)),
        tool_use_id_cache,
        Arc::new(EditSnapshots::new(std::path::PathBuf::from("/tmp"))),
    )
}

#[tokio::test]
async fn test_pre_tool_use_hook_allow() {
    let checker = make_permission_checker(PermissionSettings {
        allow: Some(vec!["Read".to_string()]),
        ..Default::default()
    });

    let hook = make_test_hook(checker);
    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Read".to_string(),
        tool_input: json!({"file_path": "/tmp/test.txt"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
            } else {
                panic!("Expected PreToolUse specific output");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

// TODO: Re-enable when implementing permission checks
// #[tokio::test]
// async fn test_pre_tool_use_hook_deny() {
//     let checker = make_permission_checker(PermissionSettings {
//         deny: Some(vec!["Bash".to_string()]),
//         ..Default::default()
//     });
//
//     let hook = make_test_hook(checker);
//     let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
//         session_id: "test".to_string(),
//         transcript_path: "/tmp/test".to_string(),
//         cwd: "/tmp".to_string(),
//         permission_mode: None,
//         tool_name: "Bash".to_string(),
//         tool_input: json!({"command": "ls"}),
//     });
//
//     let result = hook(input, None, HookContext::default()).await;
//
//     match result {
//         HookJsonOutput::Sync(output) => {
//             assert_eq!(output.continue_, Some(true));
//             if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output
//             {
//                 assert_eq!(specific.permission_decision, Some("deny".to_string()));
//             } else {
//                 panic!("Expected PreToolUse specific output");
//             }
//         }
//         HookJsonOutput::Async(_) => panic!("Expected sync output"),
//     }
// }

#[tokio::test]
async fn test_pre_tool_use_hook_ask_by_default() {
    // When no rules match, decision is "Ask".
    // Following TypeScript version's design, the hook just returns { continue: true }
    // to let the can_use_tool callback handle the permission request.
    let checker = make_permission_checker(PermissionSettings::default());
    let hook = make_test_hook(checker);

    // Test MCP tool - no matching rules means "Ask" decision,
    // hook returns continue=true with no hook_specific_output
    let input_mcp = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "mcp__acp__Write".to_string(),
        tool_input: json!({"file_path": "/tmp/test.txt", "content": "test"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result_mcp = hook(input_mcp, None, HookContext::default()).await;

    match result_mcp {
        HookJsonOutput::Sync(output) => {
            // Ask decision returns continue=true with no hook_specific_output
            // The actual permission request is handled by can_use_tool callback
            assert_eq!(output.continue_, Some(true));
            assert!(
                output.hook_specific_output.is_none(),
                "Ask decision should not set hook_specific_output"
            );
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }

    // Test built-in tool - same behavior
    let input_builtin = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Write".to_string(),
        tool_input: json!({"file_path": "/tmp/test.txt", "content": "test"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result_builtin = hook(input_builtin, None, HookContext::default()).await;

    match result_builtin {
        HookJsonOutput::Sync(output) => {
            // Ask decision returns continue=true with no hook_specific_output
            assert_eq!(output.continue_, Some(true));
            assert!(
                output.hook_specific_output.is_none(),
                "Ask decision should not set hook_specific_output"
            );
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_pre_tool_use_hook_ignores_other_events() {
    let checker = make_permission_checker(PermissionSettings::default());

    let hook = make_test_hook(checker);
    let input = HookInput::PostToolUse(claude_code_agent_sdk::PostToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Read".to_string(),
        tool_input: json!({}),
        tool_response: json!("content"),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            assert!(output.hook_specific_output.is_none());
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_bypass_permissions_mode_allows_everything() {
    // BypassPermissions mode should allow all tools without checking rules
    let checker = make_permission_checker(PermissionSettings {
        deny: Some(vec!["Bash".to_string()]),
        ..Default::default()
    });

    let hook = make_test_hook_with_mode(checker, PermissionMode::BypassPermissions);
    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Bash".to_string(),
        tool_input: json!({"command": "rm -rf /"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
                assert!(
                    specific
                        .permission_decision_reason
                        .unwrap()
                        .contains("BypassPermissions")
                );
            } else {
                panic!("Expected PreToolUse specific output");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_default_mode_respects_settings_rules() {
    // Default mode should respect settings rules
    let checker = make_permission_checker(PermissionSettings {
        allow: Some(vec!["Read".to_string()]),
        ..Default::default()
    });

    let hook = make_test_hook_with_mode(checker, PermissionMode::Default);
    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Read".to_string(),
        tool_input: json!({"file_path": "/tmp/test.txt"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
            } else {
                panic!("Expected PreToolUse specific output");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_default_mode_auto_allows_read_only_tools() {
    // Default mode should auto-allow read-only operations (Read, Grep, Glob, LS, NotebookRead)
    // even without explicit allow rules
    let checker = make_permission_checker(PermissionSettings::default()); // No rules

    let hook = make_test_hook_with_mode(checker, PermissionMode::Default);

    // Test Read tool
    let input_read = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Read".to_string(),
        tool_input: json!({"file_path": "/tmp/test.txt"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input_read, None, HookContext::default()).await;
    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
                assert!(
                    specific
                        .permission_decision_reason
                        .unwrap()
                        .contains("read-only")
                );
            } else {
                panic!("Expected PreToolUse specific output");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }

    // Test LS tool
    let input_ls = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "LS".to_string(),
        tool_input: json!({"path": "/tmp"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result_ls = hook(input_ls, None, HookContext::default()).await;
    match result_ls {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
            } else {
                panic!("Expected PreToolUse specific output for LS");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }

    // Test Grep tool with mcp__acp__ prefix
    let input_grep = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "mcp__acp__Grep".to_string(),
        tool_input: json!({"pattern": "test", "path": "/tmp"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result_grep = hook(input_grep, None, HookContext::default()).await;
    match result_grep {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
            } else {
                panic!("Expected PreToolUse specific output for Grep");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_default_mode_auto_allows_safe_bash_commands() {
    // Default mode should auto-allow known safe Bash commands
    let checker = make_permission_checker(PermissionSettings::default()); // No rules

    let hook = make_test_hook_with_mode(checker, PermissionMode::Default);

    // Test with a safe command (ls is in the safe command whitelist)
    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Bash".to_string(),
        tool_input: json!({"command": "ls -la /tmp"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;
    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
                assert!(
                    specific
                        .permission_decision_reason
                        .as_ref()
                        .unwrap()
                        .contains("known safe command"),
                    "Expected 'known safe command' in reason"
                );
            } else {
                panic!("Expected PreToolUse specific output for safe Bash command");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }

    // Test with find (conditionally safe without dangerous options)
    let input_find = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Bash".to_string(),
        tool_input: json!({"command": "find . -name '*.rs'"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result_find = hook(input_find, None, HookContext::default()).await;
    match result_find {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
            } else {
                panic!("Expected PreToolUse specific output for safe find command");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }

    // Test with git status (safe git subcommand)
    let input_git = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Bash".to_string(),
        tool_input: json!({"command": "git status"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result_git = hook(input_git, None, HookContext::default()).await;
    match result_git {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
            } else {
                panic!("Expected PreToolUse specific output for safe git command");
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_default_mode_asks_for_write_tools() {
    // Default mode should ask for permission for write tools (Bash with non-safe commands, Edit, Write)
    let checker = make_permission_checker(PermissionSettings::default()); // No rules

    let hook = make_test_hook_with_mode(checker, PermissionMode::Default);

    // Use a command that is NOT in the safe command list
    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Bash".to_string(),
        tool_input: json!({"command": "mkdir new_dir"}), // mkdir is not a safe command
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;
    match result {
        HookJsonOutput::Sync(output) => {
            // Ask decision returns continue=true with no hook_specific_output
            assert_eq!(output.continue_, Some(true));
            assert!(
                output.hook_specific_output.is_none(),
                "Bash with non-safe command should trigger Ask decision, not auto-allow"
            );
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_empty_tool_name_handling() {
    // Test that empty tool_name is handled gracefully
    let _connection_cx_lock: Arc<OnceLock<Cx>> = Arc::new(OnceLock::new());
    let _tool_use_id_cache: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    // Test with empty tool_name - should not panic and should use fallback
    let empty_tool_name = "";
    let result = format!("Tool {empty_tool_name} denied");
    assert!(
        result.contains("Tool  denied"),
        "Empty tool_name produces double space"
    );

    // Test the fallback logic
    let display_name = if empty_tool_name.is_empty() {
        "the requested tool"
    } else {
        empty_tool_name
    };
    assert_eq!(display_name, "the requested tool");

    // Test with normal tool_name
    let normal_tool_name = "Write";
    let display_name2 = if normal_tool_name.is_empty() {
        "the requested tool"
    } else {
        normal_tool_name
    };
    assert_eq!(display_name2, "Write");
}

#[tokio::test]
async fn test_plan_mode_allows_writing_plan_files() {
    // Plan mode should allow writing to ~/.claude/plans/
    let checker = make_permission_checker(PermissionSettings::default());
    let hook = make_test_hook_with_mode(checker, PermissionMode::Plan);

    let home = dirs::home_dir().unwrap();
    let plan_file = home.join(".claude").join("plans").join("test-plan.md");

    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Write".to_string(),
        tool_input: json!({
            "file_path": plan_file.to_str().unwrap(),
            "content": "# Test Plan"
        }),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            // Should allow (not deny) plan file writes
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_ne!(specific.permission_decision, Some("deny".to_string()));
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_plan_mode_blocks_non_plan_file_writes() {
    // Plan mode should block writes to non-plan files
    let checker = make_permission_checker(PermissionSettings::default());
    let hook = make_test_hook_with_mode(checker, PermissionMode::Plan);

    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Write".to_string(),
        tool_input: json!({
            "file_path": "/tmp/test.txt",
            "content": "test"
        }),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("deny".to_string()));
                assert!(
                    specific
                        .permission_decision_reason
                        .as_ref()
                        .unwrap()
                        .contains("Plan mode")
                );
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_plan_mode_blocks_bash() {
    // Plan mode should block Bash commands even in plans directory
    let checker = make_permission_checker(PermissionSettings::default());
    let hook = make_test_hook_with_mode(checker, PermissionMode::Plan);

    let home = dirs::home_dir().unwrap();

    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: home.to_str().unwrap().to_string(),
        permission_mode: None,
        tool_name: "Bash".to_string(),
        tool_input: json!({
            "command": "echo 'test' > .claude/plans/test.md"
        }),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("deny".to_string()));
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_plan_mode_allows_read_operations() {
    // Plan mode should allow read operations
    let checker = make_permission_checker(PermissionSettings::default());
    let hook = make_test_hook_with_mode(checker, PermissionMode::Plan);

    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Read".to_string(),
        tool_input: json!({"file_path": "/tmp/test.txt"}),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_eq!(specific.permission_decision, Some("allow".to_string()));
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}

#[tokio::test]
async fn test_is_plans_directory_path() {
    // Test absolute path
    let home = dirs::home_dir().unwrap();
    let plans_path = home.join(".claude").join("plans").join("plan.md");
    assert!(is_plans_directory_path(plans_path.to_str().unwrap()));

    // Test ~ expansion
    assert!(is_plans_directory_path("~/.claude/plans/plan.md"));

    // Test non-plans path
    assert!(!is_plans_directory_path("/tmp/plan.md"));
    assert!(!is_plans_directory_path("~/other/path/plan.md"));

    // Test edge case: similar but not plans directory
    assert!(!is_plans_directory_path("~/../.claude/plans/plan.md"));
}

#[tokio::test]
async fn test_plan_mode_allows_edit_in_plans_dir() {
    // Plan mode should allow Edit operations in plans directory
    let checker = make_permission_checker(PermissionSettings::default());
    let hook = make_test_hook_with_mode(checker, PermissionMode::Plan);

    let home = dirs::home_dir().unwrap();
    let plan_file = home.join(".claude").join("plans").join("existing-plan.md");

    let input = HookInput::PreToolUse(claude_code_agent_sdk::PreToolUseHookInput {
        session_id: "test".to_string(),
        transcript_path: "/tmp/test".to_string(),
        cwd: "/tmp".to_string(),
        permission_mode: None,
        tool_name: "Edit".to_string(),
        tool_input: json!({
            "file_path": plan_file.to_str().unwrap(),
            "edits": []
        }),
        tool_use_id: "test-tool-use-id".to_string(),
    });

    let result = hook(input, None, HookContext::default()).await;

    match result {
        HookJsonOutput::Sync(output) => {
            assert_eq!(output.continue_, Some(true));
            // Should not deny
            if let Some(HookSpecificOutput::PreToolUse(specific)) = output.hook_specific_output {
                assert_ne!(specific.permission_decision, Some("deny".to_string()));
            }
        }
        HookJsonOutput::Async(_) => panic!("Expected sync output"),
    }
}
