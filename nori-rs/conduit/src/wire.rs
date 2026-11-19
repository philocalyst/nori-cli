//! The versioned wire envelope: every observable fact about a program run as one message.
//!
//! The run journal (`journal.jsonl`) is a sequence of envelopes; a [`Transport`](crate::transport::Transport)
//! carries the same envelopes to another world (a daemon, a supervisor on the BEAM) and brings
//! that world's envelopes back (signals, interrupt answers, cancellation).
//!
//! ```json
//! {"v":1,"run":"run-7f3a","actor":"reviewer","turn":2,"seq":14,"op":"message.replied","payload":{"text":"APPROVED"}}
//! ```
//!
//! - `v`: envelope version. Readers reject versions they do not know.
//! - `run`: the run id; unique per program execution.
//! - `actor`: the alias the fact concerns, or `"program"` for run-level facts.
//! - `turn`: the actor's turn counter (0 for run-level facts, 1.. for each message the run
//!   delivers to that actor). Together with `actor` it names one unit of work.
//! - `seq`: run-global, strictly increasing from 1; the total order of the run.
//! - `op`: what happened, see [`WireOp`].
//! - `payload`: op-specific JSON object.
//!
//! # Mapping to Erlang terms
//!
//! [`Envelope::to_erlang_term`] renders the envelope as the external term a BEAM node
//! decodes from JSON with the same rules:
//!
//! | JSON | Erlang |
//! | --- | --- |
//! | object | map with binary keys (`#{<<"k">> => V}`) |
//! | array | list |
//! | string | binary (`<<"text">>`, UTF-8) |
//! | integer / float | integer / float |
//! | `true` / `false` | atoms `true` / `false` |
//! | `null` | atom `nil` |
//!
//! and the envelope itself is the tuple `{conduit, V, Run, Actor, Turn, Seq, Op, Payload}`
//! with `Run` and `Actor` binaries, `Turn`, `Seq` and `V` integers, `Op` an atom named by the
//! op string (`'step.started'`) and `Payload` the map. A reply from the other world is the
//! same tuple sent back as a message, so a `gen_server` can pattern match on the op atom.

use serde::Deserialize;
use serde::Serialize;

use crate::error::ConduitError;

/// The envelope version this build writes and accepts.
pub const WIRE_VERSION: i64 = 1;

/// The actor name of run-level facts.
pub const PROGRAM_ACTOR: &str = "program";

/// What an envelope reports or requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WireOp {
    /// Outbound: the run began. Payload: `program`, `source`, `ops` (the compiled op graph).
    #[serde(rename = "run.started")]
    RunStarted,
    /// Outbound: the run ended well. Payload: `value`, `tokens`.
    #[serde(rename = "run.finished")]
    RunFinished,
    /// Outbound: the run ended badly. Payload: `error`.
    #[serde(rename = "run.failed")]
    RunFailed,
    /// Inbound: stop the run. Payload: `reason`.
    #[serde(rename = "run.cancel")]
    RunCancel,
    /// Outbound: a step began. Payload: `path`, `kind`, `id`.
    #[serde(rename = "step.started")]
    StepStarted,
    /// Outbound: a step ended well. Payload: `path`, `value`.
    #[serde(rename = "step.finished")]
    StepFinished,
    /// Outbound: a step failed. Payload: `path`, `error`, `class`.
    #[serde(rename = "step.failed")]
    StepFailed,
    /// Outbound: an actor started. Payload: `session`, `agent`, `model`.
    #[serde(rename = "actor.spawned")]
    ActorSpawned,
    /// Outbound: a message was handed to an actor. Payload: `receipt`, `text`, `delivery`.
    #[serde(rename = "message.sent")]
    MessageSent,
    /// Outbound: an actor's turn ended with a reply. Payload: `receipt`, `text`.
    #[serde(rename = "message.replied")]
    MessageReplied,
    /// Outbound: recovery retries the same route. Payload: `attempt`, `delay_ms`, `class`, `error`.
    #[serde(rename = "recovery.retry")]
    RecoveryRetry,
    /// Outbound: an actor moved to another route. Payload: `from`, `to`, `class`, `step`.
    #[serde(rename = "route.changed")]
    RouteChanged,
    /// Outbound: a tool ran. Payload: `tool`, `kind`, `args`, `output`, `tokens`.
    #[serde(rename = "tool.called")]
    ToolCalled,
    /// Both ways: a signal. Payload: `topic`, `payload`. Inbound ones wake `await_signal`.
    #[serde(rename = "signal")]
    Signal,
    /// Outbound: the run suspends for a person. Payload: `interrupt`, `prompt`.
    #[serde(rename = "interrupt.requested")]
    InterruptRequested,
    /// Inbound: the answer. Payload: `interrupt`, `response`.
    #[serde(rename = "interrupt.resumed")]
    InterruptResumed,
    /// Outbound: a world was opened. Payload: `world`, `isolation`, `root`, `home`.
    #[serde(rename = "world.opened")]
    WorldOpened,
    /// Outbound: a world was closed. Payload: `world`.
    #[serde(rename = "world.closed")]
    WorldClosed,
    /// Outbound: a program-level fact worth keeping (pin, checkpoint, guard verdict).
    #[serde(rename = "note")]
    Note,
}

