use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn a_host_tool_is_known_by_its_meta_whatever_its_title() {
    let call = |title: &str, meta: Option<serde_json::Map<String, serde_json::Value>>| {
        let mut tool_call = acp::ToolCall::new("call-1", title)
            .raw_input(serde_json::json!({"paths": ["src/a.rs"]}));
        tool_call.meta = meta;
        nori_invocation(&tool_call)
    };
    assert_eq!(
        call(
            "git_diff",
            Some(nori_host_tools::tool_call_meta("git_diff"))
        ),
        Some(Invocation::Native(NativeCall::GitDiff {
            paths: vec!["src/a.rs".to_owned()]
        }))
    );
    assert_eq!(
        call(
            "Tool: anything",
            Some(nori_host_tools::tool_call_meta("task_list"))
        ),
        Some(Invocation::Delegation(DelegationCall::List))
    );
    // Without the meta, a bare name is not enough: it must come from `nori-client`.
    assert_eq!(call("git_diff", None), None);
    assert_eq!(
        call("mcp__nori-client__git_diff", None),
        Some(Invocation::Native(NativeCall::GitDiff {
            paths: vec!["src/a.rs".to_owned()]
        }))
    );
    // A name Nori does not have is left to the generic path.
    assert_eq!(
        call("x", Some(nori_host_tools::tool_call_meta("rm_rf"))),
        None
    );
}

#[test]
fn resources_say_whether_they_are_markdown() {
    assert_eq!(
        TextFormat::of_resource(Some("text/markdown"), "x"),
        TextFormat::Markdown
    );
    assert_eq!(
        TextFormat::of_resource(Some("Text/Markdown; charset=utf-8"), "x"),
        TextFormat::Markdown
    );
    assert_eq!(
        TextFormat::of_resource(Some("text/x-markdown"), "x"),
        TextFormat::Markdown
    );
    assert_eq!(
        TextFormat::of_resource(Some("text/plain"), "notes.md"),
        TextFormat::Plain
    );
    assert_eq!(
        TextFormat::of_resource(None, "file:///repo/README.MD"),
        TextFormat::Markdown
    );
    assert_eq!(
        TextFormat::of_resource(None, "docs/guide.mdx?raw=1"),
        TextFormat::Markdown
    );
    assert_eq!(
        TextFormat::of_resource(None, "src/main.rs"),
        TextFormat::Plain
    );
}

fn sample_permission_options() -> Vec<acp::PermissionOption> {
    vec![
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("allow-once"),
            "Allow",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("reject-once"),
            "Reject",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ]
}

fn sample_config_option() -> acp::SessionConfigOption {
    acp::SessionConfigOption::select(
        "verbosity",
        "Verbosity",
        "normal",
        vec![
            acp::SessionConfigSelectOption::new("normal", "Normal"),
            acp::SessionConfigSelectOption::new("verbose", "Verbose"),
        ],
    )
}

#[test]
fn normalizer_merges_placeholder_tool_call_with_refined_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let placeholder = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(acp::ToolCallId::new("tool-1"), "Edit")
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::Pending),
    );
    let refined = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-1"),
        acp::ToolCallUpdateFields::new()
            .title("Edit /repo/README.md")
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::InProgress)
            .locations(vec![acp::ToolCallLocation::new("/repo/README.md")])
            .raw_input(serde_json::json!({
                "file_path": "/repo/README.md",
                "old_string": "before\n",
                "new_string": "after\n",
            }))
            .content(vec![
                acp::Diff::new("/repo/README.md", "after\n")
                    .old_text("before\n")
                    .into(),
            ]),
    ));
    let completed = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-1"),
        acp::ToolCallUpdateFields::new()
            .status(acp::ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"success": true})),
    ));

    assert!(normalizer.push_session_update(&placeholder).is_empty());

    let refined_events = normalizer.push_session_update(&refined);
    assert_eq!(refined_events.len(), 1);

    let completed_events = normalizer.push_session_update(&completed);
    assert_eq!(completed_events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &completed_events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.call_id, "tool-1");
    assert_eq!(snapshot.title, "Edit /repo/README.md");
    assert_eq!(snapshot.kind, ToolKind::Edit);
    assert_eq!(snapshot.phase, ToolPhase::Completed);
    assert_eq!(
        snapshot.locations,
        vec![ToolLocation {
            path: "/repo/README.md".into(),
            line: None,
        }]
    );
    assert_eq!(
        snapshot.invocation,
        Some(Invocation::FileChanges {
            changes: vec![FileChange {
                path: "/repo/README.md".into(),
                old_text: Some("before\n".into()),
                new_text: "after\n".into(),
            }],
        })
    );
    assert_eq!(
        snapshot.artifacts,
        vec![Artifact::Diff(FileChange {
            path: "/repo/README.md".into(),
            old_text: Some("before\n".into()),
            new_text: "after\n".into(),
        })]
    );
    assert_eq!(
        snapshot.raw_output,
        Some(serde_json::json!({"success": true}))
    );
}

