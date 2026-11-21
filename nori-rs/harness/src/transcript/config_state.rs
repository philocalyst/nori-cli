//! The session configuration a conversation last had: the model, the effort and the mode it
//! stopped on, which a resume that has to start a new agent session puts back.
//!
//! Two records say it. The options the agent advertised (when the session started, and in each
//! `config_option_update`) are in the transcript's session events. A change the user made goes
//! through `session/set_config_option`, whose response is journaled too but reads back as another
//! response type (a response carrying only `configOptions` is ambiguous), so the journal's own
//! `set-session-config-option` command, which names the option and the value, is what is read.

use std::path::Path;

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

use super::Transcript;
use super::TranscriptEntry;
use super::TranscriptLine;
use super::TranscriptRecord;

/// The options an advertisement of them (a session start or an update) carries.
fn advertised(event: &SessionEvent) -> Option<&[acp::SessionConfigOption]> {
    match event {
        SessionEvent::Nori(NoriEvent::SessionStarted(started))
            if !started.config_options.is_empty() =>
        {
            Some(&started.config_options)
        }
        SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
            notification,
        ))) => {
            if let acp::SessionUpdate::ConfigOptionUpdate(update) = &notification.update {
                Some(&update.config_options)
            } else {
                None
            }
        }
        SessionEvent::Nori(_) | SessionEvent::Acp(_) => None,
    }
}

/// `options` with the select option `config_id` set to `value`.
fn with_value(options: &mut [acp::SessionConfigOption], config_id: &str, value: &str) {
    for option in options.iter_mut() {
        if option.id.to_string() == config_id
            && let acp::SessionConfigKind::Select(select) = &mut option.kind
        {
            select.current_value = value.to_owned().into();
        }
    }
}

impl Transcript {
    /// The agent's session config options as the advertisements in this transcript left them.
    /// A user's change that was never advertised is not here; see [`recorded_config_options`].
    pub fn last_config_options(&self) -> Vec<acp::SessionConfigOption> {
        let mut options: Vec<acp::SessionConfigOption> = Vec::new();
        for record in self.records() {
            if let TranscriptRecord::SessionEvent(event) = record
                && let Some(advertised) = advertised(event)
            {
                options = advertised.to_vec();
            }
        }
        options
    }
}

/// The config options a conversation ended on: the last advertisement of them with every change
/// the user made after it applied, read from the journal beside `transcript_path` when there is
/// one, else from `transcript`'s own records. Blocking.
pub(crate) fn recorded_config_options(
    transcript: &Transcript,
    transcript_path: &Path,
) -> Vec<acp::SessionConfigOption> {
    let journal = transcript_path.with_extension("pkl");
    if !journal.exists() {
        return transcript.last_config_options();
    }
    newest_config_options(&journal).unwrap_or_else(|_| transcript.last_config_options())
}

/// The newest advertisement on the visible branch of `journal`, read back from its end, with the
/// changes the user made after it applied. Only the entries after that advertisement and the
/// change commands are decoded, so a long conversation costs its last stretch.
fn newest_config_options(journal: &Path) -> std::io::Result<Vec<acp::SessionConfigOption>> {
    use nori_conversation::Event;
    use nori_conversation::Kind;
    let log = nori_conversation::ConversationLog::open(journal)?;
    let entries = log.visible_named(Kind::Operation, "transcript-entry");
    let mut options: Vec<acp::SessionConfigOption> = Vec::new();
    let mut since = 0;
    for record in log.records(entries.iter().rev().copied()) {
        let record = record?;
        if let Event::Operation { args, .. } = &record.event
            && let Ok(TranscriptLine {
                entry: TranscriptEntry::SessionEvent(entry),
                ..
            }) = serde_json::from_str::<TranscriptLine>(args)
            && !entry.historical_evidence
            && let Some(advertised) = advertised(&entry.event)
        {
            options = advertised.to_vec();
            since = record.id;
            break;
        }
    }
    let changes = log
        .visible_named(Kind::Command, "set-session-config-option")
        .into_iter()
        .filter(|id| *id > since);
    for record in log.records(changes) {
        if let Event::Command { payload, .. } = &record?.event
            && let Ok(change) = serde_json::from_str::<serde_json::Value>(payload)
            && let (Some(config_id), Some(value)) = (
                change.get("config_id").and_then(serde_json::Value::as_str),
                change.get("value").and_then(serde_json::Value::as_str),
            )
        {
            with_value(&mut options, config_id, value);
        }
    }
    Ok(options)
}

/// The options of the newest `config_option_update` on the visible branch of the journal beside
/// `transcript_path`. The update names itself in its line, so only the lines that say
/// `config_option_update` are decoded. Blocking.
pub(crate) fn newest_config_option_update(
    transcript_path: &Path,
) -> std::io::Result<Option<Vec<acp::SessionConfigOption>>> {
    use nori_conversation::Event;
    let journal = transcript_path.with_extension("pkl");
    if !journal.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the session has no conversation journal",
        ));
    }
    let log = nori_conversation::ConversationLog::open(journal)?;
    let mentions = log.visible_containing("config_option_update")?;
    for record in log.records(mentions.into_iter().rev()) {
        if let Event::Operation { name, args } = &record?.event
            && name == "transcript-entry"
            && let Ok(TranscriptLine {
                entry: TranscriptEntry::SessionEvent(entry),
                ..
            }) = serde_json::from_str::<TranscriptLine>(args)
            && let SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) = &entry.event
            && let acp::SessionUpdate::ConfigOptionUpdate(update) = &notification.update
        {
            return Ok(Some(update.config_options.clone()));
        }
    }
    Ok(None)
}
