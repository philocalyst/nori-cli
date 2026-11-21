//! The module catalog: finds, evaluates, validates and watches Nori's declarative modules.
//!
//! Two roots are read in this order: `$NORI_HOME` (the person's own) and `<repo>/.nori` (the
//! project's, passed only when the directory is trusted). In each, the kind's directory
//! (`jobs`, `monitors`, `skills`, `plugins`, `gates`) holds `*.pkl` modules that `amends "nori:<Kind>"`.
//! A project module shadows a personal one of the same `name`, and a personal one shadows a
//! built-in one: Nori ships modules in `$NORI_HOME/builtin/<kind>/` (a third, lowest root that
//! exists whenever `user` does) and the harness regenerates them on start.
//!
//! [`load`] is blocking: it evaluates Pkl and reads the disk. From async code use [`watch`], which
//! runs it on the blocking pool, or call it inside `spawn_blocking`. A [`CatalogCache`] keeps
//! each file's last evaluation keyed by (path, mtime, length), so a reload evaluates only what
//! changed; a module that was good and broke keeps its last good value and reports a [`Problem`].
//! Only the module file itself is tracked, not the files it imports.

mod cache;
mod watch;

#[cfg(test)]
mod gate_tests;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

pub use cache::CatalogCache;
pub use watch::watch;

use crate::EvaluatorOptions;
use crate::kinds::GateDef;
use crate::kinds::JobDef;
use crate::kinds::MonitorDef;
use crate::kinds::PluginDef;
use crate::kinds::SkillMeta;

/// A kind of module the catalog reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// `nori:Job`, in `jobs/`.
    Job,
    /// `nori:Monitor`, in `monitors/`.
    Monitor,
    /// `nori:Skill`, in `skills/`.
    Skill,
    /// `nori:Plugin`, in `plugins/`.
    Plugin,
    /// `nori:Gate`, in `gates/`.
    Gate,
}

impl Kind {
    /// Every kind, in the order the catalog loads them.
    pub const ALL: [Kind; 5] = [
        Kind::Job,
        Kind::Monitor,
        Kind::Skill,
        Kind::Plugin,
        Kind::Gate,
    ];

    /// The directory under a root that holds this kind's modules.
    pub fn dir(self) -> &'static str {
        match self {
            Kind::Job => "jobs",
            Kind::Monitor => "monitors",
            Kind::Skill => "skills",
            Kind::Plugin => "plugins",
            Kind::Gate => "gates",
        }
    }

    /// The schema a module of this kind amends.
    pub fn schema(self) -> &'static str {
        match self {
            Kind::Job => "nori:Job",
            Kind::Monitor => "nori:Monitor",
            Kind::Skill => "nori:Skill",
            Kind::Plugin => "nori:Plugin",
            Kind::Gate => "nori:Gate",
        }
    }
}

/// Which root a module came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Modules Nori ships, written under `$NORI_HOME/builtin/` by the harness (generated and
    /// overwritten on every start: copy one into `$NORI_HOME/<kind>/` to change it).
    Builtin,
    /// `$NORI_HOME`.
    User,
    /// `<repo>/.nori`.
    Project,
}

/// Where modules live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roots {
    /// `$NORI_HOME`.
    pub user: Option<PathBuf>,
    /// `<repo>/.nori`; only passed when the directory is trusted.
    pub project: Option<PathBuf>,
}

/// A module that loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded<T> {
    /// The module's `name`.
    pub name: String,
    /// The module's file.
    pub path: PathBuf,
    /// The root it came from.
    pub scope: Scope,
    /// What it evaluates to.
    pub value: T,
}

/// A module that did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The module's file (or the directory that could not be read).
    pub path: PathBuf,
    /// What is wrong; the evaluator's `file:line` diagnostics are kept.
    pub message: String,
}

/// Every module found under the roots, by kind, sorted by name, plus what was wrong.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    /// Jobs.
    pub jobs: Vec<Loaded<JobDef>>,
    /// Monitors.
    pub monitors: Vec<Loaded<MonitorDef>>,
    /// Skills (what the catalog reads of them).
    pub skills: Vec<Loaded<SkillMeta>>,
    /// Plugins.
    pub plugins: Vec<Loaded<PluginDef>>,
    /// Gates.
    pub gates: Vec<Loaded<GateDef>>,
    /// Modules that did not load, for the person to fix.
    pub problems: Vec<Problem>,
}

impl Catalog {
    /// The job named `name`.
    pub fn job(&self, name: &str) -> Option<&Loaded<JobDef>> {
        self.jobs.iter().find(|loaded| loaded.name == name)
    }

    /// The monitor named `name`.
    pub fn monitor(&self, name: &str) -> Option<&Loaded<MonitorDef>> {
        self.monitors.iter().find(|loaded| loaded.name == name)
    }

    /// The skill named `name`.
    pub fn skill(&self, name: &str) -> Option<&Loaded<SkillMeta>> {
        self.skills.iter().find(|loaded| loaded.name == name)
    }

    /// The plugin named `name`.
    pub fn plugin(&self, name: &str) -> Option<&Loaded<PluginDef>> {
        self.plugins.iter().find(|loaded| loaded.name == name)
    }

    /// The gate named `name`.
    pub fn gate(&self, name: &str) -> Option<&Loaded<GateDef>> {
        self.gates.iter().find(|loaded| loaded.name == name)
    }
}

/// Loads every module under the roots. Blocking: it evaluates Pkl and reads files, so call it
/// from `spawn_blocking` in async code (or use [`watch`]). The runtime schemas are added to
/// `options`; `nori:Skill` must come from the caller's options.
pub fn load(roots: &Roots, options: &EvaluatorOptions) -> Catalog {
    CatalogCache::default().reload(roots, options).0
}