#[test]
fn normalizer_emits_approval_request_without_losing_diff_snapshot() {
    let mut normalizer = ClientEventNormalizer::default();
    let session_id = acp::SessionId::new("session-1");

    let request = acp::RequestPermissionRequest::new(
        session_id,
        acp::ToolCallUpdate::new(
            acp::ToolCallId::new("tool-approve"),
            acp::ToolCallUpdateFields::new()
                .title("Write /repo/tmp.md")
                .kind(acp::ToolKind::Edit)
                .locations(vec![acp::ToolCallLocation::new("/repo/tmp.md")])
                .raw_input(serde_json::json!({
                    "file_path": "/repo/tmp.md",
                    "content": "hello\n",
                }))
                .content(vec![acp::Diff::new("/repo/tmp.md", "hello\n").into()]),
        ),
        sample_permission_options(),
    );

    let events = normalizer.push_permission_request(&request);
    assert_eq!(events.len(), 1);

    let ClientEvent::ApprovalRequest(approval) = &events[0] else {
        panic!("expected approval request");
    };

    assert_eq!(approval.call_id, "tool-approve");
    assert_eq!(approval.title, "Write /repo/tmp.md");
    assert_eq!(approval.kind, ToolKind::Edit);
    assert_eq!(
        approval.subject,
        ApprovalSubject::ToolSnapshot(ToolSnapshot {
            call_id: "tool-approve".into(),
            title: "Write /repo/tmp.md".into(),
            kind: ToolKind::Edit,
            phase: ToolPhase::PendingApproval,
            locations: vec![ToolLocation {
                path: "/repo/tmp.md".into(),
                line: None,
            }],
            invocation: Some(Invocation::FileChanges {
                changes: vec![FileChange {
                    path: "/repo/tmp.md".into(),
                    old_text: None,
                    new_text: "hello\n".into(),
                }],
            }),
            artifacts: vec![Artifact::Diff(FileChange {
                path: "/repo/tmp.md".into(),
                old_text: None,
                new_text: "hello\n".into(),
            })],
            raw_input: Some(serde_json::json!({
                "file_path": "/repo/tmp.md",
                "content": "hello\n",
            })),
            raw_output: None,
            owner_request_id: None,
        })
    );
    assert_eq!(approval.options.len(), 2);
}

fn seed_tool_call(normalizer: &mut ClientEventNormalizer, call_id: &str) {
    let _ = normalizer.push_session_update(&acp::SessionUpdate::ToolCall(acp::ToolCall::new(
        acp::ToolCallId::new(call_id),
        "Terminal",
    )));
}

#[test]
fn normalizer_extracts_execute_invocation_and_output_text() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-exec");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-exec"),
        acp::ToolCallUpdateFields::new()
            .title("Terminal")
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": "cargo test -p nori-tui",
            }))
            .raw_output(serde_json::json!({
                "stdout": "all green\n",
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Command {
            command: "cargo test -p nori-tui".into(),
        })
    );
    assert_eq!(
        snapshot.artifacts,
        vec![Artifact::Text {
            text: "all green\n".into(),
            format: crate::presentation::TextFormat::Plain,
        }]
    );
}

