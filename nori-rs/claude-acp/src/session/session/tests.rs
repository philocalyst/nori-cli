use super::*;

fn test_config() -> AgentConfig {
    AgentConfig {
        cli_path: None,
        environment: Default::default(),
        base_url: None,
        api_key: None,
        model: None,
        small_fast_model: None,
        max_thinking_tokens: None,
    }
}

#[test]
fn test_session_new() {
    let session = Session::new(
        "test-session-1".to_string(),
        PathBuf::from("/tmp"),
        &test_config(),
        None,
        &SessionTools::default(),
    )
    .unwrap();

    assert_eq!(session.session_id, "test-session-1");
    assert_eq!(session.cwd, PathBuf::from("/tmp"));
    assert!(!session.is_connected());
}

#[tokio::test]
async fn test_session_cancel() {
    let session = Session::new(
        "test-session-2".to_string(),
        PathBuf::from("/tmp"),
        &test_config(),
        None,
        &SessionTools::default(),
    )
    .unwrap();

    // Note: Cancellation is now handled per-prompt via CancellationToken
    // This test just verifies that cancel() doesn't panic
    session.cancel().await;
}

#[tokio::test]
async fn test_session_permission_mode() {
    let session = Session::new(
        "test-session-3".to_string(),
        PathBuf::from("/tmp"),
        &test_config(),
        None,
        &SessionTools::default(),
    )
    .unwrap();

    // Default is Default mode (standard behavior with permission prompts)
    assert_eq!(session.permission_mode().await, PermissionMode::Default);
    session.set_permission_mode(PermissionMode::DontAsk).await;
    assert_eq!(session.permission_mode().await, PermissionMode::DontAsk);
}

#[test]
fn test_stable_cache_key_ordering() {
    use serde_json::json;

    // JSON objects with same content but different key ordering should produce same cache key
    let json1 = json!({"a": 1, "b": 2, "c": 3});
    let json2 = json!({"c": 3, "b": 2, "a": 1});
    let json3 = json!({"b": 2, "a": 1, "c": 3});

    let key1 = stable_cache_key(&json1);
    let key2 = stable_cache_key(&json2);
    let key3 = stable_cache_key(&json3);

    assert_eq!(
        key1, key2,
        "Different key ordering should produce same cache key"
    );
    assert_eq!(
        key2, key3,
        "Different key ordering should produce same cache key"
    );
}

#[test]
fn test_stable_cache_key_nested_objects() {
    use serde_json::json;

    // Nested objects should also be canonicalized
    let json1 = json!({
        "command": "cargo build",
        "options": {"a": 1, "b": 2}
    });
    let json2 = json!({
        "options": {"b": 2, "a": 1},
        "command": "cargo build"
    });

    let key1 = stable_cache_key(&json1);
    let key2 = stable_cache_key(&json2);

    assert_eq!(key1, key2, "Nested objects should also produce stable keys");
}

#[test]
fn test_stable_cache_key_arrays() {
    use serde_json::json;

    // Arrays with objects inside should be canonicalized
    let json1 = json!({
        "items": [{"a": 1, "b": 2}, {"c": 3, "d": 4}]
    });
    let json2 = json!({
        "items": [{"b": 2, "a": 1}, {"d": 4, "c": 3}]
    });

    let key1 = stable_cache_key(&json1);
    let key2 = stable_cache_key(&json2);

    assert_eq!(key1, key2, "Arrays with objects should produce stable keys");
}

#[test]
fn test_stable_cache_key_different_content() {
    use serde_json::json;

    // Different content should produce different keys
    let json1 = json!({"command": "cargo build"});
    let json2 = json!({"command": "cargo test"});

    let key1 = stable_cache_key(&json1);
    let key2 = stable_cache_key(&json2);

    assert_ne!(
        key1, key2,
        "Different content should produce different keys"
    );
}

/// Test Session::cleanup() with no processes
///
/// Verifies that cleanup succeeds when there are no MCP servers
/// or background processes to clean up.
#[tokio::test]
async fn test_session_cleanup_no_processes() {
    let session = Session::new(
        "test-cleanup-session".to_string(),
        PathBuf::from("/tmp"),
        &test_config(),
        None,
        &SessionTools::default(),
    )
    .unwrap();

    // Cleanup should succeed even with no processes
    let result = session.cleanup().await;
    assert!(result.is_ok(), "Cleanup with no processes should succeed");
}
