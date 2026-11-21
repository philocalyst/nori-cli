//! Scripted processes: what a world's machine "runs" when a job starts. A driver that supervises
//! jobs for a world (`nori-world`) plays them instead of spawning anything.

use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use serde::Serialize;

use super::value::Span;

/// Which stream a scripted line is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamDef {
    #[default]
    Stdout,
    Stderr,
}

/// A line a scripted process prints, `after` the previous one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineDef {
    #[serde(default)]
    pub after: Span,
    pub text: String,
    #[serde(default)]
    pub stream: StreamDef,
}

/// A command the world's machine runs for jobs.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDef {
    /// A regular expression over the job's command.
    pub command: String,
    #[serde(default)]
    pub output: Vec<LineDef>,
    /// The exit code, `linger` after the last line; `None` runs until stopped.
    #[serde(default = "exit_zero")]
    pub exit: Option<i64>,
    #[serde(default)]
    pub linger: Span,
    #[serde(skip)]
    compiled: OnceLock<Option<Regex>>,
}

fn exit_zero() -> Option<i64> {
    Some(0)
}

impl Clone for ProcessDef {
    fn clone(&self) -> Self {
        Self {
            command: self.command.clone(),
            output: self.output.clone(),
            exit: self.exit,
            linger: self.linger,
            compiled: OnceLock::new(),
        }
    }
}

impl PartialEq for ProcessDef {
    fn eq(&self, other: &Self) -> bool {
        self.command == other.command
            && self.output == other.output
            && self.exit == other.exit
            && self.linger == other.linger
    }
}

impl ProcessDef {
    /// Whether this process is what `command` runs.
    pub fn runs(&self, command: &str) -> bool {
        self.compiled
            .get_or_init(|| Regex::new(&self.command).ok())
            .as_ref()
            .is_some_and(|regex| regex.is_match(command))
    }

    pub fn pattern_error(&self) -> Option<String> {
        Regex::new(&self.command).err().map(|error| {
            format!(
                "process command {:?} is not a regular expression: {error}",
                self.command
            )
        })
    }
}
