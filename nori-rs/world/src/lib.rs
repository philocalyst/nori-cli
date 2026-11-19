//! Integration tests and live demos against a world of fake agents.
//!
//! [`WorldSession::boot`] runs the real harness (the real `AcpBackend`, orchestrator, approvals
//! and transcripts, in a temporary `NORI_HOME`) against the fake agents of a [`World`]
//! (`nori-fake-agent`), with no network and no real provider. The test acts as the user and
//! reads the result as [`Events`]:
//!
//! ```ignore
//! let world = World::from_path("fake-agent/worlds/nested-subagents.pkl".as_ref())?;
//! let session = WorldSession::boot(world, WorldOptions::default()).await?;
//! let events = session.ask("review the parser").await?;
//! assert!(events.subagent("reviewer").is_some());
//! session.finish().await?.is_clean()?;
//! ```
//!
//! Why a crate and not a feature of `nori-harness`: the harness stays free of test support, the
//! fake agent stays free of the harness, and a test in any crate (the harness's own included)
//! can depend on this one.

mod approvals;
mod boot;
mod events;
mod invariants;
mod session;

pub use approvals::Approvals;
pub use approvals::Decision;
pub use approvals::PendingPermission;
pub use approvals::Rule;
pub use boot::Configure;
pub use boot::Transport;
pub use boot::WorldOptions;
pub use events::Events;
pub use events::Lane;
pub use events::PermissionView;
pub use events::Recorded;
pub use events::SubagentInfo;
pub use events::ToolView;
pub use invariants::Violation;
pub use nori_fake_agent::Entry;
pub use nori_fake_agent::Record;
pub use nori_fake_agent::World;
pub use session::Report;
pub use session::Stuck;
pub use session::WorldSession;