#[test]
fn normalizer_emits_snapshot_for_unknown_tool_call_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-exec-orphan"),
        acp::ToolCallUpdateFields::new()
            .title("Terminal")
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": "git status",
            }))
            .raw_output(serde_json::json!({
                "stdout": "On branch spec\n",
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.call_id, "tool-exec-orphan");
    assert_eq!(snapshot.title, "Terminal");
    assert_eq!(snapshot.kind, ToolKind::Execute);
    assert_eq!(snapshot.phase, ToolPhase::Completed);
    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Command {
            command: "git status".into(),
        })
    );
    assert_eq!(
        snapshot.artifacts,
        vec![Artifact::Text {
            text: "On branch spec\n".into(),
            format: crate::presentation::TextFormat::Plain,
        }]
    );
}

#[test]
fn normalizer_extracts_read_invocation_path() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-read");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-read"),
        acp::ToolCallUpdateFields::new()
            .title("Read File")
            .kind(acp::ToolKind::Read)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "path": "Cargo.toml",
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Read {
            path: PathBuf::from("Cargo.toml"),
        })
    );
}

#[test]
fn normalizer_extracts_codex_execute_command_from_command_array() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-exec-codex");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-exec-codex"),
        acp::ToolCallUpdateFields::new()
            .title("Run df -h .")
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": ["/usr/bin/zsh", "-lc", "df -h ."],
                "cwd": "/repo",
                "parsed_cmd": [{
                    "cmd": "df -h .",
                    "type": "unknown",
                }],
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Command {
            command: "df -h .".into(),
        })
    );
}

#[test]
fn normalizer_preserves_non_shell_command_arrays() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-exec-array");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-exec-array"),
        acp::ToolCallUpdateFields::new()
            .title("Run ls -l")
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": ["ls", "-l"],
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Command {
            command: "ls -l".into(),
        })
    );
}

#[test]
fn normalizer_extracts_codex_read_path_from_parsed_command() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-read-codex");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-read-codex"),
        acp::ToolCallUpdateFields::new()
            .title("Read SKILL.md")
            .kind(acp::ToolKind::Read)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": ["/usr/bin/zsh", "-lc", "sed -n '1,220p' /repo/SKILL.md"],
                "cwd": "/repo",
                "parsed_cmd": [{
                    "cmd": "sed -n '1,220p' /repo/SKILL.md",
                    "name": "SKILL.md",
                    "path": "/repo/SKILL.md",
                    "type": "read",
                }],
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Read {
            path: PathBuf::from("/repo/SKILL.md"),
        })
    );
}

#[test]
fn normalizer_extracts_codex_read_path_from_later_parsed_command_entry() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-read-codex-later");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-read-codex-later"),
        acp::ToolCallUpdateFields::new()
            .title("Read file")
            .kind(acp::ToolKind::Read)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": ["/usr/bin/zsh", "-lc", "test -f /repo/SKILL.md && sed -n '1,20p' /repo/SKILL.md"],
                "parsed_cmd": [
                    {
                        "cmd": "test -f /repo/SKILL.md",
                        "type": "unknown",
                    },
                    {
                        "cmd": "sed -n '1,20p' /repo/SKILL.md",
                        "name": "SKILL.md",
                        "path": "/repo/SKILL.md",
                        "type": "read",
                    }
                ],
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Read {
            path: PathBuf::from("/repo/SKILL.md"),
        })
    );
}

#[test]
fn normalizer_extracts_codex_list_files_from_parsed_command() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-list-codex");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-list-codex"),
        acp::ToolCallUpdateFields::new()
            .title("List files")
            .kind(acp::ToolKind::Search)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": ["/usr/bin/zsh", "-lc", "find /repo/src -maxdepth 1"],
                "cwd": "/repo",
                "parsed_cmd": [{
                    "cmd": "find /repo/src -maxdepth 1",
                    "path": "/repo/src",
                    "type": "list_files",
                }],
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::ListFiles {
            path: Some(PathBuf::from("/repo/src")),
        })
    );
}

