//! Sessions on disk: one append-only Nori conversation journal per session.
//!
//! `$NORI_HOME/agents/<agent>/sessions/<id>.pkl` holds, in order: the session record (where it
//! works, when it began, what it was forked from), then every message as the provider-neutral
//! conversation the model reads (reasoning proofs included, so a resumed session continues
//! exactly), interleaved with the configuration whenever it changes and the title when one is
//! set. A session is resumed by replaying its journal; nothing is ever rewritten.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use nori_acp_kit::AgentError;
use nori_conversation::ConversationLog;
use nori_conversation::Event;
use nori_llm::Message;
use nori_llm::Role;
use serde::Deserialize;
use serde::Serialize;

const SESSION: &str = "session";
const CONFIG: &str = "config";
const TITLE: &str = "title";

/// What a session is, as its first record says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: PathBuf,
    /// Seconds since the Unix epoch.
    pub created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

/// The session's dials, as last recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    /// The flavor's other dials (`service_tier`), by option id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

/// A session read back from its journal.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    pub config: SessionConfig,
    pub title: Option<String>,
}

/// A session as a listing shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub meta: SessionMeta,
    pub title: Option<String>,
    pub updated: SystemTime,
}

fn internal(error: impl std::fmt::Display) -> AgentError {
    AgentError::Internal(format!("session journal: {error}"))
}

/// Where an agent's sessions live.
#[derive(Debug, Clone)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            directory: root.join("sessions"),
        }
    }

    fn path(&self, id: &str) -> Result<PathBuf, AgentError> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(AgentError::unknown_session(id));
        }
        Ok(self.directory.join(format!("{id}.pkl")))
    }

    /// Start a new session's journal.
    pub async fn create(&self, meta: SessionMeta) -> Result<Journal, AgentError> {
        let path = self.path(&meta.id)?;
        let directory = self.directory.clone();
        run(move || {
            std::fs::create_dir_all(&directory).map_err(internal)?;
            if path.exists() {
                return Err(AgentError::invalid(format!(
                    "session {} already exists",
                    meta.id
                )));
            }
            let mut log = ConversationLog::open(&path).map_err(internal)?;
            log.append(command(SESSION, &meta)?).map_err(internal)?;
            Ok(Journal {
                log: Arc::new(Mutex::new(log)),
            })
        })
        .await
    }

    /// Read a session back, and the journal to go on writing it.
    pub async fn open(&self, id: &str) -> Result<(Recorded, Journal), AgentError> {
        let path = self.path(id)?;
        let id = id.to_owned();
        run(move || {
            if !path.is_file() {
                return Err(AgentError::unknown_session(&id));
            }
            let log = ConversationLog::open(&path).map_err(internal)?;
            let recorded = replay(&log, &id)?;
            Ok((
                recorded,
                Journal {
                    log: Arc::new(Mutex::new(log)),
                },
            ))
        })
        .await
    }

    /// A copy of session `source` as the new session `meta`, history and dials included.
    pub async fn fork(&self, source: &Recorded, meta: SessionMeta) -> Result<Journal, AgentError> {
        let journal = self.create(meta).await?;
        let messages = source.messages.clone();
        let config = source.config.clone();
        let title = source.title.clone();
        let log = Arc::clone(&journal.log);
        run(move || {
            let mut log = log.lock().map_err(internal)?;
            for message in &messages {
                log.append(message_event(message)?).map_err(internal)?;
            }
            log.append(command(CONFIG, &config)?).map_err(internal)?;
            if let Some(title) = title {
                log.append(command(TITLE, &title)?).map_err(internal)?;
            }
            Ok(())
        })
        .await?;
        Ok(journal)
    }

    pub async fn delete(&self, id: &str) -> Result<(), AgentError> {
        let path = self.path(id)?;
        let id = id.to_owned();
        run(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(AgentError::unknown_session(&id))
            }
            Err(error) => Err(internal(error)),
        })
        .await
    }

    /// Every session, newest first, reading only the start of each journal.
    pub async fn list(&self) -> Result<Vec<Listed>, AgentError> {
        let directory = self.directory.clone();
        run(move || {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                return Ok(Vec::new());
            };
            let mut listed = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "pkl") {
                    continue;
                }
                let updated = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let mut meta = None;
                let mut title = None;
                let scanned = ConversationLog::scan(&path, |record| {
                    match record.event {
                        Event::Command { name, payload } if name == SESSION => {
                            meta = serde_json::from_str::<SessionMeta>(&payload).ok();
                        }
                        Event::Command { name, payload } if name == TITLE => {
                            title = serde_json::from_str::<String>(&payload).ok();
                        }
                        Event::Message { role, content } if role == "user" && title.is_none() => {
                            title = serde_json::from_str::<Vec<nori_llm::Part>>(&content)
                                .ok()
                                .map(|parts| Message::user(parts).text())
                                .and_then(|text| derived_title(&text));
                            return ControlFlow::Break(());
                        }
                        _ => {}
                    }
                    ControlFlow::Continue(())
                });
                if let (Ok(()), Some(meta)) = (scanned, meta) {
                    listed.push(Listed {
                        meta,
                        title,
                        updated,
                    });
                }
            }
            listed.sort_by_key(|listed| std::cmp::Reverse(listed.updated));
            Ok(listed)
        })
        .await
    }
}

