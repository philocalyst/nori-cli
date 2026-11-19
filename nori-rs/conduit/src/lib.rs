//! Declarative multi-actor programs: the rules substrate behind Nori's conversation tool and
//! the seam for out-of-process "second worlds".
//!
//! - [`program`]: load a `nori:Program` Pkl module, check it and compile it to an op graph.
//! - [`run`]: execute the graph through one [`ProgramHost`], journaling every step; a
//!   [`RunHandle`] is the live capability over a run (snapshot, subscribe, signal, cancel).
//! - [`wire`] and [`transport`]: the versioned envelope every fact is journaled as, and the
//!   transports (file journal, in-process channel, fan-out) that carry it to another world.
//! - [`snapshot`]: the re-loadable Pkl snapshot of a run.
//! - [`tools`]: the sandboxed tool runtime seam for custom WebAssembly tools.

pub mod error;
pub mod host;
pub mod op;
pub mod policy;
pub mod program;
pub mod run;
pub mod schemas;
pub mod snapshot;
pub mod tools;
pub mod transport;
pub mod value;
pub mod wire;

pub use async_trait::async_trait;
pub use error::ConduitError;
pub use error::Problem;
pub use host::ActorRef;
pub use host::ActorSpec;
pub use host::Delivery;
pub use host::InterruptRegistry;
pub use host::LocalHost;
pub use host::ProgramHost;
pub use host::Receipt;
pub use host::RecoveryNote;
pub use host::Reply;
pub use host::SignalHub;
pub use host::ToolOutcome;
pub use host::WorldScope;
pub use op::CompiledProgram;
pub use policy::BudgetFrame;
pub use policy::enforce_bash_sandbox;
pub use policy::enforce_sandbox_path;
pub use policy::enforce_sandbox_tool;
pub use policy::narrow_sandbox;
pub use program::is_program;
pub use program::load_file;
pub use program::load_source;
pub use run::RunConfig;
pub use run::RunHandle;
pub use run::RunOutcome;
pub use run::run_program;
pub use schemas::Actor;
pub use schemas::Grant;
pub use schemas::Isolation;
pub use schemas::SandboxPolicy;
pub use schemas::SkillSpec;
pub use schemas::ToolParam;
pub use schemas::ToolSource;
pub use schemas::ToolSpec;
pub use schemas::builtin_virtual_modules;
pub use schemas::conduit_evaluator_options;
pub use schemas::evaluate_schema_module;
pub use schemas::program_evaluator_options;
pub use snapshot::PendingInterrupt;
pub use snapshot::RunSnapshot;
pub use snapshot::RunStatus;
pub use tools::ToolCall;
pub use tools::ToolRuntime;
pub use tools::UnavailableToolRuntime;
pub use transport::ChannelTransport;
pub use transport::FanoutTransport;
pub use transport::FileTransport;
pub use transport::RecordingTransport;
pub use transport::Transport;
pub use value::ConduitValue;
pub use wire::Envelope;
pub use wire::WIRE_VERSION;
pub use wire::WireOp;

#[cfg(test)]
mod tests;
