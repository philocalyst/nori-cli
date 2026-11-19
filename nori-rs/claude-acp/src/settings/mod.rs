//! Settings management
//!
//! Loads and merges settings from multiple sources:
//! - User settings: `~/.claude/settings.json`
//! - Project settings: `.claude/settings.json`
//! - Local settings: `.claude/settings.local.json`
//!
//! Priority: Local > Project > User

mod manager;
mod permission_checker;
mod rule;

pub use manager::Settings;
pub use manager::SettingsManager;
pub use permission_checker::PermissionChecker;
pub use rule::PermissionCheckResult;
pub use rule::PermissionDecision;

#[cfg(test)]
pub use rule::PermissionSettings;
