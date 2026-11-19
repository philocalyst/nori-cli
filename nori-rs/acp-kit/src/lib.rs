//! The plumbing for agents that run inside Nori's process.
//!
//! An in-process agent is a library that Nori gives one end of an in-memory pipe. Every one of
//! them needs the same things: an ACP connection and its handler chain, a task per request, a
//! table of sessions, one prompt at a time per session, cancellation, option parsing, usage and
//! rate-limit reporting, tool-call translation, Nori's host tools, and errors the host can
//! categorize. This crate is the only place those live; an agent crate holds the logic that is
//! specific to its provider.
//!
//! Two layers, pick the shorter one that fits:
//!
//! - [`Provider`]: the engine has sessions that are plain values. Implement `open`, `run_turn` and
//!   whatever of `interrupt`, `configure`, `close` applies; [`ProviderAgent`] is the agent. A
//!   harness-harness is this and nothing more.
//! - [`NativeAgent`]: the agent manages its own sessions, loading, resuming, forking. The kit
//!   still serves the connection, routes and gates requests by declared [`Capabilities`], and
//!   offers the pieces ([`Sessions`], [`TurnGate`], [`Sink`], [`ToolLedger`], [`HostBridge`],
//!   [`UsageReport`], [`AgentError`]) to assemble.
//!
//! Every agent crate exposes `registration() -> Registration`, and the application collects them.
//! The crate depends on the ACP library the host speaks (re-exported as [`acp`] and [`schema`]),
//! so an agent never pins a version of its own.

mod agent;
mod auth_status;
mod error;
mod host;
mod mcp;
mod options;
mod provider;
mod registration;
mod serve;
mod sessions;
mod sink;
mod tools;
mod turn;
mod usage;

pub use agent_client_protocol as acp;
pub use agent_client_protocol::schema::v1 as schema;
pub use async_trait::async_trait;

pub use agent::Capabilities;
pub use agent::NativeAgent;
pub use agent::Profile;
pub use auth_status::AUTH_STATUS_UPDATE;
pub use auth_status::AuthAccount;
pub use auth_status::AuthStatus;
pub use auth_status::announce_auth_status;
pub use error::AgentError;
pub use error::FailureKind;
pub use host::HostBridge;
pub use host::output_payload;
pub use mcp::McpEndpoint;
pub use mcp::McpTransport;
pub use mcp::endpoints as mcp_endpoints;
pub use options::Choice;
pub use options::MODE;
pub use options::MODEL;
pub use options::Options;
pub use options::Select;
pub use options::THOUGHT_LEVEL;
pub use provider::Opened;
pub use provider::Provider;
pub use provider::ProviderAgent;
pub use provider::TurnCtx;
pub use registration::Host;
pub use registration::InProcessSpec;
pub use registration::Launcher;
pub use registration::Prewarmer;
pub use registration::Registration;
pub use registration::Resolver;
pub use registration::SpecOptions;
pub use serve::serve;
pub use serve::serve_over_pipe;
pub use serve::serve_over_pipe_as;
pub use sessions::Sessions;
pub use sink::ContextUsage;
pub use sink::Cx;
pub use sink::PermissionDecision;
pub use sink::Sink;
pub use sink::usage_update;
pub use tools::Outcome;
pub use tools::ToolLedger;
pub use tools::mcp_content;
pub use tools::value_content;
pub use turn::Turn;
pub use turn::TurnGate;
pub use turn::TurnKind;
pub use turn::TurnPolicy;
pub use usage::RateLimitWindow;
pub use usage::TokenBreakdown;
pub use usage::TokensScope;
pub use usage::UsageReport;
pub use usage::rfc3339_utc;
