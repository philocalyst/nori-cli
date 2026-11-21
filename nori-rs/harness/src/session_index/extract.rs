//! Turning a session's records into a digest: merged turns of text, tool calls with their
//! commands and paths, the branch, and the model the agent advertised.
//!
//! An [`Extractor`] is fed a session's records in order and can stop and go on later: the index
//! keeps it beside the digest, so a session that grew is digested from where the last digest
//! stopped instead of from its first byte.
//!
//! The extractor holds only what can still change. A turn is *final* once nothing later can
//! touch it: a text turn when another turn follows it, a tool turn when its call finished (or a
//! person spoke after it, or it fell far behind). A final turn is cleaned up once and handed out
//! through [`Extractor::take_ready`] to be appended to the digest; what stays behind is the tail
//! of turns that are not final, a few at most, which is all the state a saved extractor carries.

use std::collections::HashMap;

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;
use serde::Deserialize;
use serde::Serialize;

use super::digest::DigestHeader;
use super::digest::DigestTurn;
use super::digest::SourceStamp;
use super::digest::TurnRole;
use crate::transcript::ContentBlock;
use crate::transcript::SessionMetaEntry;
use crate::transcript::TranscriptEntry;
use crate::transcript::TranscriptLine;

const TITLE_MAX_CHARS: usize = 120;

/// A journal record the extractor cannot fold into what it read before: a rewind or a fork
/// changes which earlier records are visible, so the digest is rebuilt from the visible records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NeedsRebuild;

/// The longest a merged turn of text grows: a message past this starts another turn, so the
/// turn that is not final yet, and the cursor that carries it, stay small.
const TURN_MAX_BYTES: usize = 64 * 1024;

/// A call that many turns behind the newest is no longer expected to report back.
const OPEN_CALL_WINDOW: i64 = 64;

/// What has been read of a session so far, in a form that can be saved and read on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Extractor {
    /// The first session metadata the session recorded.
    meta: Option<SessionMetaEntry>,
    /// The agent the latest backend binding names, which a provider handoff changes.
    bound_agent: Option<String>,
    /// The turns that are not final, as recorded: the first is at position `released`.
    tail: Vec<DigestTurn>,
    /// How many turns have been taken out of the tail, final or dropped.
    released: i64,
    /// Which turn (by position) each tool call that may still report back became, so its later
    /// updates and output land on it.
    calls: HashMap<String, i64>,
    /// The title the agent last gave the session, which is the better name when it gave one.
    agent_title: Option<String>,
    /// The model option of the last advertisement of options, and its current value with the
    /// user's later changes applied.
    model_option: Option<String>,
    model: Option<String>,
    /// Every agent session the conversation has run in, the first first.
    acp_session_ids: Vec<String>,
    /// What the final turns showed: how many were the person's, the first words asked, and
    /// whether the first message was the delegation brief (unknown until the person spoke).
    user_turns: i64,
    first_title: Option<String>,
    opens_with_brief: Option<bool>,
    /// Final turns not yet taken, already cleaned up. Never saved: they go to the digest first.
    #[serde(skip)]
    ready: Vec<DigestTurn>,
}

