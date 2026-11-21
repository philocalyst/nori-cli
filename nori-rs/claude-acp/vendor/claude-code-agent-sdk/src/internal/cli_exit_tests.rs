//! A Claude Code CLI that goes away: played by small shell scripts in its place, so the SDK's real
//! process transport, background reader and control protocol are what is tested.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use futures::StreamExt;

use crate::ClaudeClient;
use crate::errors::ClaudeError;
use crate::errors::CliExitError;
use crate::internal::query_full::QueryFull;
use crate::internal::transport::SubprocessTransport;
use crate::internal::transport::Transport;
use crate::internal::transport::subprocess::QueryPrompt;
use crate::types::config::ClaudeAgentOptions;
use crate::types::messages::Message;
use crate::types::messages::UserContentBlock;

/// Shell that answers the control request on the line it just read with an empty success.
const ANSWER_CONTROL: &str = r#"id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id""#;

/// A script standing in for the `claude` CLI, in a directory of its own (with the file its
/// launches are logged to).
struct FakeCli {
    dir: PathBuf,
}

impl FakeCli {
    fn new(body: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fake-claude-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\n{body}\n",
            dir.join("launches").display()
        );
        let path = dir.join("claude");
        std::fs::write(&path, script).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self { dir }
    }

    fn options(&self) -> ClaudeAgentOptions {
        ClaudeAgentOptions {
            cli_path: Some(self.dir.join("claude")),
            cwd: Some(self.dir.clone()),
            skip_version_check: true,
            control_request_timeout: Some(Duration::from_secs(30)),
            ..ClaudeAgentOptions::default()
        }
    }

    /// The arguments of each launch, in order.
    fn launches(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("launches"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for FakeCli {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn started_query(cli: &FakeCli) -> QueryFull {
    let mut transport =
        SubprocessTransport::new(QueryPrompt::Streaming, cli.options()).expect("transport");
    transport.connect().await.expect("connect");
    let stdin = transport.get_stdin().expect("stdin");
    let mut query = QueryFull::new(Box::new(transport));
    query.set_stdin(stdin);
    query.start().await.expect("reader");
    query
}

/// Fault `stream-eof` at start: a CLI that dies before it answers fails the request at once, with
/// how it ended, instead of after the control-request timeout.
#[tokio::test]
async fn a_cli_that_exits_before_answering_fails_the_request_at_once_and_says_why() {
    let cli = FakeCli::new("read line\necho 'Error: the config is broken' >&2\nexit 3");
    let query = started_query(&cli).await;

    let started = Instant::now();
    let error = query
        .initialize(None, None)
        .await
        .expect_err("a CLI that exited cannot initialize");

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "waited {:?}",
        started.elapsed()
    );
    match error {
        ClaudeError::CliExited(exit) => assert_eq!(
            exit,
            CliExitError {
                exit_code: Some(3),
                stderr_tail: vec!["Error: the config is broken".to_owned()],
            }
        ),
        other => panic!("expected CliExited, got {other:?}"),
    }
    assert!(!query.is_active());
    // Every later request fails the same way, without writing to a dead pipe.
    assert!(matches!(
        query.interrupt().await,
        Err(ClaudeError::CliExited(_))
    ));
}

/// A refusal from the CLI is an error, never a success with an `error` field.
#[tokio::test]
async fn a_control_request_the_cli_refuses_is_an_error() {
    let cli = FakeCli::new(
        r#"read line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"model opus-9 is not available"}}\n' "$id"
read line"#,
    );
    let query = started_query(&cli).await;

    let error = query
        .set_model(Some("opus-9"))
        .await
        .expect_err("a refused model is not applied");

    assert_eq!(
        error.to_string(),
        "Control protocol error: Claude Code refused its set_model request: model opus-9 is not available"
    );
    // The fake CLI waits for more; letting go of it ends it.
    query.stop_reader();
}

/// Fault `stream-eof` mid-turn: the turn's stream ends with how the CLI ended, and the next query
/// starts a CLI that resumes the conversation by its id instead of starting it again.
#[tokio::test]
async fn a_cli_that_dies_mid_turn_ends_the_stream_and_the_next_query_resumes_the_conversation() {
    let cli = FakeCli::new(&format!(
        r#"read line
{ANSWER_CONTROL}
read line
printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"half an answer"}}]}}}}\n'
exit 1"#
    ));
    let mut options = cli.options();
    options
        .extra_args
        .insert("session-id".to_owned(), Some("conversation-1".to_owned()));
    let mut client = ClaudeClient::new(options);
    client.connect().await.expect("connect");

    client.query("hello").await.expect("query");
    let mut seen = Vec::new();
    {
        let mut stream = client.receive_response();
        while let Some(item) = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("the stream ends instead of hanging")
        {
            seen.push(item);
        }
    }
    assert!(matches!(seen.first(), Some(Ok(Message::Assistant(_)))));
    assert!(
        matches!(
            seen.last(),
            Some(Err(ClaudeError::CliExited(CliExitError {
                exit_code: Some(1),
                ..
            })))
        ),
        "{seen:?}"
    );

    client.query("are you there?").await.expect("a new CLI");
    let launches = cli.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(
        launches[0].contains("--session-id conversation-1"),
        "{launches:?}"
    );
    assert!(
        launches[1].contains("--resume conversation-1"),
        "{launches:?}"
    );
    assert!(!launches[1].contains("--session-id"), "{launches:?}");
    client.disconnect().await.ok();
}

/// A prompt with an image reaches the CLI as content blocks: the text and the image, base64 and
/// typed, in one user message.
#[tokio::test]
async fn an_image_prompt_reaches_the_cli_as_typed_content_blocks() {
    let cli = FakeCli::new(&format!(
        r#"read line
{ANSWER_CONTROL}
read line
printf '%s\n' "$line" > "$(dirname "$0")/prompt"
read line"#
    ));
    let mut client = ClaudeClient::new(cli.options());
    client.connect().await.expect("connect");

    client
        .query_with_content(vec![
            UserContentBlock::text("what is in this picture?"),
            UserContentBlock::image_base64("image/png", "iVBORw0KGgo=").expect("a png"),
        ])
        .await
        .expect("query");

    let path = cli.dir.join("prompt");
    let started = Instant::now();
    while !path.exists() && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let line = std::fs::read_to_string(&path).expect("the CLI read the prompt");
    let message: serde_json::Value = serde_json::from_str(line.trim()).expect("one JSON line");
    assert_eq!(message["type"], "user");
    assert_eq!(
        message["message"]["content"],
        serde_json::json!([
            {"type": "text", "text": "what is in this picture?"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}},
        ])
    );
    client.disconnect().await.ok();
}