#[test]
fn normalizer_extracts_codex_search_from_later_parsed_command_entry() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-search-codex-later");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-search-codex-later"),
        acp::ToolCallUpdateFields::new()
            .title("Search files")
            .kind(acp::ToolKind::Search)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": ["/usr/bin/zsh", "-lc", "cd /repo && rg needle src"],
                "parsed_cmd": [
                    {
                        "cmd": "cd /repo",
                        "type": "unknown",
                    },
                    {
                        "cmd": "rg needle src",
                        "path": "src",
                        "query": "needle",
                        "type": "search",
                    }
                ],
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Search {
            query: Some("needle".into()),
            path: Some(PathBuf::from("src")),
        })
    );
}

#[test]
fn normalizer_extracts_codex_search_from_parsed_command() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-search-codex");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-search-codex"),
        acp::ToolCallUpdateFields::new()
            .title("Search files")
            .kind(acp::ToolKind::Search)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "command": ["/usr/bin/zsh", "-lc", "rg structured_invocation /repo/src"],
                "cwd": "/repo",
                "parsed_cmd": [{
                    "cmd": "rg structured_invocation /repo/src",
                    "path": "/repo/src",
                    "type": "search",
                }],
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Search {
            query: Some("rg structured_invocation /repo/src".into()),
            path: Some(PathBuf::from("/repo/src")),
        })
    );
}

#[test]
fn normalizer_emits_answer_message_delta_from_agent_message_chunk() {
    let mut normalizer = ClientEventNormalizer::default();
    let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
        acp::ContentBlock::Text(acp::TextContent::new("Hello from ACP")),
    ));

    let events = normalizer.push_session_update(&update);

    assert_eq!(
        events,
        vec![ClientEvent::MessageDelta(MessageDelta {
            stream: MessageStream::Answer,
            message_id: None,
            delta: "Hello from ACP".into(),
        })]
    );
}

#[test]
fn normalizer_emits_reasoning_message_delta_from_agent_thought_chunk() {
    let mut normalizer = ClientEventNormalizer::default();
    let update = acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
        acp::ContentBlock::Text(acp::TextContent::new("**Analyzing** the repo")),
    ));

    let events = normalizer.push_session_update(&update);

    assert_eq!(
        events,
        vec![ClientEvent::MessageDelta(MessageDelta {
            stream: MessageStream::Reasoning,
            message_id: None,
            delta: "**Analyzing** the repo".into(),
        })]
    );
}

#[test]
fn normalizer_emits_plan_snapshot_from_plan_update() {
    let mut normalizer = ClientEventNormalizer::default();
    let update = acp::SessionUpdate::Plan(acp::Plan::new(vec![
        acp::PlanEntry::new(
            "Research ACP",
            acp::PlanEntryPriority::High,
            acp::PlanEntryStatus::Completed,
        ),
        acp::PlanEntry::new(
            "Implement normalized flow",
            acp::PlanEntryPriority::Medium,
            acp::PlanEntryStatus::InProgress,
        ),
        acp::PlanEntry::new(
            "Delete legacy translation",
            acp::PlanEntryPriority::Low,
            acp::PlanEntryStatus::Pending,
        ),
    ]));

    let events = normalizer.push_session_update(&update);

    assert_eq!(
        events,
        vec![ClientEvent::PlanSnapshot(PlanSnapshot {
            entries: vec![
                PlanEntry {
                    step: "Research ACP".into(),
                    status: PlanStatus::Completed,
                },
                PlanEntry {
                    step: "Implement normalized flow".into(),
                    status: PlanStatus::InProgress,
                },
                PlanEntry {
                    step: "Delete legacy translation".into(),
                    status: PlanStatus::Pending,
                },
            ],
        })]
    );
}

#[test]
fn normalizer_extracts_delete_file_operation() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-delete");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-delete"),
        acp::ToolCallUpdateFields::new()
            .title("Delete README.md")
            .kind(acp::ToolKind::Delete)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "path": "README.md",
                "content": "before\n",
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.kind, ToolKind::Delete);
    assert_eq!(
        snapshot.invocation,
        Some(Invocation::FileOperations {
            operations: vec![FileOperation::Delete {
                path: PathBuf::from("README.md"),
                old_text: Some("before\n".into()),
            }],
        })
    );
}