impl Extractor {
    /// Fold in one journal record. A rewind or fork cannot be folded in.
    pub(super) fn feed_record(
        &mut self,
        record: &nori_conversation::Record,
    ) -> Result<(), NeedsRebuild> {
        use nori_conversation::Event;
        match &record.event {
            Event::Operation { name, args } if name == "transcript-entry" => {
                // An entry kind from another version is skipped, as the loader skips it.
                if let Ok(line) = serde_json::from_str::<TranscriptLine>(args) {
                    self.feed_line(&line);
                }
            }
            Event::Command { name, payload } if name == "backend-bound" => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                    if let Some(agent) = value.get("agent").and_then(serde_json::Value::as_str) {
                        self.bound_agent = Some(agent.to_owned());
                    }
                    if let Some(id) = value
                        .get("acp_session_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        self.add_acp_session(id);
                    }
                }
            }
            Event::Command { name, payload } if name == "set-session-config-option" => {
                if let Ok(change) = serde_json::from_str::<serde_json::Value>(payload)
                    && let (Some(config_id), Some(value)) = (
                        change.get("config_id").and_then(serde_json::Value::as_str),
                        change.get("value").and_then(serde_json::Value::as_str),
                    )
                    && self.model_option.as_deref() == Some(config_id)
                {
                    self.model = Some(value.to_owned());
                }
            }
            Event::Rewind { .. } | Event::Fork { .. } => return Err(NeedsRebuild),
            Event::Operation { .. }
            | Event::Command { .. }
            | Event::Message { .. }
            | Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::SessionEvent { .. }
            | Event::Unknown { .. } => {}
        }
        Ok(())
    }

    /// Fold in one transcript line, from a journal record or a JSONL transcript.
    pub(super) fn feed_line(&mut self, line: &TranscriptLine) {
        match &line.entry {
            TranscriptEntry::SessionMeta(meta) => {
                if self.meta.is_none() {
                    // The first agent session is the one a parent's transcript names its child
                    // by, so it leads the list.
                    if let Some(id) = &meta.acp_session_id
                        && !self.acp_session_ids.contains(id)
                    {
                        self.acp_session_ids.insert(0, id.clone());
                    }
                    self.meta = Some(meta.clone());
                }
            }
            TranscriptEntry::User(user) => {
                self.push_text(TurnRole::User, &user.content, Join::Message);
            }
            TranscriptEntry::Assistant(assistant) => {
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text } => {
                            self.push_text(TurnRole::Assistant, text, Join::Message);
                        }
                        ContentBlock::Thinking { .. } => {}
                    }
                }
            }
            TranscriptEntry::SessionEvent(entry) if !entry.historical_evidence => {
                self.feed_event(&entry.event);
            }
            TranscriptEntry::SessionEvent(_)
            | TranscriptEntry::ClientEvent(_)
            | TranscriptEntry::ToolCall(_)
            | TranscriptEntry::ToolResult(_)
            | TranscriptEntry::PatchApply(_) => {}
        }
        self.settle();
    }

    fn add_acp_session(&mut self, id: &str) {
        if !self.acp_session_ids.iter().any(|known| known == id) {
            self.acp_session_ids.push(id.to_owned());
        }
    }

    /// Take the model option an advertisement of options carries, and its value. An
    /// advertisement without one leaves the model as it was.
    fn advertise(&mut self, options: &[acp::SessionConfigOption]) {
        let model = options.iter().find_map(|option| {
            let value = serde_json::to_value(option).ok()?;
            let is_model = value.get("id").and_then(serde_json::Value::as_str) == Some("model")
                || value.get("category").and_then(serde_json::Value::as_str) == Some("model");
            if !is_model {
                return None;
            }
            Some((
                value.get("id")?.as_str()?.to_owned(),
                value.get("currentValue")?.as_str()?.to_owned(),
            ))
        });
        if let Some((option, value)) = model {
            self.model_option = Some(option);
            self.model = Some(value);
        }
    }

    fn feed_event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Nori(NoriEvent::SessionStarted(started)) => {
                self.advertise(&started.config_options);
                self.add_acp_session(&started.acp_session_id.to_string());
            }
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::UserMessageChunk(chunk) => {
                    // A block meant for the assistant alone is Nori's own context, not a word
                    // the person said.
                    if let Some(text) = block_text(&chunk.content)
                        && !assistant_only(&chunk.content)
                    {
                        self.push_text(TurnRole::User, text, Join::Chunk);
                    }
                }
                acp::SessionUpdate::SessionInfoUpdate(info) => match info.title.as_opt_ref() {
                    Some(Some(title)) => {
                        self.agent_title = Some(title.clone())
                            .filter(|title| !crate::injected_context::user_words(title).is_empty());
                    }
                    Some(None) => self.agent_title = None,
                    None => {}
                },
                acp::SessionUpdate::ConfigOptionUpdate(update) => {
                    self.advertise(&update.config_options);
                }
                acp::SessionUpdate::AgentMessageChunk(chunk) => {
                    if let Some(text) = block_text(&chunk.content) {
                        self.push_text(TurnRole::Assistant, text, Join::Chunk);
                    }
                }
                acp::SessionUpdate::ToolCall(call) => {
                    let at = self.released + len_i64(&self.tail);
                    self.calls.insert(call.tool_call_id.to_string(), at);
                    self.tail.push(tool_turn(call));
                }
                acp::SessionUpdate::ToolCallUpdate(update) => {
                    let id = update.tool_call_id.to_string();
                    let turn = self
                        .calls
                        .get(&id)
                        .and_then(|at| usize::try_from(at - self.released).ok())
                        .and_then(|index| self.tail.get_mut(index));
                    if let Some(turn) = turn {
                        let finished = fill_from_update(turn, update);
                        if finished || turn.output.is_some() {
                            self.calls.remove(&id);
                        }
                    }
                }
                _ => {}
            },
            SessionEvent::Nori(_) | SessionEvent::Acp(_) => {}
        }
    }

    /// Add text to the turn before it when both are the same role, else as a turn of its own.
    fn push_text(&mut self, role: TurnRole, text: &str, join: Join) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(last) = self.tail.last_mut()
            && last.role == role
            && last.command.is_none()
            && last.text.len() < TURN_MAX_BYTES
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
        if role == TurnRole::User {
            // Someone spoke after them: calls made before are not waited for any more.
            self.calls.clear();
        }
        self.tail.push(DigestTurn {
            role,
            text: text.to_owned(),
            command: None,
            output: None,
        });
    }

    /// Move the turns nothing can touch any more out of the tail: all but the last, which a
    /// following chunk may extend, and those a call still reports into.
    fn settle(&mut self) {
        let end = self.released + len_i64(&self.tail);
        self.calls.retain(|_, at| *at + OPEN_CALL_WINDOW >= end);
        let waited_for: std::collections::HashSet<i64> = self.calls.values().copied().collect();
        let released = self.released;
        let settled = (0..self.tail.len().saturating_sub(1))
            .take_while(|index| {
                i64::try_from(*index).is_ok_and(|index| !waited_for.contains(&(released + index)))
            })
            .count();
        let rest = self.tail.split_off(settled);
        for turn in std::mem::replace(&mut self.tail, rest) {
            self.released += 1;
            let Some(turn) = cleaned(turn) else {
                continue;
            };
            if turn.role == TurnRole::User {
                self.user_turns += 1;
                self.note_user_turn(&turn.text);
            }
            self.ready.push(turn);
        }
    }

    /// What the first of the person's turns says about the session.
    fn note_user_turn(&mut self, text: &str) {
        if self.opens_with_brief.is_none() {
            self.opens_with_brief = Some(text.contains(DELEGATION_BRIEF_MARK));
        }
        if self.first_title.is_none() {
            self.first_title = title_of(text);
        }
    }

    /// The final turns not handed out yet, in order.
    pub(super) fn take_ready(&mut self) -> Vec<DigestTurn> {
        std::mem::take(&mut self.ready)
    }

    /// The turns that are not final yet, cleaned up as they will be when they are.
    pub(super) fn tail_turns(&self) -> Vec<DigestTurn> {
        self.tail.iter().cloned().filter_map(cleaned).collect()
    }

    /// The header of what has been read, under `project_id`, built from `source`. `None` until
    /// the session's metadata has been read.
    pub(super) fn header(&self, project_id: &str, source: SourceStamp) -> Option<DigestHeader> {
        let meta = self.meta.as_ref()?;
        let tail = self.tail_turns();
        // What was asked: the agent's title, else the first user message. A message that was
        // only injected context names nothing.
        let title = self
            .agent_title
            .as_deref()
            .and_then(title_of)
            .or_else(|| self.first_title.clone())
            .or_else(|| {
                tail.iter()
                    .filter(|turn| turn.role == TurnRole::User)
                    .find_map(|turn| title_of(&turn.text))
            });
        let unfinished = tail
            .iter()
            .filter(|turn| turn.role == TurnRole::User)
            .count();
        let user_turns = self.user_turns + i64::try_from(unfinished).unwrap_or(i64::MAX);
        Some(DigestHeader {
            schema: super::digest::DIGEST_SCHEMA,
            session_id: meta.session_id.clone(),
            project_id: project_id.to_owned(),
            started_at: meta.started_at.clone(),
            cwd: meta.cwd.clone(),
            agent: self.bound_agent.clone().or_else(|| meta.agent.clone()),
            model: self.model.clone(),
            branch: meta.git.as_ref().and_then(|git| git.branch.clone()),
            title,
            user_turns,
            acp_session_ids: self.acp_session_ids.clone(),
            delegated: meta.delegated
                || self
                    .opens_with_brief
                    .unwrap_or_else(|| opens_with_delegation_brief(&tail)),
            source,
            indexed_at: chrono::Utc::now().to_rfc3339(),
        })
    }
}

