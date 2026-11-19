//! Turning a loaded transcript into a digest: merged turns of text, tool calls with their
//! commands and paths, the branch, and the model the agent advertised.

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

use super::digest::DigestHeader;
use super::digest::DigestTurn;
use super::digest::SessionDigest;
use super::digest::SourceStamp;
use super::digest::TurnRole;
use crate::transcript::Transcript;
use crate::transcript::TranscriptRecord;

const TITLE_MAX_CHARS: usize = 120;

pub(super) fn extract(
    transcript: &Transcript,
    project_id: &str,
    source: SourceStamp,
) -> SessionDigest {
    let mut turns: Vec<DigestTurn> = Vec::new();
    let mut model: Option<String> = None;
    // Which turn each tool call became, so its output lands on it.
    let mut calls: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for record in transcript.records() {
        match record {
            TranscriptRecord::User { content } => {
                push_text(&mut turns, TurnRole::User, content, Join::Message);
            }
            TranscriptRecord::Assistant { content } => {
                push_text(&mut turns, TurnRole::Assistant, content, Join::Message);
            }
            TranscriptRecord::Thinking { .. } => {}
            TranscriptRecord::SessionEvent(SessionEvent::Nori(NoriEvent::SessionStarted(
                started,
            ))) => {
                model = advertised_model(&started.config_options).or(model);
            }
            TranscriptRecord::SessionEvent(SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            ))) => match &notification.update {
                acp::SessionUpdate::UserMessageChunk(chunk) => {
                    if let Some(text) = block_text(&chunk.content) {
                        push_text(&mut turns, TurnRole::User, text, Join::Chunk);
                    }
                }
                acp::SessionUpdate::AgentMessageChunk(chunk) => {
                    if let Some(text) = block_text(&chunk.content) {
                        push_text(&mut turns, TurnRole::Assistant, text, Join::Chunk);
                    }
                }
                acp::SessionUpdate::ToolCall(call) => {
                    calls.insert(call.tool_call_id.to_string(), turns.len());
                    turns.push(tool_turn(call));
                }
                acp::SessionUpdate::ToolCallUpdate(update) => {
                    let turn = calls
                        .get(&update.tool_call_id.to_string())
                        .and_then(|index| turns.get_mut(*index));
                    if let Some(turn) = turn {
                        fill_from_update(turn, update);
                    }
                }
                _ => {}
            },
            TranscriptRecord::SessionEvent(_) => {}
        }
    }
    // A message recorded as a record and again as the agent's echo of it reads twice over.
    for turn in &mut turns {
        let middle = turn.text.len() / 2;
        if turn.role != TurnRole::Tool
            && turn.text.len() % 2 == 0
            && turn.text.is_char_boundary(middle)
            && turn.text[..middle] == turn.text[middle..]
        {
            turn.text.truncate(middle);
        }
    }
    let title = turns
        .iter()
        .find(|turn| turn.role == TurnRole::User)
        .and_then(|turn| turn.text.lines().find(|line| !line.trim().is_empty()))
        .map(|line| line.trim().chars().take(TITLE_MAX_CHARS).collect());
    let user_turns = turns
        .iter()
        .filter(|turn| turn.role == TurnRole::User)
        .count();
    let meta = &transcript.meta;
    SessionDigest {
        header: DigestHeader {
            schema: super::digest::DIGEST_SCHEMA,
            session_id: meta.session_id.clone(),
            project_id: project_id.to_owned(),
            started_at: meta.started_at.clone(),
            cwd: meta.cwd.clone(),
            agent: meta.agent.clone(),
            model,
            branch: meta.git.as_ref().and_then(|git| git.branch.clone()),
            title,
            user_turns: i64::try_from(user_turns).unwrap_or(i64::MAX),
            acp_session_id: meta.acp_session_id.clone(),
            delegated: meta.delegated || opens_with_delegation_brief(&turns),
            source,
            indexed_at: chrono::Utc::now().to_rfc3339(),
        },
        turns,
    }
}

/// How a piece of text joins the turn before it, when both are the same role.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Join {
    /// A whole message: a new paragraph.
    Message,
    /// A streamed chunk of one message: the text continues where the last chunk stopped.
    Chunk,
}

fn push_text(turns: &mut Vec<DigestTurn>, role: TurnRole, text: &str, join: Join) {
    if text.trim().is_empty() {
        return;
    }
    if let Some(last) = turns.last_mut()
        && last.role == role
        && last.command.is_none()
    {
        // The same message recorded twice (as a record and as the agent's echo) is one turn.
        if join == Join::Message && last.text.ends_with(text) {
            return;
        }
        if join == Join::Message && !last.text.is_empty() {
            last.text.push('\n');
        }
        last.text.push_str(text);
        return;
    }
    turns.push(DigestTurn {
        role,
        text: text.to_owned(),
        command: None,
        output: None,
    });
}