#[test]
fn normalizer_extracts_move_file_operation() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-move");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-move"),
        acp::ToolCallUpdateFields::new()
            .title("Move README.md")
            .kind(acp::ToolKind::Move)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "from": "README.md",
                "to": "docs/README.md",
                "content": "before\n",
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.kind, ToolKind::Move);
    assert_eq!(
        snapshot.invocation,
        Some(Invocation::FileOperations {
            operations: vec![FileOperation::Move {
                from_path: PathBuf::from("README.md"),
                to_path: PathBuf::from("docs/README.md"),
                old_text: Some("before\n".into()),
                new_text: Some("before\n".into()),
            }],
        })
    );
}

#[test]
fn normalizer_extracts_generic_tool_invocation_for_fetch() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-fetch");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-fetch"),
        acp::ToolCallUpdateFields::new()
            .title("Fetch")
            .kind(acp::ToolKind::Fetch)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "url": "https://example.com",
            })),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Tool {
            tool_name: "Fetch".into(),
            input: Some(serde_json::json!({
                "url": "https://example.com",
            })),
        })
    );
}

// --- Spec 08: Gemini Empty Content Fallback ---

#[test]
fn normalizer_synthesizes_read_invocation_from_location_when_no_raw_input() {
    let mut normalizer = ClientEventNormalizer::default();

    let tool_call = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(acp::ToolCallId::new("read-1"), "README.md")
            .kind(acp::ToolKind::Read)
            .status(acp::ToolCallStatus::Completed)
            .locations(vec![acp::ToolCallLocation::new("/repo/README.md")]),
    );

    let events = normalizer.push_session_update(&tool_call);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Read {
            path: "/repo/README.md".into(),
        })
    );
}

#[test]
fn normalizer_synthesizes_search_invocation_from_location_when_no_raw_input() {
    let mut normalizer = ClientEventNormalizer::default();

    let tool_call = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(acp::ToolCallId::new("search-1"), "Search src")
            .kind(acp::ToolKind::Search)
            .status(acp::ToolCallStatus::Completed)
            .locations(vec![acp::ToolCallLocation::new("/repo/src")]),
    );

    let events = normalizer.push_session_update(&tool_call);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(
        snapshot.invocation,
        Some(Invocation::Search {
            query: None,
            path: Some("/repo/src".into()),
        })
    );
}

#[test]
fn normalizer_does_not_synthesize_edit_invocation_from_location() {
    let mut normalizer = ClientEventNormalizer::default();

    let tool_call = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(acp::ToolCallId::new("edit-1"), "file.rs")
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::InProgress)
            .locations(vec![acp::ToolCallLocation::new("/repo/file.rs")]),
    );

    let events = normalizer.push_session_update(&tool_call);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    // Edit with only locations and no raw_input should NOT synthesize an
    // invocation — it falls through to the TUI's location-path fallback.
    assert_eq!(snapshot.invocation, None);
}

#[test]
fn normalizer_sanitizes_title_stripping_cwd_and_description() {
    let mut normalizer = ClientEventNormalizer::default();

    let tool_call = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(
            acp::ToolCallId::new("exec-1"),
            "echo \"hello\" [current working directory /home/user/project] (Create test file)",
        )
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::Completed)
        .raw_input(serde_json::json!({"command": "echo \"hello\""})),
    );

    let events = normalizer.push_session_update(&tool_call);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.title, "echo \"hello\"");
}

#[test]
fn normalizer_sanitizes_title_stripping_cwd_only() {
    let mut normalizer = ClientEventNormalizer::default();

    let tool_call = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(
            acp::ToolCallId::new("exec-2"),
            "ls -la [current working directory /home/user]",
        )
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::Completed)
        .raw_input(serde_json::json!({"command": "ls -la"})),
    );

    let events = normalizer.push_session_update(&tool_call);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.title, "ls -la");
}

#[test]
fn normalizer_title_without_brackets_passes_through() {
    let mut normalizer = ClientEventNormalizer::default();

    let tool_call = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(acp::ToolCallId::new("exec-3"), "echo \"hello world\"")
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({"command": "echo \"hello world\""})),
    );

    let events = normalizer.push_session_update(&tool_call);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.title, "echo \"hello world\"");
}

