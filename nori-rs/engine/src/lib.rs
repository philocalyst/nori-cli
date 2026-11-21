//! Nori's own agent engine: the agent loop, tools, permissions and sessions behind the
//! in-process Codex and OpenCode agents, talking to provider APIs directly (`nori-llm`).
//!
//! An agent is a [`Flavor`] (its models, credentials, configuration, prompt and tools) served by
//! the engine. See `docs/native-agents.md`.

mod act;
mod agent;
mod flavor;
mod mcp;
mod options;
mod prompt;
mod replay;
mod rules;
mod safe;
mod session;
mod store;
pub mod tools;
mod turn;

pub use agent::EngineAgent;
pub use flavor::CommandDef;
pub use flavor::Connection;
pub use flavor::Dial;
pub use flavor::DialChoice;
pub use flavor::EffortStyle;
pub use flavor::Flavor;
pub use flavor::Identity;
pub use flavor::Instruction;
pub use flavor::Mode;
pub use flavor::ModelInfo;
pub use flavor::Workspace;
pub use rules::Effect;
pub use rules::Rule;
pub use rules::denied_everywhere;
pub use rules::evaluate;
pub use rules::primary_action;
pub use safe::READ_ONLY_SHELL;
pub use safe::reads_only;
pub use store::Listed;
pub use store::Recorded;
pub use store::SessionConfig;
pub use store::SessionMeta;
pub use store::Store;
