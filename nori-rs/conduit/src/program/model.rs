//! The authoring model: what a `nori:Program` module evaluates to.
//!
//! These types mirror `schemas/Program.pkl` one for one. They are plain data; checking and
//! compiling them is [`super::compile`].

use std::collections::BTreeMap;

use nori_pkl::recovery::DurationValue;
use nori_pkl::recovery::RecoveryBlock;
use serde::Deserialize;
use serde::Serialize;

use crate::host::Delivery;
use crate::schemas::Actor;
use crate::schemas::Isolation;
use crate::schemas::ToolSpec;
use crate::schemas::ToolsetSpec;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub actors: BTreeMap<String, Actor>,
    #[serde(default)]
    pub worlds: BTreeMap<String, World>,
    #[serde(default)]
    pub world: Option<World>,
    #[serde(default)]
    pub toolsets: Vec<ToolsetSpec>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub recovery: Option<RecoveryBlock>,
    #[serde(default)]
    pub budget_tokens: Option<i64>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct World {
    #[serde(default)]
    pub isolation: Isolation,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub budget_tokens: Option<i64>,
    #[serde(default)]
    pub home: bool,
}

/// A condition over a binding. Exactly one predicate is set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub value: String,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub starts_with: Option<String>,
    #[serde(default)]
    pub negate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardPhase {
    Pre,
    Post,
}

impl GuardPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Post => "post",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExhausted {
    Fail,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepMeta {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub recovery: Option<RecoveryBlock>,
    #[serde(default)]
    pub timeout: Option<DurationValue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    #[serde(flatten)]
    pub meta: StepMeta,
    #[serde(flatten)]
    pub body: StepKind,
}

fn yes() -> bool {
    true
}

fn item() -> String {
    "item".to_owned()
}

fn three() -> i64 {
    3
}

fn post() -> GuardPhase {
    GuardPhase::Post
}

fn fail() -> OnExhausted {
    OnExhausted::Fail
}

fn queue() -> Delivery {
    Delivery::Queue
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
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
        to: String,
        message: String,
        #[serde(default = "yes")]
        await_reply: bool,
        #[serde(default = "queue")]
        delivery: Delivery,
    },
    Await {
        receipt: String,
    },
    Call {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    Unsafe {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    SelectContext {
        start: i64,
        end: i64,
    },
    Fork {
        actor: String,
        start: i64,
        end: i64,
    },
    Rewind {
        #[serde(default)]
        head: Option<i64>,
    },
    Sequence {
        #[serde(default)]
        steps: Vec<Step>,
    },
    Par {
        #[serde(default)]
        branches: Vec<Step>,
    },
    Race {
        #[serde(default)]
        branches: Vec<Step>,
    },
    FanOut {
        over: Vec<String>,
        #[serde(default = "item")]
        each: String,
        #[serde(default)]
        steps: Vec<Step>,
    },
    Loop {
        max_iterations: i64,
        #[serde(default)]
        until: Option<Check>,
        #[serde(default)]
        steps: Vec<Step>,
    },
    When {
        check: Check,
        #[serde(default)]
        steps: Vec<Step>,
        #[serde(default)]
        otherwise: Vec<Step>,
    },
    Guard {
        #[serde(default = "post")]
        phase: GuardPhase,
        check: Check,
        feedback: String,
        #[serde(default = "three")]
        max_attempts: i64,
        #[serde(default = "fail")]
        on_exhausted: OnExhausted,
        #[serde(default)]
        steps: Vec<Step>,
    },
    Budget {
        tokens: i64,
        #[serde(default)]
        steps: Vec<Step>,
    },
    Within {
        world: String,
        #[serde(default)]
        steps: Vec<Step>,
    },
    Retry {
        #[serde(default)]
        steps: Vec<Step>,
    },
    Recover {
        #[serde(default)]
        steps: Vec<Step>,
        #[serde(default)]
        fallback: Vec<Step>,
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
        duration: DurationValue,
    },
    Pin {
        fact: String,
    },
    Checkpoint {
        name: String,
    },
}