/// A title from the first line of a prompt.
fn derived_title(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut title: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        title.push('…');
    }
    Some(title)
}

fn command(name: &str, payload: &impl Serialize) -> Result<Event, AgentError> {
    Ok(Event::Command {
        name: name.to_owned(),
        payload: serde_json::to_string(payload).map_err(internal)?,
    })
}

fn message_event(message: &Message) -> Result<Event, AgentError> {
    Ok(Event::Message {
        role: match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
        .to_owned(),
        content: serde_json::to_string(&message.parts).map_err(internal)?,
    })
}

fn replay(log: &ConversationLog, id: &str) -> Result<Recorded, AgentError> {
    let mut meta = None;
    let mut messages = Vec::new();
    let mut config = SessionConfig::default();
    let mut title = None;
    for record in log.visible() {
        let record = record.map_err(internal)?;
        match &record.event {
            Event::Command { name, payload } if name == SESSION => {
                meta = Some(serde_json::from_str::<SessionMeta>(payload).map_err(internal)?);
            }
            Event::Command { name, payload } if name == CONFIG => {
                config = serde_json::from_str(payload).map_err(internal)?;
            }
            Event::Command { name, payload } if name == TITLE => {
                title = serde_json::from_str(payload).ok();
            }
            Event::Message { role, content } => {
                let parts = serde_json::from_str(content).map_err(internal)?;
                let role = if role == "assistant" {
                    Role::Assistant
                } else {
                    Role::User
                };
                messages.push(Message { role, parts });
            }
            Event::Command { .. }
            | Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::Rewind { .. }
            | Event::Fork { .. }
            | Event::Operation { .. }
            | Event::SessionEvent { .. }
            | Event::Unknown { .. } => {}
        }
    }
    let meta = meta
        .ok_or_else(|| AgentError::NotResumable(format!("session {id} has no session record")))?;
    Ok(Recorded {
        meta,
        messages,
        config,
        title,
    })
}

/// One session's journal, to append to. Cheap to clone.
#[derive(Clone)]
pub struct Journal {
    log: Arc<Mutex<ConversationLog>>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Journal").finish_non_exhaustive()
    }
}

impl Journal {
    pub async fn message(&self, message: &Message) -> Result<(), AgentError> {
        let event = message_event(message)?;
        self.append(event).await
    }

    pub async fn config(&self, config: &SessionConfig) -> Result<(), AgentError> {
        let event = command(CONFIG, config)?;
        self.append(event).await
    }

    pub async fn title(&self, title: &str) -> Result<(), AgentError> {
        let event = command(TITLE, &title)?;
        self.append(event).await
    }

    async fn append(&self, event: Event) -> Result<(), AgentError> {
        let log = Arc::clone(&self.log);
        run(move || {
            log.lock()
                .map_err(internal)?
                .append(event)
                .map(drop)
                .map_err(internal)
        })
        .await
    }

    /// Make everything appended durable now (a clean shutdown).
    pub async fn sync(&self) {
        let log = Arc::clone(&self.log);
        let _ = run(move || log.lock().map_err(internal)?.sync().map_err(internal)).await;
    }
}

/// Run journal IO off the async threads.
async fn run<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
) -> Result<T, AgentError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| AgentError::Internal(format!("session journal task: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_llm::Part;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn a_session_replays_messages_dials_and_forks() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let store = Store::new(root.path());
        let meta = SessionMeta {
            id: "s1".to_owned(),
            cwd: PathBuf::from("/work"),
            created: 1,
            forked_from: None,
        };
        let journal = store
            .create(meta.clone())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let question = Message::user(vec![Part::text("Fix the build\nplease")]);
        let answer = Message::assistant(vec![Part::text("Done.")]);
        let config = SessionConfig {
            model: Some("anthropic/claude".to_owned()),
            effort: Some("high".to_owned()),
            mode: None,
            extra: BTreeMap::new(),
        };
        for result in [
            journal.message(&question).await,
            journal.config(&config).await,
            journal.message(&answer).await,
        ] {
            result.unwrap_or_else(|error| panic!("{error}"));
        }
        let (recorded, _) = store
            .open("s1")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            recorded,
            Recorded {
                meta: meta.clone(),
                messages: vec![question.clone(), answer.clone()],
                config: config.clone(),
                title: None,
            }
        );
        let fork_meta = SessionMeta {
            id: "s2".to_owned(),
            forked_from: Some("s1".to_owned()),
            ..meta
        };
        store
            .fork(&recorded, fork_meta.clone())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let (forked, _) = store
            .open("s2")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            (forked.meta, forked.messages, forked.config),
            (fork_meta, vec![question, answer], config)
        );
        let titles: Vec<Option<String>> = store
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|listed| listed.title)
            .collect();
        assert_eq!(titles, vec![Some("Fix the build".to_owned()); 2]);
        assert!(matches!(
            store.open("../etc").await,
            Err(AgentError::SessionNotFound(_))
        ));
    }
}