#[test]
fn normalizer_sanitizes_title_preserves_command_parens_without_description() {
    let mut normalizer = ClientEventNormalizer::default();

    let tool_call = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(
            acp::ToolCallId::new("exec-4"),
            "echo (hello) [current working directory /foo]",
        )
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::Completed)
        .raw_input(serde_json::json!({"command": "echo (hello)"})),
    );

    let events = normalizer.push_session_update(&tool_call);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.title, "echo (hello)");
}

#[test]
fn normalizer_converts_available_commands_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let update =
        acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(vec![
            acp::AvailableCommand::new("loop", "Run a prompt on a recurring interval"),
            acp::AvailableCommand::new("schedule", "Create scheduled remote agents").input(
                acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                    "cron expression",
                )),
            ),
        ]));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::AgentCommandsUpdate(commands_update) = &events[0] else {
        panic!("expected AgentCommandsUpdate");
    };

    assert_eq!(commands_update.commands.len(), 2);
    assert_eq!(commands_update.commands[0].name, "loop");
    assert_eq!(
        commands_update.commands[0].description,
        "Run a prompt on a recurring interval"
    );
    assert_eq!(commands_update.commands[0].input_hint, None);
    assert_eq!(commands_update.commands[1].name, "schedule");
    assert_eq!(
        commands_update.commands[1].input_hint,
        Some("cron expression".to_string())
    );
}

#[test]
fn normalizer_converts_empty_available_commands_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let update =
        acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(vec![]));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::AgentCommandsUpdate(commands_update) = &events[0] else {
        panic!("expected AgentCommandsUpdate");
    };

    assert_eq!(commands_update.commands.len(), 0);
}

#[test]
fn normalizer_converts_user_message_chunk() {
    let mut normalizer = ClientEventNormalizer::default();

    let update = acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
        acp::ContentBlock::Text(acp::TextContent::new("resume this session")),
    ));

    let events = normalizer.push_session_update(&update);

    assert_eq!(
        events,
        vec![ClientEvent::MessageDelta(MessageDelta {
            stream: MessageStream::User,
            message_id: None,
            delta: "resume this session".to_string(),
        })]
    );
}

#[test]
fn normalizer_renders_user_attachments_without_exposing_payloads() {
    let mut normalizer = ClientEventNormalizer::default();
    let message_id = acp::MessageId::new("user-attachment");
    let updates = [
        acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Image(acp::ImageContent::new(
                "base64-payload",
                "image/png",
            )))
            .message_id(message_id.clone()),
        ),
        acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                "notes",
                "file:///tmp/notes.md",
            )))
            .message_id(message_id),
        ),
    ];

    let events = updates
        .iter()
        .flat_map(|update| normalizer.push_session_update(update))
        .collect::<Vec<_>>();

    assert_eq!(
        events,
        vec![
            ClientEvent::MessageDelta(MessageDelta {
                stream: MessageStream::User,
                message_id: Some("user-attachment".to_string()),
                delta: " [image attachment]".to_string(),
            }),
            ClientEvent::MessageDelta(MessageDelta {
                stream: MessageStream::User,
                message_id: Some("user-attachment".to_string()),
                delta: " [resource: notes]".to_string(),
            }),
        ]
    );
}

#[test]
fn normalizer_converts_current_mode_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let update = acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("review"));

    let events = normalizer.push_session_update(&update);

    assert_eq!(
        events,
        vec![ClientEvent::SessionModeChanged(SessionModeChanged {
            current_mode_id: "review".to_string(),
        })]
    );
}

#[test]
fn normalizer_converts_config_option_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let update = acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(vec![
        sample_config_option(),
    ]));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::SessionConfigUpdate(update) = &events[0] else {
        panic!("expected SessionConfigUpdate");
    };

    assert_eq!(update.config_options, vec![sample_config_option()]);
}

