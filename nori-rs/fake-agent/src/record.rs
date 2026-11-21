//! The flight recorder: what the fake agents saw and did, as a world-wide ordered log.
//!
//! The harness's event stream says what a client observed. The recorder says what the agents
//! actually received and sent, which is what an assertion about delivery needs ("the message was
//! read exactly once", "the permission was answered after the turn ended"). A driver reads it;
//! a served process can tee it to a file.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::world::Answer;

/// How a response on the wire was tampered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFate {
    Delivered,
    Dropped,
    Duplicated,
    /// An extra response for an id that was not pending.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    /// The agent process or in-process agent started serving.
    Serving {
        agent: String,
    },
    SessionOpened {
        agent: String,
        session: String,
        model: Option<String>,
    },
    /// A prompt arrived and was matched to a scene (`None`: unscripted).
    PromptReceived {
        agent: String,
        session: String,
        index: i64,
        text: String,
        scene: Option<String>,
        /// What the prompt weighed on the wire: all its text blocks, and the longest of them.
        /// Nori's own context counts here, though `text` leaves it out.
        #[serde(default)]
        bytes: i64,
        #[serde(default)]
        largest_block_bytes: i64,
    },
    /// The agent's provider refused a prompt for its size (see `Limits`), before any scene ran.
    PromptRefused {
        agent: String,
        session: String,
        bytes: i64,
        largest_block_bytes: i64,
        /// The provider's body, as the client received it.
        detail: String,
    },
    CancelReceived {
        agent: String,
        session: String,
    },
    /// A permission request went out.
    PermissionAsked {
        agent: String,
        session: String,
        tool: String,
        /// The request was sent when no turn of this session was running.
        outside_turn: bool,
    },
    PermissionAnswered {
        agent: String,
        session: String,
        tool: String,
        answer: Answer,
    },
    HostTool {
        agent: String,
        session: String,
        name: String,
        arguments: Value,
        output: String,
        is_error: bool,
    },
    /// What happened to a response the agent produced.
    Response {
        agent: String,
        request_id: Value,
        method: String,
        fate: WireFate,
    },
    /// The agent ended a turn.
    TurnEnded {
        agent: String,
        session: String,
        outcome: String,
    },
    ConfigChanged {
        agent: String,
        session: String,
        option: String,
        value: String,
    },
    /// A `session/set_config_option` went wrong on purpose (see `ConfigFault`): `effect` is
    /// `reject`, `ignore` or `substitute`; `actual` is the value the option has afterwards.
    ConfigFaulted {
        agent: String,
        session: String,
        option: String,
        requested: String,
        effect: String,
        actual: Option<String>,
    },
    /// The script and the world disagreed: an answer other than the one expected, an unbound
    /// variable, a prompt no scene covers, a host tool that could not run.
    Divergence {
        agent: String,
        session: String,
        message: String,
    },
    Crashed {
        agent: String,
        exit_code: i64,
    },
    /// What the person (or the driver standing in for one) did, in the same order as everything
    /// the agents did: `prompt`, `cancel`, `steer`, `approve`, `deny`, `rewind`, `resume`,
    /// `shutdown`.
    User {
        action: String,
        detail: String,
    },
}

/// One entry: when (in the world's time) and what.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: i64,
    pub at: Duration,
    #[serde(flatten)]
    pub record: Record,
}

struct Shared {
    entries: Mutex<Vec<Entry>>,
    live: broadcast::Sender<Entry>,
    /// The recorder is a file that a served process also appends to; its order is the file's.
    file: Option<std::path::PathBuf>,
}

/// An append-only log all of a world's agents write to. Cheap to clone.
#[derive(Clone)]
pub struct FlightRecorder {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for FlightRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlightRecorder")
            .field("entries", &self.snapshot().len())
            .finish()
    }
}

impl Default for FlightRecorder {
    fn default() -> Self {
        let (live, _) = broadcast::channel(1024);
        Self {
            shared: Arc::new(Shared {
                entries: Mutex::new(Vec::new()),
                live,
                file: None,
            }),
        }
    }
}

impl FlightRecorder {
    /// A recorder that is the file `path`: what this side pushes and what a served process
    /// appends to it (see [`RECORD_FILE_ENV`]) are one log, in the order they were written.
    pub fn file_backed(path: std::path::PathBuf) -> Self {
        let (live, _) = broadcast::channel(1024);
        Self {
            shared: Arc::new(Shared {
                entries: Mutex::new(Vec::new()),
                live,
                file: Some(path),
            }),
        }
    }

    pub fn push(&self, at: Duration, record: Record) {
        let mut entries = self
            .shared
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entry = Entry {
            seq: i64::try_from(entries.len()).unwrap_or(i64::MAX),
            at,
            record,
        };
        if tracing::enabled!(target: "nori_fake_agent::record", tracing::Level::DEBUG)
            && let Ok(line) = serde_json::to_string(&entry)
        {
            tracing::debug!(target: "nori_fake_agent::record", "{line}");
        }
        if let Some(path) = &self.shared.file {
            use std::io::Write;
            if let Ok(line) = serde_json::to_string(&entry)
                && let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            {
                let _ = writeln!(file, "{line}");
            }
            return;
        }
        tee(&entry);
        entries.push(entry.clone());
        let _ = self.shared.live.send(entry);
    }

    pub fn snapshot(&self) -> Vec<Entry> {
        if let Some(path) = &self.shared.file {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            return text
                .lines()
                .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
                .enumerate()
                .map(|(index, mut entry)| {
                    entry.seq = i64::try_from(index).unwrap_or(i64::MAX);
                    entry
                })
                .collect();
        }
        self.shared
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Entries as they are written, from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Entry> {
        self.shared.live.subscribe()
    }

    /// Every divergence recorded so far.
    pub fn divergences(&self) -> Vec<String> {
        self.snapshot()
            .into_iter()
            .filter_map(|entry| match entry.record {
                Record::Divergence {
                    agent,
                    session,
                    message,
                } => Some(format!("{agent}/{session}: {message}")),
                Record::Serving { .. }
                | Record::SessionOpened { .. }
                | Record::PromptReceived { .. }
                | Record::PromptRefused { .. }
                | Record::CancelReceived { .. }
                | Record::PermissionAsked { .. }
                | Record::PermissionAnswered { .. }
                | Record::HostTool { .. }
                | Record::Response { .. }
                | Record::TurnEnded { .. }
                | Record::ConfigChanged { .. }
                | Record::ConfigFaulted { .. }
                | Record::Crashed { .. }
                | Record::User { .. } => None,
            })
            .collect()
    }
}

/// The environment variable that names a file a served process appends its recorder to, one JSON
/// entry per line, so a driver in another process reads what this agent did. The driver
/// appends what the user did to the same file, which gives both one order.
pub const RECORD_FILE_ENV: &str = "NORI_WORLD_RECORD";

fn tee(entry: &Entry) {
    use std::io::Write;
    static FILE: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os(RECORD_FILE_ENV)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    });
    if let Some(file) = file
        && let Ok(line) = serde_json::to_string(entry)
    {
        let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(file, "{line}");
    }
}
