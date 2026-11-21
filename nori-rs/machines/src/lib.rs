//! The user's other computers, and running commands on them.
//!
//! An agent names a machine (`"machine": "h16003mac"`) and Nori runs the command there itself,
//! so no agent wraps commands in `ssh` or `rex`. The pieces:
//!
//! - [`Roster`]: which machines exist. Rex's hosts (`rex hosts`) and `[machines.NAME]` in Nori's
//!   `config.toml`, installed once per process ([`discover_in_background`], [`global`]); empty in
//!   tests and for users with no other machines, so nothing about machines shows then.
//! - [`Facts`]: what a probe learned (OS, architecture, home, login `PATH`), kept in
//!   `machines.json` so the next run can describe machines before connecting.
//! - running: [`Roster::exec`] sends one self-contained `sh` script over `ssh -T` (see
//!   `script`), streams [`Line`]s back, and returns a [`Run`] or an [`ExecError`] that says
//!   whether anything ran. Why ssh rather than Rex is in `exec`.
//!
//! Credentials never pass through Nori: ssh authenticates with the user's own agent and keys, in
//! `BatchMode`, so a machine that would prompt is reported unreachable instead.

mod exec;
mod facts;
mod fs;
pub mod jobs;
mod name;
mod rex;
mod roster;
mod script;

pub use exec::ExecError;
pub use exec::Line;
pub use exec::Run;
pub use exec::Stream;
pub use facts::Facts;
pub use fs::MachineFs;
pub use fs::PathKind;
pub use fs::PathStat;
pub use name::LOCAL;
pub use name::MachineName;
pub use name::NameError;
pub use name::SshDestination;
pub use roster::Machine;
pub use roster::NoriThere;
pub use roster::Origin;
pub use roster::Reach;
pub use roster::Roster;
pub use roster::Target;
pub use roster::UnknownMachine;
pub use roster::discover_in_background;
pub use roster::global;
pub use roster::global_ready;
pub use roster::install;
pub use script::RemoteDir;
pub use script::StartDir;

#[cfg(test)]
mod tests;