impl WireOp {
    pub fn name(self) -> &'static str {
        match self {
            Self::RunStarted => "run.started",
            Self::RunFinished => "run.finished",
            Self::RunFailed => "run.failed",
            Self::RunCancel => "run.cancel",
            Self::StepStarted => "step.started",
            Self::StepFinished => "step.finished",
            Self::StepFailed => "step.failed",
            Self::ActorSpawned => "actor.spawned",
            Self::MessageSent => "message.sent",
            Self::MessageReplied => "message.replied",
            Self::RecoveryRetry => "recovery.retry",
            Self::RouteChanged => "route.changed",
            Self::ToolCalled => "tool.called",
            Self::Signal => "signal",
            Self::InterruptRequested => "interrupt.requested",
            Self::InterruptResumed => "interrupt.resumed",
            Self::WorldOpened => "world.opened",
            Self::WorldClosed => "world.closed",
            Self::Note => "note",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: i64,
    pub run: String,
    pub actor: String,
    pub turn: i64,
    pub seq: i64,
    pub op: WireOp,
    pub payload: serde_json::Value,
}

impl Envelope {
    pub fn new(
        run: &str,
        actor: &str,
        turn: i64,
        seq: i64,
        op: WireOp,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            v: WIRE_VERSION,
            run: run.to_owned(),
            actor: actor.to_owned(),
            turn,
            seq,
            op,
            payload,
        }
    }

    /// One JSON line, without the trailing newline.
    pub fn to_line(&self) -> Result<String, ConduitError> {
        serde_json::to_string(self).map_err(|error| ConduitError::Wire {
            message: format!("cannot encode envelope: {error}"),
        })
    }

    /// Parse one line, rejecting envelope versions this build does not know.
    pub fn from_line(line: &str) -> Result<Self, ConduitError> {
        let envelope: Self = serde_json::from_str(line).map_err(|error| ConduitError::Wire {
            message: format!("invalid envelope: {error}"),
        })?;
        if envelope.v != WIRE_VERSION {
            return Err(ConduitError::Wire {
                message: format!(
                    "envelope version {} is not supported (this build speaks {WIRE_VERSION})",
                    envelope.v
                ),
            });
        }
        Ok(envelope)
    }

    /// The envelope as Erlang external term text; see the module docs for the mapping.
    pub fn to_erlang_term(&self) -> String {
        format!(
            "{{conduit,{},{},{},{},{},{},{}}}",
            self.v,
            erlang_binary(&self.run),
            erlang_binary(&self.actor),
            self.turn,
            self.seq,
            erlang_atom(self.op.name()),
            json_to_erlang(&self.payload),
        )
    }
}

fn erlang_atom(name: &str) -> String {
    format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn erlang_binary(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("<<\"{escaped}\"/utf8>>")
}

fn json_to_erlang(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "nil".to_owned(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => erlang_binary(text),
        serde_json::Value::Array(items) => {
            let items: Vec<String> = items.iter().map(json_to_erlang).collect();
            format!("[{}]", items.join(","))
        }
        serde_json::Value::Object(map) => {
            let pairs: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{}=>{}", erlang_binary(key), json_to_erlang(value)))
                .collect();
            format!("#{{{}}}", pairs.join(","))
        }
    }
}