fn block_text(block: &acp::ContentBlock) -> Option<&str> {
    match block {
        acp::ContentBlock::Text(text) => Some(&text.text),
        _ => None,
    }
}

fn tool_turn(call: &acp::ToolCall) -> DigestTurn {
    let mut label = call.title.clone();
    let paths: Vec<String> = call
        .locations
        .iter()
        .map(|location| location.path.display().to_string())
        .collect();
    let input_path = string_field(
        call.raw_input.as_ref(),
        &["path", "file_path", "filePath", "target_file"],
    );
    for path in paths.iter().chain(input_path.iter()) {
        if !label.contains(path.as_str()) {
            label.push_str(" · ");
            label.push_str(path);
        }
    }
    DigestTurn {
        role: TurnRole::Tool,
        text: label,
        command: string_field(call.raw_input.as_ref(), &["command", "cmd"]),
        output: None,
    }
}

/// The first of `keys` that is a string in `raw`, which is an object or a string holding one.
fn string_field(raw: Option<&serde_json::Value>, keys: &[&str]) -> Option<String> {
    let parsed;
    let object = match raw? {
        serde_json::Value::Object(object) => object,
        serde_json::Value::String(text) => {
            parsed = serde_json::from_str::<serde_json::Value>(text).ok()?;
            parsed.as_object()?
        }
        _ => return None,
    };
    keys.iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

/// The model among the options an agent advertised, if it advertised one.
fn advertised_model(options: &[acp::SessionConfigOption]) -> Option<String> {
    options.iter().find_map(|option| {
        let value = serde_json::to_value(option).ok()?;
        let is_model = value.get("id").and_then(serde_json::Value::as_str) == Some("model")
            || value.get("category").and_then(serde_json::Value::as_str) == Some("model");
        is_model
            .then(|| value.get("currentValue")?.as_str().map(str::to_owned))
            .flatten()
    })
}

/// How much of a tool's output is kept: enough to find an error message, not a build log.
const OUTPUT_MAX_CHARS: usize = 600;

/// The text a tool call update carries: content blocks first, else the raw output. Read through
/// the serialized form so that every ACP schema revision's shape is understood.
fn tool_output(update: &acp::ToolCallUpdate) -> Option<String> {
    let value = serde_json::to_value(update).ok()?;
    let mut text = String::new();
    if let Some(blocks) = value.get("content").and_then(serde_json::Value::as_array) {
        for block in blocks {
            if let Some(part) = block
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(serde_json::Value::as_str)
            {
                text.push_str(part);
                text.push('\n');
            }
        }
    }
    if text.trim().is_empty() {
        let raw = value.get("rawOutput")?;
        text = match raw {
            serde_json::Value::String(output) => output.clone(),
            serde_json::Value::Object(object) => [
                "output",
                "stdout",
                "aggregated_output",
                "formatted_output",
                "text",
            ]
            .iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))?
            .to_owned(),
            _ => return None,
        };
    }
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.chars().take(OUTPUT_MAX_CHARS).collect())
}

/// What a tool call update adds to the turn its call became: the command or path the call did
/// not carry yet, a title where the call had none, and the start of the output.
fn fill_from_update(turn: &mut DigestTurn, update: &acp::ToolCallUpdate) {
    let Ok(value) = serde_json::to_value(update) else {
        return;
    };
    if turn.command.is_none() {
        turn.command = string_field(value.get("rawInput"), &["command", "cmd"]);
    }
    if turn.text.trim().is_empty()
        && let Some(title) = value.get("title").and_then(serde_json::Value::as_str)
    {
        turn.text = title.to_owned();
    }
    if let Some(path) = string_field(
        value.get("rawInput"),
        &["path", "file_path", "filePath", "target_file"],
    ) && !turn.text.contains(path.as_str())
    {
        turn.text.push_str(" · ");
        turn.text.push_str(&path);
    }
    if turn.output.is_none() {
        turn.output = tool_output(update);
    }
}

/// What Nori's delegation brief says first. Transcripts recorded before children were marked
/// are recognized by it: a session whose first message is the brief is a delegated child.
const DELEGATION_BRIEF_MARK: &str = "Another agent delegated this task to you.";

fn opens_with_delegation_brief(turns: &[DigestTurn]) -> bool {
    turns
        .iter()
        .find(|turn| turn.role == TurnRole::User)
        .is_some_and(|turn| turn.text.contains(DELEGATION_BRIEF_MARK))
}
