//! The op graph: a validated program compiled for execution.
//!
//! [`CompiledProgram`] is what the runner executes and what travels in the `run.started`
//! envelope, so another world sees exactly the graph that runs. Every [`Node`] carries its
//! path (`steps[2].branches[0]`), the stable name used in the journal, errors and snapshots.

use std::collections::BTreeMap;

use nori_pkl::recovery::RecoveryBlock;
use serde::Deserialize;
use serde::Serialize;

use crate::host::Delivery;
use crate::program::model::Check;
use crate::program::model::GuardPhase;
use crate::program::model::OnExhausted;
use crate::program::model::World;
use crate::schemas::Actor;
use crate::schemas::SkillSpec;
use crate::schemas::ToolSpec;

/// Who a `send` addresses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Target {
    /// An actor the program declares; started on first use.
    Declared(String),
    /// An actor the program did not start, addressed as `actor://alias`.
    External(String),
}

/// Which implementation a `call` reaches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolRef {
    Host { name: String },
    Declared { tool: Box<ToolSpec> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub path: String,
    pub id: Option<String>,
    /// The binding that names this step's value.
    pub bind: Option<String>,
    pub recovery: Option<RecoveryBlock>,
    pub timeout_ms: Option<i64>,
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Prompt {
        text: String,
    },
    Config {
        key: String,
        value: String,
    },
    Spawn {
        actor: String,
    },
    Send {
        to: Target,
        message: String,
        wait: bool,
        delivery: Delivery,
        /// Names the receipt of a send that does not wait.
        receipt: Option<String>,
    },
    Await {
        receipt: String,
    },
    Call {
        tool: ToolRef,
        args: serde_json::Value,
    },
    Exec {
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
    },
    SelectContext {
        start: i64,
        end: i64,
    },
    Fork {
        source: String,
        start: i64,
        end: i64,
    },
    Rewind {
        head: Option<i64>,
    },
    Seq {
        body: Vec<Node>,
    },
    Par {
        branches: Vec<Node>,
    },
    Race {
        branches: Vec<Node>,
    },
    FanOut {
        over: Vec<String>,
        each: String,
        body: Vec<Node>,
    },
    Loop {
        max_iterations: i64,
        until: Option<Check>,
        body: Vec<Node>,
    },
    When {
        check: Check,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
    Guard {
        phase: GuardPhase,
        check: Check,
        feedback: String,
        max_attempts: i64,
        on_exhausted: OnExhausted,
        body: Vec<Node>,
    },
    Budget {
        tokens: i64,
        body: Vec<Node>,
    },
    Within {
        world: String,
        body: Vec<Node>,
    },
    Retry {
        body: Vec<Node>,
    },
    Recover {
        body: Vec<Node>,
        fallback: Vec<Node>,
    },
    Emit {
        topic: String,
        payload: String,
    },
    AwaitSignal {
        topic: String,
    },
    Interrupt {
        prompt: String,
    },
    Delay {
        ms: i64,
    },
    Pin {
        fact: String,
    },
    Checkpoint {
        name: String,
    },
}

impl Node {
    /// The nodes directly inside this one, in execution order.
    pub fn children(&self) -> Vec<&Node> {
        match &self.op {
            Op::Seq { body }
            | Op::FanOut { body, .. }
            | Op::Loop { body, .. }
            | Op::Guard { body, .. }
            | Op::Budget { body, .. }
            | Op::Within { body, .. }
            | Op::Retry { body } => body.iter().collect(),
            Op::Par { branches } | Op::Race { branches } => branches.iter().collect(),
            Op::When {
                then, otherwise, ..
            } => then.iter().chain(otherwise).collect(),
            Op::Recover { body, fallback } => body.iter().chain(fallback).collect(),
            Op::Prompt { .. }
            | Op::Config { .. }
            | Op::Spawn { .. }
            | Op::Send { .. }
            | Op::Await { .. }
            | Op::Call { .. }
            | Op::Exec { .. }
            | Op::SelectContext { .. }
            | Op::Fork { .. }
            | Op::Rewind { .. }
            | Op::Emit { .. }
            | Op::AwaitSignal { .. }
            | Op::Interrupt { .. }
            | Op::Delay { .. }
            | Op::Pin { .. }
            | Op::Checkpoint { .. } => Vec::new(),
        }
    }
}

impl Op {
    /// The step kind as the program spelled it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Config { .. } => "config",
            Self::Spawn { .. } => "spawn",
            Self::Send { .. } => "send",
            Self::Await { .. } => "await",
            Self::Call { .. } => "call",
            Self::Exec { .. } => "unsafe",
            Self::SelectContext { .. } => "select_context",
            Self::Fork { .. } => "fork",
            Self::Rewind { .. } => "rewind",
            Self::Seq { .. } => "sequence",
            Self::Par { .. } => "par",
            Self::Race { .. } => "race",
            Self::FanOut { .. } => "fan_out",
            Self::Loop { .. } => "loop",
            Self::When { .. } => "when",
            Self::Guard { .. } => "guard",
            Self::Budget { .. } => "budget",
            Self::Within { .. } => "within",
            Self::Retry { .. } => "retry",
            Self::Recover { .. } => "recover",
            Self::Emit { .. } => "emit",
            Self::AwaitSignal { .. } => "await_signal",
            Self::Interrupt { .. } => "interrupt",
            Self::Delay { .. } => "delay",
            Self::Pin { .. } => "pin",
            Self::Checkpoint { .. } => "checkpoint",
        }
    }
}

/// A program ready to run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompiledProgram {
    pub name: String,
    pub description: String,
    pub actors: BTreeMap<String, Actor>,
    pub worlds: BTreeMap<String, World>,
    /// The world the whole program runs in, when it names one.
    pub world: Option<World>,
    pub skills: Vec<SkillSpec>,
    pub tools: Vec<ToolSpec>,
    pub recovery: Option<RecoveryBlock>,
    pub budget_tokens: Option<i64>,
    pub root: Node,
    pub result: Option<String>,
}