fn len_i64<T>(items: &[T]) -> i64 {
    i64::try_from(items.len()).unwrap_or(i64::MAX)
}

/// The first words of `text` as a session's title, when it has words that are the person's.
fn title_of(text: &str) -> Option<String> {
    let words = crate::injected_context::user_words(text);
    (!words.is_empty()).then(|| words.chars().take(TITLE_MAX_CHARS).collect())
}

/// What a turn shows once it is final: a message recorded as a record and again as the agent's
/// echo of it reads once, Nori's own context is not part of what anyone said (it is not shown,
/// titled or searched), and a message that was only that context is no turn at all.
fn cleaned(mut turn: DigestTurn) -> Option<DigestTurn> {
    let middle = turn.text.len() / 2;
    if turn.role != TurnRole::Tool
        && turn.text.len().is_multiple_of(2)
        && turn.text.is_char_boundary(middle)
        && turn.text[..middle] == turn.text[middle..]
    {
        turn.text.truncate(middle);
    }
    if turn.role == TurnRole::User {
        turn.text = crate::injected_context::without_injected(&turn.text);
        if turn.text.is_empty() {
            return None;
        }
    }
    Some(turn)
}

/// How a piece of text joins the turn before it, when both are the same role.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Join {
    /// A whole message: a new paragraph.
    Message,
    /// A streamed chunk of one message: the text continues where the last chunk stopped.
    Chunk,
}

/// Whether the block's annotations say it is for the assistant and not for the person.
fn assistant_only(block: &acp::ContentBlock) -> bool {
    match block {
        acp::ContentBlock::Text(text) => text
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.audience.as_ref())
            .is_some_and(|audience| !audience.contains(&acp::Role::User)),
        _ => false,
    }
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
/// not carry yet, a title where the call had none, and the start of the output. Whether the
/// update says the call is over.
fn fill_from_update(turn: &mut DigestTurn, update: &acp::ToolCallUpdate) -> bool {
    let Ok(value) = serde_json::to_value(update) else {
        return false;
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
    matches!(
        value.get("status").and_then(serde_json::Value::as_str),
        Some("completed" | "failed")
    )
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