#[test]
fn normalizer_converts_session_info_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let update = acp::SessionUpdate::SessionInfoUpdate(
        acp::SessionInfoUpdate::new()
            .title("Resume chat")
            .updated_at("2026-04-14T15:00:00Z"),
    );

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::SessionUpdateInfo(info) = &events[0] else {
        panic!("expected SessionUpdateInfo");
    };

    assert_eq!(info.kind, SessionUpdateKind::SessionInfo);
    assert_eq!(
        info.message,
        "Session info updated: title=\"Resume chat\"; updated_at=2026-04-14T15:00:00Z"
    );
    assert_eq!(info.hint, None);
}

#[test]
fn normalizer_converts_usage_update() {
    let mut normalizer = ClientEventNormalizer::default();

    let update = acp::SessionUpdate::UsageUpdate(
        acp::UsageUpdate::new(128, 4096).cost(acp::Cost::new(0.42, "USD")),
    );

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::SessionUpdateInfo(info) = &events[0] else {
        panic!("expected SessionUpdateInfo");
    };

    assert_eq!(info.kind, SessionUpdateKind::Usage);
    assert_eq!(
        info.usage,
        Some(session_runtime::SessionUsageState {
            used_tokens: 128,
            total_tokens: 4096,
            cost_display: Some("0.42 USD".to_string()),
        })
    );
    assert_eq!(
        info.message,
        "Session usage: 128 / 4096 tokens, cost 0.42 USD"
    );
    assert_eq!(info.hint, None);
}

// ── refine_edit_kind: Codex ACP sends kind:"edit" for create/delete/move ──

#[test]
fn codex_edit_with_changes_type_add_becomes_create() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-create");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-create"),
        acp::ToolCallUpdateFields::new()
            .title("Edit /repo/new-file.txt")
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "changes": {
                    "/repo/new-file.txt": {
                        "type": "add",
                        "content": "hello\n"
                    }
                }
            }))
            .content(vec![acp::Diff::new("/repo/new-file.txt", "hello\n").into()]),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.kind, ToolKind::Create);
}

#[test]
fn codex_edit_with_changes_type_delete_becomes_delete() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-delete");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-delete"),
        acp::ToolCallUpdateFields::new()
            .title("Edit /repo/old-file.txt")
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "changes": {
                    "/repo/old-file.txt": {
                        "type": "delete",
                        "content": "goodbye\n"
                    }
                }
            }))
            .content(vec![
                acp::Diff::new("/repo/old-file.txt", "")
                    .old_text("goodbye\n")
                    .into(),
            ]),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.kind, ToolKind::Delete);
}

#[test]
fn codex_edit_with_move_path_becomes_move() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-move");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-move"),
        acp::ToolCallUpdateFields::new()
            .title("Edit /repo/old-name.txt")
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "changes": {
                    "/repo/old-name.txt": {
                        "type": "update",
                        "unified_diff": "@@ -1 +1 @@\n old\n+new\n",
                        "move_path": "/repo/new-name.txt"
                    }
                }
            }))
            .content(vec![
                acp::Diff::new("/repo/old-name.txt", "new\n")
                    .old_text("old\n")
                    .into(),
            ]),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.kind, ToolKind::Move);
}

#[test]
fn codex_edit_with_null_move_path_stays_edit() {
    let mut normalizer = ClientEventNormalizer::default();
    seed_tool_call(&mut normalizer, "tool-edit-normal");

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("tool-edit-normal"),
        acp::ToolCallUpdateFields::new()
            .title("Edit /repo/file.txt")
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(serde_json::json!({
                "changes": {
                    "/repo/file.txt": {
                        "type": "update",
                        "unified_diff": "@@ -1 +1 @@\n-old\n+new\n",
                        "move_path": null
                    }
                }
            }))
            .content(vec![
                acp::Diff::new("/repo/file.txt", "new\n")
                    .old_text("old\n")
                    .into(),
            ]),
    ));

    let events = normalizer.push_session_update(&update);
    assert_eq!(events.len(), 1);

    let ClientEvent::ToolSnapshot(snapshot) = &events[0] else {
        panic!("expected tool snapshot");
    };

    assert_eq!(snapshot.kind, ToolKind::Edit);
}
