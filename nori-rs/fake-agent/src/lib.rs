//! A scriptable, deterministic fake ACP agent, and the worlds that drive it.
//!
//! A **world** is a declarative script, in Pkl or JSON: one or more fake agents (slug, models,
//! modes, capabilities) and, for each, **scenes** that say what the agent does for the prompts
//! they match (by index, by regular expression, or any). A scene is a timeline of **steps**:
//! text and thought with chunking and cadence, tool calls with diffs and output, permission
//! requests that branch on the answer, plans, usage with rate limits, delegation through Nori's
//! host tools, structured errors, and the faults real agents commit (a dropped, repeated or
//! stale response, work after the turn ended, permission requests after a cancel, a hang, a
//! crash). See `docs/fake-worlds.md`.
//!
//! The agents are built on `nori-acp-kit`, so they are ordinary in-process agents: register
//! [`registration`] with the host and select them with `native = { agent = "fake" }`, or serve
//! one over stdio with `nori agent serve fake world=<file>`. Time is virtual and deterministic
//! when Nori runs them in its own process, and real when they are served to a client
//! ([`clock`]).
//!
//! Layout:
//!
//! - [`world`]: the serde model, the Pkl schema, loading and validation.
//! - [`exec`] (private): the interpreter. [`FakeAgent`] is the `NativeAgent` around it.
//! - [`wire`]: the tap for faults below the ACP library.
//! - [`runtime`]: what the agents of a world share; [`record`]: the flight recorder.

mod agent;
pub mod clock;
mod exec;
pub mod record;
mod registration;
pub mod runtime;
mod session;
pub mod wire;
pub mod world;

pub use agent::FakeAgent;
pub use clock::Clock;
pub use clock::ClockMode;
pub use record::Entry;
pub use record::FlightRecorder;
pub use record::RECORD_FILE_ENV;
pub use record::Record;
pub use record::WireFate;
pub use registration::AgentEntry;
pub use registration::InstalledWorld;
pub use registration::NAME;
pub use registration::OPTIONS;
pub use registration::registration;
pub use runtime::WorldRuntime;
pub use world::Answer;
pub use world::LineDef;
pub use world::ProcessDef;
pub use world::StreamDef;
pub use world::World;
pub use world::WorldError;
