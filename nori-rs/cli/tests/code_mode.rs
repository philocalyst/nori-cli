#![cfg(all(feature = "native-codex", unix))]

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolInvocationFuture;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Delegate;
impl CodeModeSessionDelegate for Delegate {
    fn invoke_tool<'a>(
        &'a self,
        call: CodeModeNestedToolCall,
        _: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            assert_eq!(call.tool_name, ToolName::plain("test_tool"));
            Ok(serde_json::json!({"value": "from host tool"}))
        })
    }
    fn notify<'a>(
        &'a self,
        _: String,
        _: CellId,
        _: String,
        _: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn cell_closed(&self, _: &CellId) {}
}

#[tokio::test]
async fn nori_hosts_code_mode_and_relays_nested_tools() -> Result<(), Box<dyn std::error::Error>> {
    // This wrapper selects Nori's hidden entry point and forwards the transport
    // arguments in the same way as the in-process ACP provider.
    let dir = tempfile::tempdir()?;
    let wrapper = dir.path().join("host");
    let binary_path = std::path::PathBuf::from(env!("CARGO_BIN_EXE_nori"));
    codex_install_context::set_code_mode_host_program(binary_path.clone());
    assert_eq!(
        codex_install_context::InstallContext::current().code_mode_host_program(),
        binary_path
    );
    let binary = env!("CARGO_BIN_EXE_nori").replace('\'', "'\\''");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec '{binary}' --nori-code-mode-host \"$@\"\n"),
    )?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
    let providers: Vec<Box<dyn CodeModeSessionProvider>> = vec![
        Box::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
            wrapper.clone(),
        )),
        Box::new(codex_core::ProcessOwnedGrpcCodeModeSessionProvider::with_host_program(wrapper)),
    ];
    for provider in providers {
        let session = provider.create_session().await?;
        let request = ExecuteRequest {
            tool_call_id: "test".into(),
            enabled_tools: vec![ToolDefinition {
                name: "test_tool".into(),
                tool_name: ToolName::plain("test_tool"),
                description: String::new(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                input_schema_max_bytes: None,
                output_schema: None,
            }],
            source: "const result = await tools.test_tool({}); text(result.value);".into(),
            yield_time_ms: Some(60_000),
            max_output_tokens: None,
        };
        let response = session
            .execute(request, Arc::new(Delegate), None)
            .await?
            .initial_response()
            .await?;
        match response {
            RuntimeResponse::Result {
                content_items,
                error_text,
                ..
            } => {
                assert_eq!(error_text, None);
                assert_eq!(
                    content_items,
                    vec![FunctionCallOutputContentItem::InputText {
                        text: "from host tool".into()
                    }]
                );
            }
            other => panic!("expected completed code mode execution, got {other:?}"),
        }
        session.shutdown().await?;
    }
    Ok(())
}
